//! Dict type inference with multi-pass binding and generalization.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use super::{infer_surface_expr, resolve_annotation, resolve_type_expr, TypeMap};
use crate::ast::{Span, Spanned, SurfaceDeclaration, SurfaceEntry, SurfaceExpression, SurfaceNode};
use crate::types::{
    generalize_with_doc, unify, InferState, Row, Substitution, Type, TypeAlias, TypeEnv, TypeError,
    TypeScheme,
};

/// Inject NominalVariant constructor function types into `dict_env` for ADT constructor scoping.
///
/// Given a resolved alias body type (e.g., `Union([NominalVariant("Circle", {r: Int}), ...])`),
/// registers each constructor as a callable function in `dict_env`. This makes constructors
/// available as typed functions both for type-checking call sites and for runtime injection.
///
/// Constructor type for variant with fields `{k1: T1, k2: T2, ...}`:
///   `Function { params: [(Some("k1"), T1), (Some("k2"), T2), ...], ret: NominalVariant{tag, fields}, variadic: false }`
///
/// Constructor type for unit variant (no fields):
///   `NominalVariant { tag, fields: {} }` (a value, not a function — constructed by bare reference)
fn inject_adt_constructor_schemes(alias_ty: &Type, dict_env: &mut TypeEnv) {
    match alias_ty {
        Type::NominalVariant { tag, fields } => {
            inject_single_constructor(tag, fields, dict_env);
        }
        Type::Union(members) => {
            for member in members {
                if let Type::NominalVariant { tag, fields } = member {
                    inject_single_constructor(tag, fields, dict_env);
                }
            }
        }
        // Non-ADT types (Records, primitives, etc.) — nothing to inject
        _ => {}
    }
}

/// Register a single NominalVariant constructor into `dict_env`.
///
/// For **unit constructors** (no fields), the constructor IS the value — it has type
/// `NominalVariant { tag, fields: {} }`. Example: `None` in `[type Option [Some a] None]`.
///
/// For **field constructors** (with fields), the constructor is a FUNCTION that takes
/// the fields as named arguments and returns a `NominalVariant`. Example: `Circle` in
/// `[type Shape [Circle r: Int]]` has type `Function { params: [("r", Int)], ret: NominalVariant("Circle", {r: Int}), variadic: false }`.
///
/// This allows the type checker to verify constructor call correctness: `[Circle r: 5]` ✓,
/// `[Circle r: "hello"]` ✗ (type error: expected Int, got String).
fn inject_single_constructor(tag: &str, fields: &Row, dict_env: &mut TypeEnv) {
    if fields.fields.is_empty() {
        // Unit constructor: no fields → the constructor is a value, not a function.
        // Type: NominalVariant { tag, fields: {} }
        dict_env.insert(
            tag.to_string(),
            Type::NominalVariant {
                tag: tag.to_string(),
                fields: Row {
                    fields: std::collections::HashMap::new(),
                },
            },
        );
    } else {
        // Field constructor: has fields → the constructor is a function.
        // Type: Function { params: [(field_name, field_type), ...], ret: NominalVariant }
        //
        // Build params list from fields. Field order is not semantically meaningful
        // (HashMap is unordered), but we sort by key for deterministic output.
        let mut field_vec: Vec<_> = fields.fields.iter().collect();
        field_vec.sort_by(|a, b| a.0.cmp(b.0));

        let params: Vec<(Option<String>, Type)> = field_vec
            .into_iter()
            .map(|(name, ty)| (Some(name.clone()), ty.clone()))
            .collect();

        let ret = Type::NominalVariant {
            tag: tag.to_string(),
            fields: fields.clone(),
        };

        dict_env.insert(
            tag.to_string(),
            Type::Function {
                params,
                ret: Box::new(ret),
                variadic: false,
            },
        );
    }
}

/// Strongly Connected Component - a group of mutually dependent bindings
pub(crate) struct Scc {
    /// Indices into the entries array
    pub(crate) indices: Vec<usize>,
}

