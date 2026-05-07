//! Dict type inference with multi-pass binding and generalization.

use std::collections::HashMap;
use std::rc::Rc;

use super::{infer_expr, resolve_type_expr, TypeMap};
use crate::ast::{Entry, Expr, Span, Spanned};
use crate::types::{
    generalize, unify, InferState, Row, RowTail, Substitution, Type, TypeAlias, TypeEnv, TypeError,
    TypeScheme,
};

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

    // Pass 1: Bind all non-alias entries to fresh TypeVar at level state.level.
    // Also collect fresh vars into a local HashMap for direct O(1) lookup in Pass 3,
    // bypassing the TypeEnv parent-chain traversal in TypeEnv::get().
    let mut fresh_vars: HashMap<String, Type> = HashMap::new();
    for (key_name, is_alias) in &key_entries {
        if !is_alias {
            if let Some(ref name) = key_name {
                let fresh_var = state.fresh_type_var();
                fresh_vars.insert(name.clone(), fresh_var.clone());
                dict_env.insert_scheme(name.clone(), TypeScheme::mono(fresh_var));
            }
        }
    }

    // Pass 2: Register type aliases
    for ((key_name, is_alias), entry) in key_entries.iter().zip(entries.iter()) {
        if *is_alias {
            if let Some(name) = key_name {
                if let Expr::TypeAlias { params, body } = &entry.node.value.node {
                    // Use a fresh per-alias mapping so annotation names within one type
                    // alias expression (e.g., `a` in `[Fn@a [a]]`) consistently map to
                    // the same fresh TypeVar. Without a mapping, every occurrence of `@a`
                    // creates a distinct fresh var, breaking identity-function types.
                    let mut alias_ann_map: HashMap<String, String> = HashMap::new();
                    let mut alias_row_map: HashMap<String, String> = HashMap::new();
                    // Pre-seed param names in BOTH maps so they survive cross-kind
                    // collision checks (a param can appear as both `a` and `...a`).
                    for p in params {
                        let fresh = format!("_t{}", state.name_counter);
                        state.name_counter += 1;
                        state.levels.insert(fresh.clone(), state.level);
                        alias_ann_map.insert(p.clone(), fresh.clone());
                        alias_row_map.insert(p.clone(), fresh.clone());
                    }
                    if let Ok(alias_ty) = resolve_type_expr(
                        body,
                        &dict_env,
                        state,
                        &mut Some(&mut alias_ann_map),
                        &mut Some(&mut alias_row_map),
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

    let dict_env = Rc::new(dict_env);

    // Pass 3a: Initialize local substitution with bindings from state.subst.
    // Algorithm W threads a single substitution through inference. The two-substitution
    // model (local subst + state.subst) is a borrow-checker workaround. We initialize the
    // local subst with state.subst bindings so that letrec unification can see access-chain
    // constraints generated during value inference.
    let mut subst = Substitution {
        type_map: state.subst.type_map.clone(),
        row_map: state.subst.row_map.clone(),
    };

    // Pass 3: Infer values and unify with bound type vars
    let mut field_types: HashMap<String, Type> = HashMap::new();
    let mut errors = Vec::new();

    for ((key_name, is_alias), entry) in key_entries.iter().zip(entries.iter()) {
        if *is_alias || matches!(&entry.node.value.node, Expr::Rest(_)) {
            continue;
        }
        if let Some(name) = key_name {
            match infer_expr(&entry.node.value, &dict_env, state, type_map) {
                Ok(value_ty) => {
                    // Get the bound TypeVar from Pass 1 via direct HashMap lookup,
                    // avoiding TypeEnv parent-chain traversal.
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
                    // Populate type_map with Any for LSP hover on failed dict value expressions
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

    // Pass 3b: Merge bindings from state.subst added during value inference.
    // Algorithm W substitution composition (Damas & Milner 1982): correct composition
    // S = S_state . S_local requires unifying overlapping bindings, not discarding one.
    // The previous or_insert pattern dropped state.subst bindings when local subst already
    // had the same key, leaving access-chain constraints unresolved as free TypeVars.
    {
        let state_type_entries: Vec<(String, Type)> = state
            .subst
            .type_map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, v) in state_type_entries {
            let applied_v = subst.apply(&v);
            match subst.type_map.get(&k).cloned() {
                Some(existing) => {
                    // Remove binding before calling unify to prevent apply() from chasing
                    // k -> existing -> k in a cycle during resolution (mirrors row_map path).
                    subst.type_map.remove(&k);
                    // Both maps bind the same variable: unify to reconcile constraints
                    unify(&existing, &applied_v, &mut subst, state, span).map_err(|e| vec![e])?;
                }
                None => {
                    subst.type_map.insert(k, applied_v);
                }
            }
        }
    }

    // For row_map: apply local subst to field types in state.subst row bindings, then merge.
    // Algorithm W substitution composition: unify on collision (same as type_map above).
    {
        let state_row_entries: Vec<(String, Row)> = state
            .subst
            .row_map
            .iter()
            .map(|(k, row)| (k.clone(), row.clone()))
            .collect();

        // Reusable HashMap to avoid allocation per iteration
        let mut applied_fields: HashMap<String, Type> = HashMap::new();

        for (k, row) in state_row_entries {
            applied_fields.clear();
            for (field_name, field_ty) in &row.fields {
                applied_fields.insert(field_name.clone(), subst.apply(field_ty));
            }
            let applied_row = Row {
                fields: applied_fields.clone(),
                // Tail not applied here; Pass 3c's subst.apply() chases tail chains transitively.
                tail: row.tail.clone(),
            };
            match subst.row_map.get(&k).cloned() {
                Some(existing) => {
                    // Both maps bind the same row variable: unify to reconcile constraints.
                    // Remove the binding for k before calling unify to prevent apply() from
                    // chasing k -> existing -> k in an infinite cycle during resolution.
                    subst.row_map.remove(&k);
                    unify(
                        &Type::Record(existing),
                        &Type::Record(applied_row),
                        &mut subst,
                        state,
                        span,
                    )
                    .map_err(|e| vec![e])?;
                }
                None => {
                    subst.row_map.insert(k, applied_row);
                }
            }
        }
    }

    // Pass 3c: Apply the merged substitution to all field types
    let field_types: HashMap<String, Type> = if subst.is_empty() {
        // Fast path: no substitution needed, avoid O(n) apply() calls
        field_types
    } else {
        field_types
            .into_iter()
            .map(|(k, ty)| (k, subst.apply(&ty)))
            .collect()
    };

    // Pass 3d: Merge local subst back into state.subst so that subsequent dict entries
    // in the same document can see the letrec bindings from this dict.
    // Without this, access-chain constraints in later dicts won't resolve TypeVars
    // that were bound during this dict's letrec unification.
    for (k, v) in &subst.type_map {
        state.subst.type_map.insert(k.clone(), v.clone());
    }
    for (k, row) in &subst.row_map {
        state.subst.row_map.insert(k.clone(), row.clone());
    }
    state.subst.check_size(span).map_err(|e| vec![e])?;

    // Pass 4: Generalize - create TypeSchemes for each entry
    let mut schemes = HashMap::with_capacity(field_types.len());
    for (name, ty) in &field_types {
        let scheme = generalize(enclosing_level, ty, state);
        schemes.insert(name.clone(), scheme);
    }

    // Restore enclosing level
    state.level = enclosing_level;

    let record_type = Type::Record(Row {
        fields: field_types,
        tail: RowTail::Empty,
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
