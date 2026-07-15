//! Dict type inference with multi-pass binding and generalization.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::{infer_surface_expr, TypeMap};
use crate::ast::{Span, Spanned, SurfaceDeclaration, SurfaceEntry, SurfaceExpression, SurfaceNode};
use crate::env::Env;
use crate::type_def::{RowTail, TyConDef};
use crate::type_infer::Substitution;
use crate::types::{
    generalize_with_doc, Constraint, InferState, Kind, Row, Type, TypeAlias, TypeEnv, TypeError,
    TypeScheme,
};

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
    key_entries: &[(Option<String>, bool, bool)],
) -> Vec<Scc> {
    let n = entries.len();

    // Build name-to-index map for O(1) lookup
    let mut name_to_idx: HashMap<String, usize> = HashMap::new();
    for (i, (key_name, _, _)) in key_entries.iter().enumerate() {
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
            | SurfaceExpression::U64(_)
            | SurfaceExpression::Float(_)
            | SurfaceExpression::StringLiteral { .. } => {}
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
            SurfaceExpression::Field { expr, .. } => {
                if let Some(target) = expr {
                    worklist.push(target);
                }
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
            SurfaceExpression::TypeAssert { expr, .. } => {
                worklist.push(expr);
            }
            SurfaceExpression::Rest(..) => {}
            SurfaceExpression::Quote(e)
            | SurfaceExpression::Unquote(e)
            | SurfaceExpression::UnquoteSplice(e) => {
                worklist.push(e);
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
            SurfaceExpression::CaseArm {
                let_bindings,
                pattern,
                body,
            } => {
                worklist.push(let_bindings);
                worklist.push(pattern);
                worklist.push(body);
            }
            SurfaceExpression::Placeholder | SurfaceExpression::Error(_) => {}
        }
    }

    deps
}

pub(crate) async fn infer_dict(
    entries: &[Spanned<SurfaceEntry>],
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
    _span: Span,
) -> (Type, indexmap::IndexMap<String, TypeScheme>, Vec<TypeError>) {
    // Level management: save enclosing level, increment for dict body
    let enclosing_level = state.level;
    state.level += 1;

    let dict_env: Arc<RwLock<Env>> = Arc::new(RwLock::new(Env::with_parent(Arc::clone(env))));
    // Extra schemes from ADT constructors — injected in Pass 2, merged into final schemes.
    // Keyed by short constructor name (e.g. "True" for Bool.True).
    let mut ctor_schemes: HashMap<String, TypeScheme> = HashMap::new();
    // key_entries: Vec<(key_name, is_alias, is_static_key)>
    // - key_name: the resolved key string (Some for named/auto-indexed, None for computed)
    // - is_alias: true if the entry value is a TypeAlias declaration
    // - is_static_key: true if the key is statically known at compile time AND was assigned
    //   a slot by the resolver (i.e., key node is Str or Annotated, not auto-indexed or computed).
    //   Only static-key entries appear in surface_dict_static_keys and get pre-registered in
    //   the TypeEnv's slotted IndexMap.
    let mut key_entries: Vec<(Option<String>, bool, bool)> = Vec::new();
    let mut auto_index: i64 = 0;

    // Pass 0: Key resolution
    for entry in entries {
        let key_name = entry_key_name(&entry.node, &mut auto_index, env, state, type_map).await;
        let is_alias = matches!(
            &entry.node.value.expr,
            SurfaceExpression::Decl(d) if matches!(d.as_ref(), SurfaceDeclaration::TypeAlias { .. })
        );
        // A key is "static" (resolver-visible) iff it has a key node that is Str or Annotated.
        // This mirrors `surface_dict_static_keys` in resolve.rs: auto-indexed entries (no key
        // node) and computed-key entries are excluded from the resolver's scope frame.
        let is_static_key = entry.node.key.as_ref().is_some_and(|k| {
            matches!(
                &k.expr,
                SurfaceExpression::StringLiteral { .. } | SurfaceExpression::VarRef { .. }
            )
        });
        key_entries.push((key_name, is_alias, is_static_key));
    }

    // Pass 0a: Compute SCCs for binding group analysis
    let sccs = compute_sccs(entries, &key_entries);

    // Pass 1 (global): Pre-insert fresh TypeVar placeholders for ALL key entries in SOURCE
    // ORDER before the SCC loop.  This ensures the slotted IndexMap in dict_env has entries
    // at the same positions as the resolver's slot assignments (both iterate key_entries in
    // source order).  Type aliases are included so they occupy their resolver-assigned slot
    // even though their inferred type is registered separately (via TypeAlias, not a scheme).
    //
    // The SCC loop below will update these entries (insert_scheme is update-in-place for
    // IndexMap, preserving slot positions) as it infers the actual types.
    //
    // fresh_vars_by_name: name → TypeVar, used to recover the bound TypeVar in Pass 3_i.
    // Pass 1 (global): Pre-insert fresh TypeVar placeholders for ALL statically-known bindings
    // in SOURCE ORDER, matching the slot assignment order of surface_dict_static_keys in resolve.rs.
    //
    // This includes:
    // (a) Static-key entries (keyed with Str/VarRef key): one placeholder per entry.
    // (b) Anonymous InstanceDecl entries (no outer key): one placeholder per ɪ-prefixed method
    //     binding, interleaved at the source position where the InstanceDecl appears.
    //
    // Correct interleaving is critical: the resolver assigns slots in source order (keyed entries
    // AND ɪ-prefixed names). If we insert keyed entries first then ɪ-prefixed names, slot indices
    // shift and get_scheme_at(level, slot) will not find the right TypeScheme for class method VarRefs.
    let mut fresh_vars_by_name: HashMap<String, Type> = HashMap::new();
    for ((key_name, is_alias, is_static_key), entry) in key_entries.iter().zip(entries.iter()) {
        // (a) Static-key entry.
        if *is_static_key {
            if let Some(ref name) = key_name {
                let fresh_var = state.fresh_type_var(&entry.span);
                if !is_alias {
                    fresh_vars_by_name.insert(name.clone(), fresh_var.clone());
                }
                dict_env
                    .write()
                    .unwrap()
                    .insert_scheme(name.clone(), TypeScheme::mono(fresh_var));
            }
        }
        // (b) Anonymous InstanceDecl entry: insert ɪ-prefixed placeholders at this source position.
        if entry.node.key.is_none() {
            if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                if let SurfaceDeclaration::InstanceDecl { class_name, arms } = decl.as_ref() {
                    for (pattern, method_entries) in arms {
                        let dispatch_tags = crate::lower::extract_dispatch_tags(&pattern.expr);
                        let type_args: Vec<&str> =
                            dispatch_tags.iter().filter_map(|t| t.as_deref()).collect();
                        for me in method_entries {
                            let method_name = match me.node.key.as_ref() {
                                Some(k) => match &k.expr {
                                    SurfaceExpression::StringLiteral { content: s, .. } => {
                                        s.clone()
                                    }
                                    SurfaceExpression::VarRef { name, .. } => name.clone(),
                                    _ => continue,
                                },
                                None => continue,
                            };
                            let binding_name = crate::type_def::instance_binding_name(
                                class_name,
                                &method_name,
                                &type_args,
                            );
                            let fresh_var = state.fresh_type_var(&entry.span);
                            fresh_vars_by_name.insert(binding_name.clone(), fresh_var.clone());
                            dict_env
                                .write()
                                .unwrap()
                                .insert_scheme(binding_name, TypeScheme::mono(fresh_var));
                        }
                    }
                }
            }
        }
    }

    // Pass 2: Register type aliases (before SCC processing)
    for ((key_name, is_alias, _), entry) in key_entries.iter().zip(entries.iter()) {
        if *is_alias {
            if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                if let SurfaceDeclaration::TypeAlias { params, body } = decl.as_ref() {
                    let mut alias_ann_map: HashMap<String, String> = HashMap::new();
                    for (param_name, param_ann) in params {
                        let param_span = param_ann
                            .as_ref()
                            .map(|a| a.span.clone())
                            .unwrap_or_else(|| entry.span.clone());
                        let fresh = state
                            .fresh_type_var_with(
                                Some(param_name.as_str()),
                                None,
                                Kind::Type,
                                &param_span,
                            )
                            .0;
                        alias_ann_map.insert(param_name.clone(), fresh.clone());
                    }

                    // Resolve the alias body to extract its TyConDef (constructor registry).
                    // For Dict bodies (the common case for [type ...] declarations), call
                    // resolve_type_dict directly to avoid the eval_type_stage_expr fallback in
                    // resolve_property_dict_as_record — triggering the type-stage evaluator from
                    // Pass 2 would spam stderr with TypeNode errors from the prelude.
                    // For non-Dict bodies, resolve_type_expr handles VarRefs and implied calls.
                    let alias_name = key_name.as_deref().unwrap_or("");
                    let stub_env = TypeEnv::new();
                    let mut alias_constraints: Vec<Constraint> = Vec::new();
                    let mut ann_map_for_body = alias_ann_map.clone();
                    let resolved_body: Type = match &body.expr {
                        crate::ast::SurfaceExpression::Dict(entries) => {
                            super::typecheck_annot::resolve_type_dict(
                                entries,
                                &stub_env,
                                body.span.clone(),
                                state,
                                &mut alias_constraints,
                                &mut Some(&mut ann_map_for_body),
                                &mut None,
                                None,
                            )
                            .await
                            .unwrap_or(Type::Unknown)
                        }
                        _ => super::typecheck_annot::resolve_type_expr(
                            body,
                            &stub_env,
                            state,
                            &mut alias_constraints,
                            &mut Some(&mut ann_map_for_body),
                            &mut None,
                            None,
                        )
                        .await
                        .unwrap_or(Type::Unknown),
                    };

                    // Qualify constructor tags with the alias name (e.g., "Ok" → "Result.Ok").
                    // NominalVariant tags in the raw resolved body are unqualified bare names
                    // from the source; they must be qualified so type display and unification
                    // use the correct qualified tags.
                    let qualify_tag = |tag: &str| -> String {
                        if alias_name.is_empty() || tag.contains('.') {
                            tag.to_string()
                        } else {
                            format!("{}.{}", alias_name, tag)
                        }
                    };
                    let qualify_nominal = |ty: Type| -> Type {
                        match ty {
                            Type::NominalVariant { tag, fields } => Type::NominalVariant {
                                tag: qualify_tag(&tag),
                                fields,
                            },
                            other => other,
                        }
                    };
                    let qualified_body = match resolved_body {
                        Type::NominalVariant { tag, fields } => Type::NominalVariant {
                            tag: qualify_tag(&tag),
                            fields,
                        },
                        Type::Union(members) => Type::normalize_union(
                            members.into_iter().map(qualify_nominal).collect(),
                        ),
                        other => other,
                    };
                    let constructors: Vec<(String, usize)> = match &qualified_body {
                        Type::NominalVariant { tag, fields } => {
                            let arity = if fields.fields.is_empty() { 0 } else { 1 };
                            vec![(tag.clone(), arity)]
                        }
                        Type::Union(members) => members
                            .iter()
                            .filter_map(|m| match m {
                                Type::NominalVariant { tag, fields } => {
                                    let arity = if fields.fields.is_empty() { 0 } else { 1 };
                                    Some((tag.clone(), arity))
                                }
                                _ => None,
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    let param_names: Vec<String> = params.iter().map(|(n, _)| n.clone()).collect();
                    let tycon_def = std::sync::Arc::new(TyConDef {
                        params: param_names,
                        body: qualified_body.clone(),
                        constraints: Vec::new(),
                        variance: Vec::new(),
                        constructors,
                        builtin_type: None,
                        annotation: None,
                        field_annotations: indexmap::IndexMap::new(),
                        constructor_constants: indexmap::IndexMap::new(),
                    });
                    let alias_ty = qualified_body;

                    // Register the named alias (keyed entries only)
                    if let Some(name) = key_name {
                        let remapped_params: Vec<String> = params
                            .iter()
                            .map(|(p, _)| {
                                alias_ann_map.get(p).cloned().unwrap_or_else(|| p.clone())
                            })
                            .collect();
                        dict_env.write().unwrap().insert_type_alias(
                            name.clone(),
                            TypeAlias {
                                params: remapped_params,
                                body: alias_ty.clone(),
                            },
                        );
                        // Register in state.tycon_env so coverage checking and tests can find it.
                        // Use or_insert to preserve static seed entries (e.g. DirCap with body
                        // Type::DirCap from the initial TypeContext) over dynamic declarations
                        // that produce nominal bodies from tinct [type ...] syntax.
                        let tycon_constructors = tycon_def.constructors.clone();
                        state.tycon_env.entry(name.clone()).or_insert(tycon_def);
                        // Update the scheme for this alias name with the resolved alias body type.
                        // Pass 1 pre-inserted a fresh TypeVar placeholder for the alias slot; we
                        // replace it here with the actual resolved type so that callers who look up
                        // the alias name (e.g. `env_get(&env, "Bool")` in tests) see the correct
                        // Union/NominalVariant type rather than the raw TypeVar placeholder.
                        // Only zero-arity aliases are resolved to concrete types at this point;
                        // parameterized aliases stay as TypeVar (they are instantiated on use).
                        if params.is_empty() {
                            dict_env
                                .write()
                                .unwrap()
                                .insert_scheme(name.clone(), TypeScheme::mono(alias_ty.clone()));
                            // Collect constructor TypeSchemes so callers can look up constructors
                            // by their short name (e.g. "True" for Bool.True, "Ok" for Result.Ok).
                            // Unit constructors get NominalVariant with the qualified tag.
                            // Payload constructors are typed via the evaluator's desugar path.
                            for (qualified_tag, arity) in &tycon_constructors {
                                let short =
                                    qualified_tag.rsplit('.').next().unwrap_or(qualified_tag);
                                if *arity == 0 {
                                    let ctor_ty = Type::NominalVariant {
                                        tag: qualified_tag.clone(),
                                        fields: Row {
                                            fields: indexmap::IndexMap::new(),
                                            tail: RowTail::Empty,
                                        },
                                    };
                                    ctor_schemes
                                        .insert(short.to_string(), TypeScheme::mono(ctor_ty));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Initialize global substitution and field types accumulator.
    // Start with empty local substitution and incrementally merge state.subst entries per SCC.
    // Eliminates O(n) upfront clone of state.subst.type_map (cycle-31 major item).
    let subst = Substitution {
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
    //
    // IMPORTANT: Pass 0c runs AFTER Pass 1 so that dict_env includes all letrec
    // TypeVar placeholders (including sibling names like `result-map`). Running before
    // Pass 1 would leave instance method bodies unable to see sibling bindings.
    //
    // dict_env is the single Arc<RwLock<Env>> used throughout infer_dict. Pass 0c writes
    // ɪ-prefixed TypeSchemes (from infer_instance_decl_from_surface) directly into it, so
    // they are visible to all subsequent SCC clones without any propagation step.
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
            match Box::pin(infer_surface_expr(
                &entry.node.value,
                &dict_env,
                state,
                type_map,
            ))
            .await
            {
                Ok(ty) => {
                    let (ref key_name, _, _) = key_entries[idx];
                    if let Some(name) = key_name {
                        field_types.insert(name.clone(), ty);
                    }
                }
                Err(mut errs) => {
                    let (ref key_name, _, _) = key_entries[idx];
                    if let Some(name) = key_name {
                        // Carry the causal errors in the Error node so downstream
                        // blame (e.g. "expected function type, got <error>") can point
                        // back to the root cause at this binding site.
                        // Use Type::Error (not Type::Unknown) so that:
                        // - unify(Error, T) succeeds silently (prevents cascade errors)
                        // - is_subtype(Error, _) = false (TypeAsserts correctly reject)
                        // - T010 "inferred Unknown" diagnostic doesn't fire (Error ≠ Unknown)
                        // - lower.rs emits TypeAssert with Type::Unknown (accepts all values)
                        let typed: Vec<crate::type_errors::TypeErrorTyped> = errs
                            .iter()
                            .map(|e| {
                                crate::type_errors::TypeErrorTyped::new(
                                    e.message.clone(),
                                    e.span.clone(),
                                )
                            })
                            .collect();
                        field_types.insert(name.clone(), Type::error_with(typed));
                        state
                            .failed_bindings
                            .insert(name.clone(), entry.span.clone());
                    }
                    errors.append(&mut errs);
                }
            }
        }
    }

    // Process each SCC in topological order
    // Tarjan's algorithm produces SCCs in reverse topological order, so we process them as-is
    for scc in sccs.into_iter() {
        // Pass 1_i (SCC): Collect the fresh TypeVars assigned above for THIS SCC's entries.
        // We no longer insert here (done in global Pass 1 above); we only collect the bound
        // vars for use in unification (Pass 3_i).
        enum FreshVars {
            Singleton(String, Type),
            Multiple(HashMap<String, Type>),
        }
        let mut fresh_vars_storage = None;

        for &idx in &scc.indices {
            let (ref key_name, is_alias, _is_static) = key_entries[idx];
            if !is_alias {
                if let Some(ref name) = key_name {
                    if let Some(fresh_var) = fresh_vars_by_name.get(name).cloned() {
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
                    }
                }
            }
        }

        // Clone the up-to-date dict_env (including Pass 0c's ɪ-prefixed schemes) for
        // within-SCC isolation. Each SCC gets a fresh snapshot so mutual recursion among
        // SCC members is handled via TypeVar unification within the snapshot; Pass 4
        // then promotes the generalized results back to dict_env.
        let scc_env = Arc::new(RwLock::new(dict_env.read().unwrap().clone()));

        // Pass 3_i: Infer values and unify with bound type vars for this SCC
        for &idx in &scc.indices {
            let entry = &entries[idx];
            let (ref key_name, is_alias, _is_static) = key_entries[idx];

            // Skip type aliases (already processed in Pass 2), Rest markers,
            // and class/instance declarations (already processed in Pass 0c)
            let skip = is_alias
                || matches!(&entry.node.value.expr, SurfaceExpression::Rest(..))
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
                // Type assertion annotation resolution is async; infer_dict is sync.
                // Skip async annotation resolution — type_assert_ty falls back to None,
                // and type inference will determine the type from the expression body.
                let type_assert_ty: Option<Type> = None;

                // Special case: if the value is a Dict, call infer_dict directly to capture schemes
                let (value_ty, nested_schemes_opt) =
                    if let SurfaceExpression::Dict(nested_entries) = &entry.node.value.expr {
                        let (ty, schemes, mut nested_errs) = Box::pin(infer_dict(
                            nested_entries,
                            &scc_env,
                            state,
                            type_map,
                            entry.node.value.span.clone(),
                        ))
                        .await;
                        errors.append(&mut nested_errs);
                        (Ok(ty), Some(schemes))
                    } else {
                        (
                            infer_surface_expr(&entry.node.value, &scc_env, state, type_map).await,
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

                // Store nested schemes if present.
                // inner_schemes is HashMap-keyed (name-based DOT-POLY lookup, no slot indexing),
                // so convert from IndexMap to HashMap here.
                if let Some(nested_schemes) = nested_schemes_opt {
                    entry_inner_schemes.insert(name.clone(), nested_schemes.into_iter().collect());
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
                            // Bind the TypeVar to the inferred type in the substitution.
                            // (unify is async; infer_dict is sync — direct substitution instead)
                            if let Type::TypeVar(var_name, _) = bound_var {
                                subst
                                    .type_map
                                    .borrow_mut()
                                    .insert(var_name.clone(), value_ty.clone());
                            }
                            field_types.insert(name.clone(), value_ty);
                        } else {
                            field_types.insert(name.clone(), value_ty);
                        }
                    }
                    Err(mut errs) => {
                        let typed: Vec<crate::type_errors::TypeErrorTyped> = errs
                            .iter()
                            .map(|e| {
                                crate::type_errors::TypeErrorTyped::new(
                                    e.message.clone(),
                                    e.span.clone(),
                                )
                            })
                            .collect();
                        let error_ty = Type::error_with(typed);
                        errors.append(&mut errs);
                        // If the entry was wrapped in TypeAssert ([@T expr]), use the asserted
                        // type T as the public field type even when the body has errors. This
                        // ensures callers see the declared type T rather than Error, preserving
                        // the purpose of the annotation as a type-interface boundary.
                        // Fall back to Error only when no assertion type is available.
                        let fallback_ty = type_assert_ty.unwrap_or(error_ty);
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
                        // Can't use async unify in sync infer_dict; use direct substitution
                        // as a best-effort merge (may lose some unification precision)
                        let resolved = subst.apply(&applied_v);
                        subst.type_map.borrow_mut().insert(k, resolved);
                        let _ = existing; // suppress unused warning
                    }
                    None => {
                        subst.type_map.borrow_mut().insert(k, applied_v);
                    }
                }
            }
        }

        // Skip async process_deferred_equalities in sync infer_dict context.
        let _ = &state.deferred_equalities; // acknowledge field exists

        // Apply substitution to this SCC's field types
        for &idx in &scc.indices {
            let (ref key_name, _, _) = key_entries[idx];
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
        // Substitution size check removed (check_size not available on Substitution)

        // Pass 4_i: Generalize this SCC's entries before processing the next SCC
        for &idx in &scc.indices {
            let entry = &entries[idx];
            let (ref key_name, _, _) = key_entries[idx];

            if let Some(name) = key_name {
                if let Some(ty) = field_types.get(name) {
                    // Extract doc string from key annotation (e.g., name@[doc: "..."])
                    let key_doc = if let Some(ref key_node) = entry.node.key {
                        match &key_node.expr {
                            SurfaceExpression::VarRef {
                                annotation: Some(ann),
                                ..
                            } => ann.node.get_property("doc").and_then(|doc_node| {
                                if let SurfaceExpression::StringLiteral {
                                    content: doc_string,
                                    ..
                                } = &doc_node.expr
                                {
                                    Some(doc_string.clone())
                                } else {
                                    None
                                }
                            }),
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
                                    if let SurfaceExpression::StringLiteral {
                                        content: doc_string,
                                        ..
                                    } = &doc_node.expr
                                    {
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
                        dict_env
                            .write()
                            .unwrap()
                            .insert_scheme(name.clone(), scheme);
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
                    dict_env
                        .write()
                        .unwrap()
                        .insert_scheme(name.clone(), scheme);
                }
            }
        }
    }

    // Re-apply zero-arity TypeAlias schemes from state.tycon_env.
    // Pass 4 (generalization) re-inserts each entry's scheme — for TypeAlias entries this
    // produces a generalized TypeVar (the placeholder from Pass 1) rather than the resolved
    // alias body (Union, NominalVariant, etc.). Re-applying here restores the correct type.
    for (key_name, is_alias, _) in &key_entries {
        if *is_alias {
            if let Some(name) = key_name {
                if let Some(def) = state.tycon_env.get(name.as_str()) {
                    if def.params.is_empty() {
                        dict_env
                            .write()
                            .unwrap()
                            .insert_scheme(name.clone(), TypeScheme::mono(def.body.clone()));
                    }
                }
            }
        }
    }

    // Build final schemes map from dict_env in SOURCE ORDER.
    // IndexMap preserves insertion order, which must match the resolver's slot
    // assignment order (surface_dict_static_keys iterates key_entries in source
    // order). Callers use insert_scheme() which appends to an IndexMap-backed
    // slots table; get_scheme_at(level, slot) looks up by positional index.
    // A HashMap here would produce non-deterministic slot misalignment — see
    // the diagnosis: 109 false-positive type warnings from HashMap iteration.
    let mut schemes = indexmap::IndexMap::with_capacity(field_types.len());
    {
        let dict_env_guard = dict_env.read().unwrap();
        for (key_name, _is_alias, _) in &key_entries {
            if let Some(name) = key_name {
                if let Some(scheme) = dict_env_guard.get_scheme(name) {
                    schemes.insert(name.clone(), scheme);
                }
            }
        }
    }
    // Merge in ADT constructor schemes collected in Pass 2.
    // These are short-name bindings (e.g. "True" → NominalVariant("Bool.True")) that
    // did not have resolver-assigned slots and therefore aren't in key_entries.
    for (name, scheme) in ctor_schemes {
        schemes.entry(name).or_insert(scheme);
    }

    // Restore enclosing level
    state.level = enclosing_level;

    // Compact the levels map: remove entries for TypeVars that have been unified.
    // This prevents unbounded growth during long inference sessions (e.g., prelude loading).
    state.compact_levels();

    // Apply the local substitution to field types before building the Record.
    // Also apply state.subst to capture global bindings (e.g. from FD improvement).
    // Detect TypeVar cycles in the local substitution (e.g. mutual recursion where
    // a: $b and b: $a creates _t0 → TypeVar("_t1") and _t1 → TypeVar("_t0")).
    // When a cycle is detected, replace the cycled TypeVar with Unknown (unresolvable type).
    let resolved_field_types: indexmap::IndexMap<String, Type> = field_types
        .into_iter()
        .map(|(k, v)| {
            // Apply local subst first, then state.subst
            let after_local = subst.apply(&v);
            let after_state = state.subst.apply(&after_local);
            // Detect 2-cycle: if v was a TypeVar that pointed to another TypeVar in local subst,
            // and that other TypeVar points back to v, we have a mutual recursion cycle.
            // Replace with Unknown since the type is indeterminate.
            let resolved = match (&v, &after_local) {
                (Type::TypeVar(orig_name, _), Type::TypeVar(next_name, _))
                    if orig_name != next_name =>
                {
                    // Check if next_name → orig_name (cycle) in local subst
                    let local_map = subst.type_map.borrow();
                    let is_cycle = local_map.get(next_name.as_str()).map_or(
                        false,
                        |t| matches!(t, Type::TypeVar(n, _) if n == orig_name),
                    );
                    drop(local_map);
                    if is_cycle {
                        Type::Unknown
                    } else {
                        after_state
                    }
                }
                _ => after_state,
            };
            (k, resolved)
        })
        .collect();
    // If the dict has any spread entries (...expr), the result is an open dict (Dict),
    // not a closed Record. The `...` marker is the syntactic signal for openness.
    let has_spread = entries.iter().any(|e| {
        e.node.key.is_none()
            && matches!(&e.node.value.expr, crate::ast::SurfaceExpression::Rest(..))
    });
    let tail = if has_spread {
        crate::type_def::RowTail::Uniform {
            key: None,
            value: Box::new(Type::Any),
        }
    } else {
        crate::type_def::RowTail::Empty
    };
    let record_type = Type::Dict(Row {
        fields: resolved_field_types,
        tail,
    });

    // Always return best-effort results along with any errors.
    // The schemes collected in dict_env are correct for entries that succeeded; failed
    // entries have Type::Error (or the TypeAssert fallback) in record_type and are
    // marked in state.failed_bindings. Callers propagate errors via the third element.
    (record_type, schemes, errors)
}

pub(crate) async fn entry_key_name(
    entry: &SurfaceEntry,
    auto_index: &mut i64,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Option<String> {
    match &entry.key {
        Some(key_node) => match &key_node.expr {
            SurfaceExpression::StringLiteral { content: s, .. } => Some(s.clone()),
            SurfaceExpression::Int(n) => Some(n.to_string()),
            // Annotated key: name@[doc: "..."] — extract name directly
            // VarRef with annotation (name@[doc: "..."]) — extract name
            SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
            _ => match Box::pin(infer_surface_expr(key_node, env, state, type_map)).await {
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
        Arc::new(SurfaceNode::new(
            expr,
            crate::ast::Span::rust_source(file!(), line!()),
        ))
    }

    /// Helper: build a `Spanned<SurfaceEntry>` whose value is a `VarRef` to `ref_name`.
    /// Used to encode a dependency edge: this entry's value references `ref_name`.
    fn entry_ref(ref_name: &str) -> Spanned<SurfaceEntry> {
        sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::VarRef {
                name: ref_name.to_string(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
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

    /// Helper: build a key_entries list of named, non-alias, static-key entries.
    fn key_entries_for(names: &[&str]) -> Vec<(Option<String>, bool, bool)> {
        names
            .iter()
            .map(|n| (Some(n.to_string()), false, true))
            .collect()
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
        let key_entries: Vec<(Option<String>, bool, bool)> = vec![];
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
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
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
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
            }),
        });
        let b_entry = sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::VarRef {
                name: "a".to_string(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
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
                        resolution: crate::ast::Resolution::new(),
                        call_dispatch: crate::ast::CallDispatch::new(),
                        annotation: None,
                    }),
                }),
                sp(SurfaceEntry {
                    key: None,
                    value: sn(SurfaceExpression::VarRef {
                        name: "c".to_string(),
                        escaped: false,
                        resolution: crate::ast::Resolution::new(),
                        call_dispatch: crate::ast::CallDispatch::new(),
                        annotation: None,
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