/// Tarjan's algorithm for computing SCCs in topological order.
/// Returns SCCs in reverse topological order (dependencies before dependents).
///
/// Uses an iterative worklist implementation to avoid stack overflow on large
/// prelude dicts with many interdependent bindings. The recursive formulation
/// of Tarjan's algorithm would overflow on dicts with O(n) dependency chains;
/// the iterative version uses an explicit work stack instead of the call stack.
pub(crate) fn compute_sccs(
    entries: &[Spanned<SurfaceEntry>],
    key_entries: &[(Option<String>, bool)],
) -> Vec<Scc> {
    let n = entries.len();

    // Build name-to-index map for O(1) lookup
    let mut name_to_idx: HashMap<String, usize> = HashMap::new();
    for (i, (key_name, _)) in key_entries.iter().enumerate() {
        if let Some(ref kn) = key_name {
            name_to_idx.insert(kn.clone(), i);
        }
    }

    // Build adjacency list: for each entry, which other entries does it reference?
    let mut graph: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, entry) in entries.iter().enumerate() {
        let deps = collect_dependencies(&entry.node.value, &name_to_idx);
        graph[i] = deps;
    }

    // Tarjan's algorithm state (Cormen et al. 2009 §22.5 formulation)
    let mut index = 0usize;
    let mut tarjan_stack: Vec<usize> = Vec::new(); // Tarjan's S stack
    let mut disc: Vec<Option<usize>> = vec![None; n]; // discovery time
    let mut lowlinks: Vec<usize> = vec![0; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut sccs: Vec<Scc> = Vec::new();

    // Iterative Tarjan's SCC using an explicit work stack.
    //
    // Each work frame stores (node v, index into graph[v] — the next successor to process).
    // On push: initialize v and visit the first unvisited successor.
    // On resume: advance the successor index, propagate lowlink, check root condition.
    //
    // This exactly mirrors the recursive call structure without using the call stack.
    // Each "frame" is (v, next_successor_idx) where next_successor_idx is the 0-based
    // index of the next successor of v to process. When next_successor_idx == graph[v].len(),
    // all successors are done and we check the root condition.
    let mut call_stack: Vec<(usize, usize)> = Vec::new(); // (node, next_succ_idx)

    for start in 0..n {
        if disc[start].is_some() {
            continue;
        }

        // Initialize start node and push its frame
        disc[start] = Some(index);
        lowlinks[start] = index;
        index += 1;
        tarjan_stack.push(start);
        on_stack[start] = true;
        call_stack.push((start, 0));

        'outer: while let Some((v, succ_idx)) = call_stack.last().copied() {
            let succs = &graph[v];

            // Find the next successor to process starting from succ_idx
            let mut next_succ = succ_idx;
            while next_succ < succs.len() {
                let w = succs[next_succ];
                if disc[w].is_none() {
                    // Tree edge: initialize w and recurse into it.
                    // Update call_stack frame for v to resume at next_succ+1 after w returns.
                    call_stack.last_mut().unwrap().1 = next_succ + 1;
                    disc[w] = Some(index);
                    lowlinks[w] = index;
                    index += 1;
                    tarjan_stack.push(w);
                    on_stack[w] = true;
                    call_stack.push((w, 0));
                    continue 'outer;
                } else if on_stack[w] {
                    // Back edge: w is on the tarjan stack, update lowlink for v
                    lowlinks[v] = lowlinks[v].min(disc[w].unwrap());
                }
                // Already-visited and not on stack (cross/forward edge — skip): no lowlink update needed
                next_succ += 1;
            }

            // All successors of v are processed. Propagate lowlink to parent and check root.
            call_stack.pop();
            if let Some(&(parent, _)) = call_stack.last() {
                // Propagate lowlink: parent's lowlink = min(parent's lowlink, v's lowlink)
                lowlinks[parent] = lowlinks[parent].min(lowlinks[v]);
            }

            // Root check: if disc[v] == lowlinks[v], v is the root of an SCC
            if Some(lowlinks[v]) == disc[v] {
                let mut scc_indices = Vec::new();
                loop {
                    let x = tarjan_stack.pop().unwrap();
                    on_stack[x] = false;
                    scc_indices.push(x);
                    if x == v {
                        break;
                    }
                }
                sccs.push(Scc {
                    indices: scc_indices,
                });
            }
        }
    }

    // Tarjan's algorithm produces SCCs in reverse topological order of the condensation DAG:
    // dependency SCCs are emitted before the SCCs that depend on them. infer_dict processes
    // the returned list front-to-back, so dependencies are inferred (and generalized) before
    // the dependent SCCs that reference them — exactly the order we need.
    sccs
}

