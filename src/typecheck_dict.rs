//! Dict type inference with multi-pass binding and generalization.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::{infer_expr, resolve_type_expr, TypeMap};
use crate::ast::{Entry, Expr, Span, Spanned};
use crate::types::{
    generalize_with_doc, unify, InferState, Row, Substitution, Type, TypeAlias, TypeEnv, TypeError,
    TypeScheme,
};

/// Strongly Connected Component - a group of mutually dependent bindings
struct Scc {
    /// Indices into the entries array
    indices: Vec<usize>,
}

/// Tarjan's algorithm for computing SCCs in topological order.
/// Returns SCCs in reverse topological order (dependencies before dependents).
///
/// Uses an iterative worklist implementation to avoid stack overflow on large
/// prelude dicts with many interdependent bindings. The recursive formulation
/// of Tarjan's algorithm would overflow on dicts with O(n) dependency chains;
/// the iterative version uses an explicit work stack instead of the call stack.
fn compute_sccs(entries: &[Spanned<Entry>], key_entries: &[(Option<String>, bool)]) -> Vec<Scc> {
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
fn collect_dependencies(expr: &Spanned<Expr>, name_to_idx: &HashMap<String, usize>) -> Vec<usize> {
    let mut deps = HashSet::new();
    let mut worklist: Vec<&Spanned<Expr>> = vec![expr];

    while let Some(current) = worklist.pop() {
        match &current.node {
            Expr::VarRef { name, .. } => {
                if let Some(&idx) = name_to_idx.get(name) {
                    deps.insert(idx);
                }
            }
            Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) => {}
            Expr::Dict(entries) => {
                for entry in entries {
                    if let Some(ref key) = entry.node.key {
                        worklist.push(key);
                    }
                    worklist.push(&entry.node.value);
                }
            }
            Expr::Fn { body, .. } => {
                worklist.push(body);
            }
            Expr::Call {
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
            Expr::Match { scrutinee, arms } => {
                worklist.push(scrutinee);
                for arm in arms {
                    worklist.push(&arm.body);
                    if let Some(ref guard) = arm.guard {
                        worklist.push(guard);
                    }
                }
            }
            Expr::DotAccess { expr, .. } => {
                worklist.push(expr);
            }
            Expr::Pipe { lhs, rhs } => {
                worklist.push(lhs);
                worklist.push(rhs);
            }
            Expr::Sequential(exprs) => {
                for e in exprs {
                    worklist.push(e);
                }
            }
            Expr::Annotated { .. } => {
                // Annotated is a name with annotation, not an expr containing expr
                // No dependencies to collect
            }
            Expr::TypeAssert { expr, .. } => {
                worklist.push(expr);
            }
            Expr::Rest(_) => {}
            Expr::Quote(e) | Expr::Unquote(e) | Expr::UnquoteSplice(e) => {
                worklist.push(e);
            }
            Expr::DefMacro { body, .. } => {
                worklist.push(body);
            }
            Expr::TypeAlias { .. } => {}
            Expr::ClassDecl { .. } | Expr::InstanceDecl { .. } => {}
            Expr::TypeApp { func, arg } => {
                worklist.push(func);
                worklist.push(arg);
            }
            Expr::Error(_) => {}
        }
    }

    deps.into_iter().collect()
}

pub(crate) fn infer_dict(
    entries: &[Spanned<Entry>],
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
    span: Span,
) -> Result<(Type, HashMap<String, TypeScheme>), Vec<TypeError>> {
    // Level management: save enclosing level, increment for dict body
    let enclosing_level = state.level;
    state.level += 1;

    let mut dict_env = TypeEnv::with_parent(env);
    let mut key_entries: Vec<(Option<String>, bool)> = Vec::new();
    let mut auto_index: i64 = 0;

    // Pass 0: Key resolution
    for entry in entries {
        let key_name = entry_key_name(&entry.node, &mut auto_index, env, state, type_map);
        let is_alias = matches!(&entry.node.value.node, Expr::TypeAlias { .. });
        key_entries.push((key_name, is_alias));
    }

    // Pass 0a: Compute SCCs for binding group analysis
    let sccs = compute_sccs(entries, &key_entries);

    // Pass 2: Register type aliases (before SCC processing)
    for ((key_name, is_alias), entry) in key_entries.iter().zip(entries.iter()) {
        if *is_alias {
            if let Some(name) = key_name {
                if let Expr::TypeAlias { params, body } = &entry.node.value.node {
                    let mut alias_ann_map: HashMap<String, String> = HashMap::new();
                    for p in params {
                        let fresh = format!("_t{}", state.name_counter);
                        state.name_counter += 1;
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
                        let remapped_params: Vec<String> = params
                            .iter()
                            .map(|p| alias_ann_map.get(p).cloned().unwrap())
                            .collect();
                        dict_env.insert_type_alias(
                            name.clone(),
                            TypeAlias {
                                params: remapped_params,
                                body: alias_ty,
                            },
                        );
                    }
                }
            }
        }
    }

    let mut dict_env = dict_env;

    // Initialize global substitution and field types accumulator
    let mut subst = Substitution {
        type_map: std::cell::RefCell::new(state.subst.type_map.borrow().clone()),
    };
    let mut field_types: HashMap<String, Type> = HashMap::new();
    let mut errors = Vec::new();

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
        match &entry.node.value.node {
            Expr::ClassDecl { .. } | Expr::InstanceDecl { .. } => {
                // Infer the class/instance declaration, which registers it in state.class_env
                // or state.instance_env. The result type is always an empty record.
                match infer_expr(&entry.node.value, &dict_env_rc, state, type_map) {
                    Ok(ty) => {
                        // Store the type in field_types for this entry
                        let (ref key_name, _) = key_entries[idx];
                        if let Some(name) = key_name {
                            field_types.insert(name.clone(), ty);
                        }
                    }
                    Err(mut errs) => {
                        errors.append(&mut errs);
                        let (ref key_name, _) = key_entries[idx];
                        if let Some(name) = key_name {
                            field_types.insert(name.clone(), Type::Unknown);
                            state.failed_bindings.insert(name.clone(), entry.span);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Process each SCC in topological order
    // Tarjan's algorithm produces SCCs in reverse topological order, so we process them as-is
    for scc in sccs.into_iter() {
        // Pass 1_i: Bind this SCC's entries to fresh TypeVars at level state.level
        let mut fresh_vars: HashMap<String, Type> = HashMap::new();
        for &idx in &scc.indices {
            let (ref key_name, is_alias) = key_entries[idx];
            if !is_alias {
                if let Some(ref name) = key_name {
                    let fresh_var = state.fresh_type_var();
                    fresh_vars.insert(name.clone(), fresh_var.clone());
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
            if is_alias
                || matches!(&entry.node.value.node, Expr::Rest(_))
                || matches!(&entry.node.value.node, Expr::ClassDecl { .. })
                || matches!(&entry.node.value.node, Expr::InstanceDecl { .. })
            {
                continue;
            }

            if let Some(name) = key_name {
                // Set current_function for polymorphic recursion detection.
                // Only set it for functions WITHOUT return annotations — functions with
                // annotations can recurse safely because the return type is pinned.
                let should_check_recursion =
                    if let Expr::Fn { return_ann, .. } = &entry.node.value.node {
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

                // Special case: if the value is a Dict, call infer_dict directly to capture schemes
                let (value_ty, nested_schemes_opt) =
                    if let Expr::Dict(nested_entries) = &entry.node.value.node {
                        match infer_dict(
                            nested_entries,
                            &dict_env_rc,
                            state,
                            type_map,
                            entry.node.value.span,
                        ) {
                            Ok((ty, schemes)) => (Ok(ty), Some(schemes)),
                            Err(errs) => (Err(errs), None),
                        }
                    } else {
                        (
                            infer_expr(&entry.node.value, &dict_env_rc, state, type_map),
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
                        if let Some(bound_var) = fresh_vars.get(name.as_str()) {
                            // Unify the inferred type with the bound var
                            if let Err(e) = unify(
                                bound_var,
                                &value_ty,
                                &mut subst,
                                state,
                                entry.node.value.span,
                            ) {
                                errors.push(e);
                                field_types.insert(name.clone(), Type::Unknown);
                                state.failed_bindings.insert(name.clone(), entry.span);
                            } else {
                                field_types.insert(name.clone(), value_ty);
                            }
                        } else {
                            field_types.insert(name.clone(), value_ty);
                        }
                    }
                    Err(mut errs) => {
                        errors.append(&mut errs);
                        field_types.insert(name.clone(), Type::Unknown);
                        state.failed_bindings.insert(name.clone(), entry.span);
                        // Populate type_map with Unknown for LSP hover on failed dict value expressions
                        if let Some(ref mut map) = type_map {
                            let key = (
                                entry.node.value.span.start.offset,
                                entry.node.value.span.end.offset,
                            );
                            map.insert(key, Type::Unknown);
                        }
                    }
                }
            }
        }

        // Merge state.subst into local subst after each SCC
        {
            let state_type_entries: Vec<(String, Type)> = state
                .subst
                .type_map
                .borrow()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (k, v) in state_type_entries {
                let applied_v = subst.apply(&v);
                let existing_opt = subst.type_map.borrow().get(&k).cloned();
                match existing_opt {
                    Some(existing) => {
                        subst.type_map.borrow_mut().remove(&k);
                        if let Err(e) = unify(&existing, &applied_v, &mut subst, state, span) {
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

        // Pass 4_i: Generalize this SCC's entries before processing the next SCC
        for &idx in &scc.indices {
            let entry = &entries[idx];
            let (ref key_name, _) = key_entries[idx];

            if let Some(name) = key_name {
                if let Some(ty) = field_types.get(name) {
                    // Extract doc string from key annotation (e.g., name@[doc: "..."])
                    let key_doc = if let Some(ref key_expr) = entry.node.key {
                        match &key_expr.node {
                            Expr::Annotated { annotation, .. } => {
                                annotation.node.get_property("doc").and_then(|doc_value| {
                                    if let Expr::Str(doc_string) = &doc_value.node {
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
                    let value_doc = match &entry.node.value.node {
                        Expr::Fn { return_ann, .. } => return_ann.as_ref().and_then(|ann| {
                            ann.node.get_property("doc").and_then(|doc_value| {
                                if let Expr::Str(doc_string) = &doc_value.node {
                                    Some(doc_string.clone())
                                } else {
                                    None
                                }
                            })
                        }),
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
                        let mut scheme =
                            generalize_with_doc(enclosing_level, ty, state, doc, entry.span);
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
                        generalize_with_doc(enclosing_level, ty, state, doc, entry.span);

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

    // Merge local subst back into state.subst
    for (k, v) in subst.type_map.borrow().iter() {
        state
            .subst
            .type_map
            .borrow_mut()
            .insert(k.clone(), v.clone());
    }
    state.subst.check_size(span).map_err(|e| vec![e])?;

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

    let record_type = Type::Record(Row {
        fields: field_types,
    });

    if errors.is_empty() {
        Ok((record_type, schemes))
    } else {
        Err(errors)
    }
}

pub(crate) fn entry_key_name(
    entry: &Entry,
    auto_index: &mut i64,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Option<String> {
    match &entry.key {
        Some(key_expr) => match &key_expr.node {
            Expr::Str(s) => Some(s.clone()),
            Expr::Int(n) => Some(n.to_string()),
            // Annotated key: name@[doc: "..."] — extract name directly
            Expr::Annotated { name, .. } => Some(name.clone()),
            _ => match infer_expr(key_expr, env, state, type_map) {
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