/// Collect all sibling variable references in an expression.
/// Returns the set of indices that this expression depends on.
///
/// Uses an iterative worklist to avoid stack overflow on deeply nested
/// Sequential/Pipe chains (which are unbounded by the parser's MAX_PARSE_DEPTH).
/// This mirrors the iterative Tarjan's algorithm above.
fn collect_dependencies(
    node: &Arc<SurfaceNode>,
    name_to_idx: &HashMap<String, usize>,
) -> Vec<usize> {
    let mut deps: Vec<usize> = Vec::new();
    let mut worklist: Vec<&Arc<SurfaceNode>> = vec![node];

    while let Some(current) = worklist.pop() {
        match &current.expr {
            SurfaceExpression::VarRef { name, .. } => {
                if let Some(&idx) = name_to_idx.get(name) {
                    deps.push(idx);
                }
            }
            SurfaceExpression::Int(_)
            | SurfaceExpression::Float(_)
            | SurfaceExpression::Str(_)
            | SurfaceExpression::Bool(_) => {}
            SurfaceExpression::Dict(entries) => {
                for entry in entries {
                    if let Some(ref key) = entry.node.key {
                        worklist.push(key);
                    }
                    worklist.push(&entry.node.value);
                }
            }
            SurfaceExpression::Fn { body, .. } => {
                worklist.push(body);
            }
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                worklist.push(func);
                for arg in args {
                    worklist.push(arg);
                }
                for named_arg in named_args {
                    worklist.push(&named_arg.node.value);
                }
            }
            SurfaceExpression::Match { scrutinee, arms } => {
                worklist.push(scrutinee);
                for arm in arms {
                    worklist.push(&arm.body);
                    if let Some(ref guard) = arm.guard {
                        worklist.push(guard);
                    }
                }
            }
            SurfaceExpression::DotAccess { expr, .. } => {
                worklist.push(expr);
            }
            SurfaceExpression::Pipe { lhs, rhs } => {
                worklist.push(lhs);
                worklist.push(rhs);
            }
            SurfaceExpression::Sequential(exprs) => {
                for e in exprs {
                    worklist.push(e);
                }
            }
            SurfaceExpression::Annotated { .. } => {
                // Annotated is a name with annotation, not an expr containing expr
                // No dependencies to collect
            }
            SurfaceExpression::TypeAssert { expr, .. } => {
                worklist.push(expr);
            }
            SurfaceExpression::Rest(_) => {}
            SurfaceExpression::Quote(e)
            | SurfaceExpression::Unquote(e)
            | SurfaceExpression::UnquoteSplice(e) => {
                worklist.push(e);
            }
            SurfaceExpression::TypeApp { func, arg } => {
                worklist.push(func);
                worklist.push(arg);
            }
            SurfaceExpression::Decl(_) => {
                // Type aliases, class/instance/macro declarations have no sibling variable
                // dependencies in the dict scope — they are fully processed in Pass 0c/Pass 2.
            }
            SurfaceExpression::PatternDecl { bindings }
            | SurfaceExpression::LetDecl { bindings } => {
                for b in bindings {
                    worklist.push(b);
                }
            }
            SurfaceExpression::CaseArm { pattern, body } => {
                worklist.push(pattern);
                worklist.push(body);
            }
            SurfaceExpression::Placeholder | SurfaceExpression::Error(_) => {}
        }
    }

    deps
}

pub(crate) fn infer_dict(
    entries: &[Spanned<SurfaceEntry>],
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
    span: Span,
) -> (Type, HashMap<String, TypeScheme>, Vec<TypeError>) {
    // Level management: save enclosing level, increment for dict body
    let enclosing_level = state.level;
    state.level += 1;

    let mut dict_env = TypeEnv::with_parent(env);
    let mut key_entries: Vec<(Option<String>, bool)> = Vec::new();
    let mut auto_index: i64 = 0;

    // Pass 0: Key resolution
    for entry in entries {
        let key_name = entry_key_name(&entry.node, &mut auto_index, env, state, type_map);
        let is_alias = matches!(
            &entry.node.value.expr,
            SurfaceExpression::Decl(d) if matches!(d.as_ref(), SurfaceDeclaration::TypeAlias { .. })
        );
        key_entries.push((key_name, is_alias));
    }

    // Pass 0a: Compute SCCs for binding group analysis
    let sccs = compute_sccs(entries, &key_entries);

    // Pass 2: Register type aliases (before SCC processing)
    for ((key_name, is_alias), entry) in key_entries.iter().zip(entries.iter()) {
        if *is_alias {
            if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                if let SurfaceDeclaration::TypeAlias { params, body } = decl.as_ref() {
                    let mut alias_ann_map: HashMap<String, String> = HashMap::new();
                    for p in params {
                        let fresh = format!("_t{}", state.name_counter);
                        state.name_counter = state.name_counter.saturating_add(1);
                        state.levels.insert(fresh.clone(), state.level);
                        alias_ann_map.insert(p.clone(), fresh.clone());
                    }
                    if let Ok(alias_ty) = resolve_type_expr(
                        body,
                        &Rc::new(dict_env.clone()),
                        state,
                        &mut Some(&mut alias_ann_map),
                        &mut None,
                    ) {
                        // Register the named alias (keyed entries only)
                        if let Some(name) = key_name {
                            let remapped_params: Vec<String> = params
                                .iter()
                                .map(|p| alias_ann_map.get(p).cloned().unwrap())
                                .collect();
                            dict_env.insert_type_alias(
                                name.clone(),
                                TypeAlias {
                                    params: remapped_params,
                                    body: alias_ty.clone(),
                                },
                            );
                        }

                        // ADT constructor scoping: inject each NominalVariant constructor from the
                        // alias body as a callable function type in dict_env.
                        // This handles both keyed aliases (`Result: [type [Ok a] [Err b]]`) and
                        // positional aliases (`[type Shape [Circle r: Int] [Square s: Int]]`).
                        //
                        // Constructor type: for fields {k1: T1, k2: T2, ...} → Fn@Ret [k1: T1 k2: T2 ...]
                        // where Ret = NominalVariant{tag, fields}.
                        inject_adt_constructor_schemes(&alias_ty, &mut dict_env);
                    }
                }
            }
        }
    }

    // Initialize global substitution and field types accumulator.
    // Start with empty local substitution and incrementally merge state.subst entries per SCC.
    // Eliminates O(n) upfront clone of state.subst.type_map (cycle-31 major item).
    let mut subst = Substitution {
        type_map: std::cell::RefCell::new(HashMap::new()),
    };
    let mut field_types: HashMap<String, Type> = HashMap::new();
    let mut errors = Vec::new();

    // state.subst entries are fully re-merged into local subst after each SCC iteration.

    // Track inner_schemes for nested dict values (DOT-POLY support)
    let mut entry_inner_schemes: HashMap<String, HashMap<String, TypeScheme>> = HashMap::new();

    // Track constraints generated during each entry's inference (constraint-preservation fix).
    // Constraints from fn@[constraint: ...] annotations should be scoped to the function being
    // inferred, not leak across dict entries.
    let mut entry_constraints: HashMap<String, Vec<crate::types::Constraint>> = HashMap::new();

    // Pass 0c: pre-register class/instance declarations so all classes and instances
    // are visible during body type-checking, regardless of declaration order in the file.
    // Modeled on Pass 2 (type alias pre-registration). (Wadler & Blott 1989 — class/instance
    // declarations are globally visible within their scope.)
    let dict_env_rc = Rc::new(dict_env.clone());
    for (idx, entry) in entries.iter().enumerate() {
        let is_class_or_instance = matches!(
            &entry.node.value.expr,
            SurfaceExpression::Decl(d)
                if matches!(
                    d.as_ref(),
                    SurfaceDeclaration::ClassDecl { .. } | SurfaceDeclaration::InstanceDecl { .. }
                )
        );
        if is_class_or_instance {
            // Infer class/instance expression (registers into state)
            match infer_surface_expr(&entry.node.value, &dict_env_rc, state, type_map) {
                Ok(ty) => {
                    let (ref key_name, _) = key_entries[idx];
                    if let Some(name) = key_name {
                        field_types.insert(name.clone(), ty);
                    }
                }
                Err(mut errs) => {
                    errors.append(&mut errs);
                    let (ref key_name, _) = key_entries[idx];
                    if let Some(name) = key_name {
                        // Use Type::Error (not Type::Unknown) for failed bindings so that:
                        // - unify(Error, T) succeeds silently (prevents cascade errors)
                        // - is_subtype(Error, _) = false (TypeAsserts correctly reject)
                        // - T010 "inferred Unknown" diagnostic doesn't fire (Error ≠ Unknown)
                        // - lower.rs emits TypeAssert with Type::Unknown (accepts all values)
                        field_types.insert(name.clone(), Type::Error);
                        state
                            .failed_bindings
                            .insert(name.clone(), entry.span.clone());
                    }
                }
            }
        }
    }

    // Process each SCC in topological order
    // Tarjan's algorithm produces SCCs in reverse topological order, so we process them as-is
    for scc in sccs.into_iter() {
        // Pass 1_i: Bind this SCC's entries to fresh TypeVars at level state.level
        // Optimize common singleton case with Option instead of HashMap allocation
        enum FreshVars {
            Singleton(String, Type),
            Multiple(HashMap<String, Type>),
        }
        let mut fresh_vars_storage = None;

        for &idx in &scc.indices {
            let (ref key_name, is_alias) = key_entries[idx];
            if !is_alias {
                if let Some(ref name) = key_name {
                    let fresh_var = state.fresh_type_var();
                    match &mut fresh_vars_storage {
                        None => {
                            fresh_vars_storage =
                                Some(FreshVars::Singleton(name.clone(), fresh_var.clone()));
                        }
                        Some(FreshVars::Singleton(first_name, first_var)) => {
                            let mut map = HashMap::new();
                            map.insert(first_name.clone(), first_var.clone());
                            map.insert(name.clone(), fresh_var.clone());
                            fresh_vars_storage = Some(FreshVars::Multiple(map));
                        }
                        Some(FreshVars::Multiple(map)) => {
                            map.insert(name.clone(), fresh_var.clone());
                        }
                    }
                    dict_env.insert_scheme(name.clone(), TypeScheme::mono(fresh_var));
                }
            }
        }

        // Wrap dict_env for use in infer_expr calls for this SCC
        let dict_env_rc = Rc::new(dict_env.clone());

        // Pass 3_i: Infer values and unify with bound type vars for this SCC
        for &idx in &scc.indices {
            let entry = &entries[idx];
            let (ref key_name, is_alias) = key_entries[idx];

            // Skip type aliases (already processed in Pass 2), Rest markers,
            // and class/instance declarations (already processed in Pass 0c)
            let skip = is_alias
                || matches!(&entry.node.value.expr, SurfaceExpression::Rest(_))
                || matches!(
                    &entry.node.value.expr,
                    SurfaceExpression::Decl(d)
                        if matches!(
                            d.as_ref(),
                            SurfaceDeclaration::ClassDecl { .. }
                                | SurfaceDeclaration::InstanceDecl { .. }
                        )
                );
            if skip {
                continue;
            }

            if let Some(name) = key_name {
                // Set current_function for polymorphic recursion detection.
                // Only set it for functions WITHOUT return annotations — functions with
                // annotations can recurse safely because the return type is pinned.
                let should_check_recursion =
                    if let SurfaceExpression::Fn { return_ann, .. } = &entry.node.value.expr {
                        return_ann.is_none()
                    } else {
                        false
                    };

                if should_check_recursion {
                    state.current_function = Some(name.clone());
                }

                // Save constraints before inferring this entry's value.
                // Function constraints (from fn@[constraint: ...] annotations) should be scoped
                // to the function being inferred, not leak across dict entries.
                let saved_constraints = std::mem::take(&mut state.constraints);

                // If the value is wrapped in TypeAssert (e.g., `x: [@T expr]`), extract the
                // asserted type upfront. When inference of the inner expression fails, the
                // asserted type is used as the public field type instead of Type::Error.
                // This preserves the declared interface type T even when the body has errors,
                // so callers see T rather than Error (which defeats the annotation's purpose).
                //
                // We resolve the annotation in a fresh mapping scope — the same approach used
                // by `resolve_type_assert` — to avoid leaking TypeVars into the outer scope.
                let type_assert_ty: Option<Type> = if let SurfaceExpression::TypeAssert {
                    annotation,
                    ..
                } = &entry.node.value.expr
                {
                    let mut ann_mapping: Option<std::collections::HashMap<String, String>> =
                        Some(std::collections::HashMap::new());
                    let mut ann_mapping_opt = ann_mapping.as_mut();
                    let mut row_ann_mapping_opt: Option<
                        &mut std::collections::HashMap<String, String>,
                    > = None;
                    resolve_annotation(
                        &annotation.node,
                        &dict_env_rc,
                        annotation.span.clone(),
                        state,
                        &mut ann_mapping_opt,
                        &mut row_ann_mapping_opt,
                    )
                    .ok()
                } else {
                    None
                };

                // Special case: if the value is a Dict, call infer_dict directly to capture schemes
                let (value_ty, nested_schemes_opt) =
                    if let SurfaceExpression::Dict(nested_entries) = &entry.node.value.expr {
                        let (ty, schemes, mut nested_errs) = infer_dict(
                            nested_entries,
                            &dict_env_rc,
                            state,
                            type_map,
                            entry.node.value.span.clone(),
                        );
                        errors.append(&mut nested_errs);
                        (Ok(ty), Some(schemes))
                    } else {
                        (
                            infer_surface_expr(&entry.node.value, &dict_env_rc, state, type_map),
                            None,
                        )
                    };

                // Constraints generated during this entry's inference are now in state.constraints.
                // We'll process them during generalization (Pass 4), then discard them.
                // Restore the saved constraints so parent scope constraints are preserved.
                let this_entry_constraints =
                    std::mem::replace(&mut state.constraints, saved_constraints);

                // Store this entry's constraints for use during generalization
                if !this_entry_constraints.is_empty() {
                    entry_constraints.insert(name.clone(), this_entry_constraints);
                }

                if should_check_recursion {
                    state.current_function = None;
                }

                // Store nested schemes if present
                if let Some(nested_schemes) = nested_schemes_opt {
                    entry_inner_schemes.insert(name.clone(), nested_schemes);
                }

                match value_ty {
                    Ok(value_ty) => {
                        // Get the bound TypeVar from Pass 1_i
                        let bound_var_opt = match &fresh_vars_storage {
                            Some(FreshVars::Singleton(n, ty)) if n == name.as_str() => Some(ty),
                            Some(FreshVars::Multiple(map)) => map.get(name.as_str()),
                            _ => None,
                        };

                        if let Some(bound_var) = bound_var_opt {
                            // Unify the inferred type with the bound var
                            if let Err(e) = unify(
                                bound_var,
                                &value_ty,
                                &mut subst,
                                state,
                                entry.node.value.span.clone(),
                            ) {
                                errors.push(e);
                                field_types.insert(name.clone(), Type::Error);
                                state
                                    .failed_bindings
                                    .insert(name.clone(), entry.span.clone());
                            } else {
                                field_types.insert(name.clone(), value_ty);
                            }
                        } else {
                            field_types.insert(name.clone(), value_ty);
                        }
                    }
                    Err(mut errs) => {
                        errors.append(&mut errs);
                        // If the entry was wrapped in TypeAssert ([@T expr]), use the asserted
                        // type T as the public field type even when the body has errors. This
                        // ensures callers see the declared type T rather than Error, preserving
                        // the purpose of the annotation as a type-interface boundary.
                        // Fall back to Error only when no assertion type is available.
                        let fallback_ty = type_assert_ty.unwrap_or(Type::Error);
                        field_types.insert(name.clone(), fallback_ty.clone());
                        state
                            .failed_bindings
                            .insert(name.clone(), entry.span.clone());
                        // Populate type_map for LSP hover on failed dict value expressions.
                        // Use the asserted type if available so hover shows T instead of Unknown.
                        if let Some(ref mut map) = type_map {
                            let key = (
                                entry.node.value.span.start.offset,
                                entry.node.value.span.end.offset,
                            );
                            map.insert(key, fallback_ty);
                        }
                    }
                }
            }
        }

        // Merge state.subst into local subst after each SCC (full re-merge every iteration)
        {
            // Every entry in state.subst is re-merged into the local subst on each SCC iteration.
            // For each entry (k, v) from state.subst:
            //   - v is first resolved through the local subst (applied_v = subst.apply(v)).
            //   - If k is already bound in the local subst, the two bindings are unified
            //     (removing k first to avoid self-unification, then re-inserting the winner).
            //     On unification failure the original local binding is restored and an error
            //     is recorded, but inference continues.
            //   - If k is not yet bound in the local subst, it is inserted directly.
            // This handles the case where state.subst accumulates new bindings produced by
            // recursive calls during SCC inference, and ensures the local subst stays
            // consistent with the global state after each SCC is processed.
            let state_type_entries: Vec<(String, Type)> = {
                let state_map = state.subst.type_map.borrow();
                state_map
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };

            for (k, v) in state_type_entries {
                let applied_v = subst.apply(&v);
                let existing_opt = subst.type_map.borrow().get(&k).cloned();
                match existing_opt {
                    Some(existing) => {
                        subst.type_map.borrow_mut().remove(&k);
                        if let Err(e) =
                            unify(&existing, &applied_v, &mut subst, state, span.clone())
                        {
                            errors.push(e);
                            subst.type_map.borrow_mut().insert(k, existing);
                            continue;
                        }
                        let resolved = subst.apply(&applied_v);
                        subst.type_map.borrow_mut().insert(k, resolved);
                    }
                    None => {
                        subst.type_map.borrow_mut().insert(k, applied_v);
                    }
                }
            }
        }

        // Process deferred equality constraints after this SCC's substitution merge.
        // This attempts to resolve TypeStageApp and Union-vs-Union constraints that may
        // have become ground after unification in this SCC. See doc/06-type-inference.md:884.
        if !state.deferred_equalities.is_empty() {
            crate::types::process_deferred_equalities(state, &mut subst, span.clone());
        }

        // Apply substitution to this SCC's field types
        for &idx in &scc.indices {
            let (ref key_name, _) = key_entries[idx];
            if let Some(name) = key_name {
                if let Some(ty) = field_types.get(name) {
                    let resolved_ty = subst.apply(ty);
                    field_types.insert(name.clone(), resolved_ty);
                }
            }
        }

        // Merge local subst into state.subst BEFORE generalization.
        // This ensures generalize_with_doc's subst_snapshot (type_env.rs:561) includes
        // bindings created during this SCC's inference, fixing is_discharged for FD constraints.
        // Previously this merge happened after all SCCs (line 807), causing is_discharged to
        // return false for TypeVars bound in earlier SCCs, triggering spurious T013 warnings.
        for (k, v) in subst.type_map.borrow().iter() {
            state
                .subst
                .type_map
                .borrow_mut()
                .insert(k.clone(), v.clone());
        }
        if let Err(e) = state.subst.check_size(span.clone()) {
            errors.push(e);
        }

        // Pass 4_i: Generalize this SCC's entries before processing the next SCC
        for &idx in &scc.indices {
            let entry = &entries[idx];
            let (ref key_name, _) = key_entries[idx];

            if let Some(name) = key_name {
                if let Some(ty) = field_types.get(name) {
                    // Extract doc string from key annotation (e.g., name@[doc: "..."])
                    let key_doc = if let Some(ref key_node) = entry.node.key {
                        match &key_node.expr {
                            SurfaceExpression::Annotated { annotation, .. } => {
                                annotation.node.get_property("doc").and_then(|doc_node| {
                                    if let SurfaceExpression::Str(doc_string) = &doc_node.expr {
                                        Some(doc_string.clone())
                                    } else {
                                        None
                                    }
                                })
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };

                    // Extract doc string from value annotation (e.g., [fn@[doc: "..."] ...])
                    let value_doc = match &entry.node.value.expr {
                        SurfaceExpression::Fn { return_ann, .. } => {
                            return_ann.as_ref().and_then(|ann| {
                                ann.node.get_property("doc").and_then(|doc_node| {
                                    if let SurfaceExpression::Str(doc_string) = &doc_node.expr {
                                        Some(doc_string.clone())
                                    } else {
                                        None
                                    }
                                })
                            })
                        }
                        _ => None,
                    };

                    // Value doc takes precedence over key doc
                    let doc = value_doc.or(key_doc);

                    // Skip constraint restore for entries that failed type inference.
                    // Constraints accumulated before the failure (e.g., by resolve_fn_metadata)
                    // were never discharged by unification, so restoring them would cause
                    // generalize_with_doc to emit spurious T013 warnings alongside the real
                    // type error already recorded in state.diagnostics.
                    if state.failed_bindings.contains_key(name) {
                        let mut scheme = generalize_with_doc(
                            enclosing_level,
                            ty,
                            state,
                            doc,
                            entry.span.clone(),
                        );
                        if let Some(inner) = entry_inner_schemes.get(name) {
                            scheme.inner_schemes = Some(inner.clone());
                        }
                        dict_env.insert_scheme(name.clone(), scheme);
                        continue;
                    }

                    // Restore this entry's constraints before generalization.
                    // generalize_with_doc will check which constraints apply to the generalized vars.
                    let saved_constraints = std::mem::replace(
                        &mut state.constraints,
                        entry_constraints.get(name).cloned().unwrap_or_default(),
                    );

                    let mut scheme =
                        generalize_with_doc(enclosing_level, ty, state, doc, entry.span.clone());

                    // Restore parent scope constraints after generalization
                    state.constraints = saved_constraints;

                    // Attach inner_schemes if this entry's value was a dict literal
                    if let Some(inner) = entry_inner_schemes.get(name) {
                        scheme.inner_schemes = Some(inner.clone());
                    }

                    // Update dict_env with the generalized scheme for subsequent SCCs
                    dict_env.insert_scheme(name.clone(), scheme);
                }
            }
        }
    }

    // Build final schemes map from dict_env
    let mut schemes = HashMap::with_capacity(field_types.len());
    for (key_name, _is_alias) in &key_entries {
        if let Some(name) = key_name {
            if let Some(scheme) = dict_env.get(name) {
                schemes.insert(name.clone(), scheme.clone());
            }
        }
    }

    // Restore enclosing level
    state.level = enclosing_level;

    // Compact the levels map: remove entries for TypeVars that have been unified.
    // This prevents unbounded growth during long inference sessions (e.g., prelude loading).
    state.compact_levels();

    let record_type = Type::Record(Row {
        fields: field_types,
    });

    // Always return best-effort results along with any errors.
    // The schemes collected in dict_env are correct for entries that succeeded; failed
    // entries have Type::Error (or the TypeAssert fallback) in record_type and are
    // marked in state.failed_bindings. Callers propagate errors via the third element.
    (record_type, schemes, errors)
}

pub(crate) fn entry_key_name(
    entry: &SurfaceEntry,
    auto_index: &mut i64,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Option<String> {
    match &entry.key {
        Some(key_node) => match &key_node.expr {
            SurfaceExpression::Str(s) => Some(s.clone()),
            SurfaceExpression::Int(n) => Some(n.to_string()),
            // Annotated key: name@[doc: "..."] — extract name directly
            SurfaceExpression::Annotated { name, .. } => Some(name.clone()),
            _ => match infer_surface_expr(key_node, env, state, type_map) {
                Ok(Type::StringLiteral(s)) => Some(s),
                Ok(Type::IntLiteral(n)) => Some(n.to_string()),
                _ => None,
            },
        },
        None => {
            let name = auto_index.to_string();
            *auto_index += 1;
            Some(name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::sp;

    /// Helper: build a zero-origin [`SurfaceNode`] from a [`SurfaceExpression`].
    fn sn(expr: SurfaceExpression) -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode {
            expr,
            span: Span::origin(),
        })
    }

    /// Helper: build a `Spanned<SurfaceEntry>` whose value is a `VarRef` to `ref_name`.
    /// Used to encode a dependency edge: this entry's value references `ref_name`.
    fn entry_ref(ref_name: &str) -> Spanned<SurfaceEntry> {
        sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::VarRef {
                name: ref_name.to_string(),
                escaped: false,
            }),
        })
    }

    /// Helper: build a `Spanned<SurfaceEntry>` whose value is an integer literal (no deps).
    fn entry_lit() -> Spanned<SurfaceEntry> {
        sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::Int(0)),
        })
    }

    /// Helper: build a key_entries list of named, non-alias entries.
    fn key_entries_for(names: &[&str]) -> Vec<(Option<String>, bool)> {
        names.iter().map(|n| (Some(n.to_string()), false)).collect()
    }

    /// Collect the SCC groups as sorted index sets so tests are order-independent within
    /// a group (Tarjan's exact member ordering is implementation-defined).
    fn scc_index_sets(sccs: &[Scc]) -> Vec<Vec<usize>> {
        let mut result: Vec<Vec<usize>> = sccs
            .iter()
            .map(|scc| {
                let mut v = scc.indices.clone();
                v.sort_unstable();
                v
            })
            .collect();
        result.sort();
        result
    }

    // --- compute_sccs unit tests ---

    /// Empty entries: no SCCs produced.
    #[test]
    fn test_scc_empty_entries() {
        let entries: Vec<Spanned<SurfaceEntry>> = vec![];
        let key_entries: Vec<(Option<String>, bool)> = vec![];
        let sccs = compute_sccs(&entries, &key_entries);
        assert!(sccs.is_empty(), "expected no SCCs for empty input");
    }

    /// Linear chain A→B (A references B): two singletons with B processed before A.
    /// A depends on B, so B's SCC appears first in Tarjan's output (dependencies first).
    #[test]
    fn test_scc_linear_chain() {
        // entries[0] = A (references B at index 1)
        // entries[1] = B (no deps)
        let b_entry = entry_lit();
        let a_entry = sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::VarRef {
                name: "b".to_string(),
                escaped: false,
            }),
        });
        let entries = vec![a_entry, b_entry];
        let key_entries = key_entries_for(&["a", "b"]);
        let sccs = compute_sccs(&entries, &key_entries);

        // Two singleton SCCs
        assert_eq!(sccs.len(), 2, "expected 2 singleton SCCs for a→b chain");
        let sets = scc_index_sets(&sccs);
        assert!(
            sets.contains(&vec![0]),
            "expected SCC containing index 0 (a)"
        );
        assert!(
            sets.contains(&vec![1]),
            "expected SCC containing index 1 (b)"
        );
    }

    /// Two-node mutual cycle A↔B: both should be in the same SCC.
    #[test]
    fn test_scc_two_node_cycle() {
        // entries[0] = A (references B at index 1)
        // entries[1] = B (references A at index 0)
        let a_entry = sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::VarRef {
                name: "b".to_string(),
                escaped: false,
            }),
        });
        let b_entry = sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::VarRef {
                name: "a".to_string(),
                escaped: false,
            }),
        });
        let entries = vec![a_entry, b_entry];
        let key_entries = key_entries_for(&["a", "b"]);
        let sccs = compute_sccs(&entries, &key_entries);

        assert_eq!(sccs.len(), 1, "expected 1 SCC for mutual cycle a↔b");
        let sets = scc_index_sets(&sccs);
        assert_eq!(sets[0], vec![0, 1], "both nodes must be in the same SCC");
    }

    /// Diamond DAG: A→B, A→C, B→D, C→D.
    /// D has no deps, B and C each depend only on D, A depends on B and C.
    /// Expected: four singletons in dependency-first order (D, then B and C in some order, then A).
    #[test]
    fn test_scc_diamond_dag() {
        // indices: A=0, B=1, C=2, D=3
        let a_entry = sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::Dict(vec![
                sp(SurfaceEntry {
                    key: None,
                    value: sn(SurfaceExpression::VarRef {
                        name: "b".to_string(),
                        escaped: false,
                    }),
                }),
                sp(SurfaceEntry {
                    key: None,
                    value: sn(SurfaceExpression::VarRef {
                        name: "c".to_string(),
                        escaped: false,
                    }),
                }),
            ])),
        });
        let b_entry = entry_ref("d");
        let c_entry = entry_ref("d");
        let d_entry = entry_lit();

        let entries = vec![a_entry, b_entry, c_entry, d_entry];
        let key_entries = key_entries_for(&["a", "b", "c", "d"]);
        let sccs = compute_sccs(&entries, &key_entries);

        // Four singleton SCCs (no cycles in a DAG)
        assert_eq!(sccs.len(), 4, "expected 4 singleton SCCs for diamond DAG");

        let sets = scc_index_sets(&sccs);
        // Every node appears exactly once
        assert!(sets.contains(&vec![0]), "a (index 0) must appear");
        assert!(sets.contains(&vec![1]), "b (index 1) must appear");
        assert!(sets.contains(&vec![2]), "c (index 2) must appear");
        assert!(sets.contains(&vec![3]), "d (index 3) must appear");

        // Dependency ordering: d must appear before b and c; b and c must appear before a.
        // Tarjan returns SCCs in reverse topological order (dependencies first).
        // Build output-position map: original_index → position in sccs output
        let mut output_pos = [0usize; 4];
        for (scc_pos, scc) in sccs.iter().enumerate() {
            for &idx in &scc.indices {
                output_pos[idx] = scc_pos;
            }
        }
        // d (3) must come before b (1) and c (2)
        assert!(
            output_pos[3] < output_pos[1],
            "d must be processed before b"
        );
        assert!(
            output_pos[3] < output_pos[2],
            "d must be processed before c"
        );
        // b (1) and c (2) must come before a (0)
        assert!(
            output_pos[1] < output_pos[0],
            "b must be processed before a"
        );
        assert!(
            output_pos[2] < output_pos[0],
            "c must be processed before a"
        );
    }
}
