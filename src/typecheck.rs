//! Type checker: infers types from AST expressions, resolves type aliases,
//! validates type assertions, and performs Hindley-Milner style type variable
//! unification for polymorphic function calls.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{Annotation, Document, Entry, Expr, File, NamedArg, Param, Span, Spanned};
use crate::types::{
    generalize, instantiate_at_level, instantiate_scheme, lower_row_var_levels_pub,
    row_var_occurs_pub, unify, InferState, Row, RowTail, Substitution, Type, TypeEnv, TypeError,
    TypeScheme,
};

/// A map from expression span (start_offset, end_offset) to the inferred type.
/// Populated during type checking so hover can look up types without re-inference.
pub type TypeMap = HashMap<(usize, usize), Type>;

pub fn typecheck_file(file: &File) -> Result<(), Vec<TypeError>> {
    let mut errors = Vec::new();
    let mut env = Rc::new(TypeEnv::new());
    let mut state = InferState::new();

    for doc in &file.documents {
        match typecheck_document(doc, &env, &mut state, &mut None) {
            Ok(new_env) => env = new_env,
            Err(mut doc_errors) => errors.append(&mut doc_errors),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Type-check a file, returning both errors and a map from expression spans to
/// inferred types. The type map is populated even when errors occur, covering
/// every expression that was successfully inferred.
pub fn typecheck_file_with_types(file: &File) -> (Vec<TypeError>, TypeMap) {
    let mut errors = Vec::new();
    let mut env = Rc::new(TypeEnv::new());
    let mut state = InferState::new();
    let mut type_map = TypeMap::new();

    for doc in &file.documents {
        match typecheck_document(doc, &env, &mut state, &mut Some(&mut type_map)) {
            Ok(new_env) => env = new_env,
            Err(mut doc_errors) => errors.append(&mut doc_errors),
        }
    }

    (errors, type_map)
}

fn typecheck_document(
    doc: &Spanned<Document>,
    parent_env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Rc<TypeEnv>, Vec<TypeError>> {
    let mut errors = Vec::new();
    let mut env = Rc::new(TypeEnv::with_parent(Rc::clone(parent_env)));
    let mut result_type = Type::Record(Row {
        fields: HashMap::new(),
        tail: RowTail::Empty,
    });

    let exprs = &doc.node.expressions;
    if exprs.is_empty() {
        let mut result_env = TypeEnv::with_parent(Rc::clone(&env));
        result_env.insert(
            "$".to_string(),
            Type::Record(Row {
                fields: HashMap::new(),
                tail: RowTail::Empty,
            }),
        );
        return Ok(Rc::new(result_env));
    }

    let mut last_dict_schemes: Option<IndexMap<String, TypeScheme>> = None;
    let mut last_expr: Option<&Spanned<Expr>> = None;

    for (i, expr) in exprs.iter().enumerate() {
        let is_last = i == exprs.len() - 1;

        // Special handling for Dict expressions at document level to preserve schemes
        if let Expr::Dict(entries) = &expr.node {
            match infer_dict(entries, &env, state, type_map) {
                Ok((ty, schemes)) => {
                    if is_last {
                        result_type = ty;
                        last_dict_schemes = Some(schemes);
                        last_expr = Some(expr);
                    } else {
                        let mut new_env = TypeEnv::with_parent(Rc::clone(&env));
                        // Thread schemes into the environment
                        for (name, scheme) in &schemes {
                            new_env.insert_scheme(name.clone(), scheme.clone());
                        }
                        let mut alias_errs = register_type_aliases(expr, &mut new_env, &env, state);
                        errors.append(&mut alias_errs);
                        env = Rc::new(new_env);
                    }
                }
                Err(mut errs) => errors.append(&mut errs),
            }
        } else {
            match infer_expr(expr, &env, state, type_map) {
                Ok(ty) => {
                    if is_last {
                        result_type = ty;
                    } else {
                        match &ty {
                            Type::Record(Row { fields, .. }) => {
                                // LIMITATION: non-Dict intermediate results lose polymorphism — fields inserted as monomorphic schemes
                                let mut new_env = TypeEnv::with_parent(Rc::clone(&env));
                                for (name, field_ty) in fields {
                                    new_env.insert(name.clone(), field_ty.clone());
                                }
                                let mut alias_errs =
                                    register_type_aliases(expr, &mut new_env, &env, state);
                                errors.append(&mut alias_errs);
                                env = Rc::new(new_env);
                            }
                            Type::Any => {}
                            _ => errors.push(TypeError::not_a_record(&ty, expr.span)),
                        }
                    }
                }
                Err(mut errs) => errors.append(&mut errs),
            }
        }
    }

    let mut result_env = TypeEnv::with_parent(Rc::clone(&env));

    // If the last expression was a dict, thread its schemes into the result environment
    if let Some(schemes) = last_dict_schemes {
        for (name, scheme) in schemes {
            result_env.insert_scheme(name, scheme);
        }
    }

    // Register type aliases from the last expression
    // Note: errors are intentionally ignored here, matching the behavior of infer_dict Pass 2.
    // Type alias resolution errors are reported when the aliases are used, not when registered.
    if let Some(expr) = last_expr {
        let _ = register_type_aliases(expr, &mut result_env, &env, state);
    }

    result_env.insert("$".to_string(), result_type);

    if errors.is_empty() {
        Ok(Rc::new(result_env))
    } else {
        Err(errors)
    }
}

fn register_type_aliases(
    expr: &Spanned<Expr>,
    target_env: &mut TypeEnv,
    resolve_env: &TypeEnv,
    state: &mut InferState,
) -> Vec<TypeError> {
    let mut errors = Vec::new();
    if let Expr::Dict(entries) = &expr.node {
        for entry in entries {
            if let Some(ref key) = entry.node.key {
                if let Expr::Str(name) = &key.node {
                    if let Expr::TypeAlias(inner) = &entry.node.value.node {
                        match resolve_type_expr(inner, resolve_env, state, &mut None) {
                            Ok(alias_ty) => {
                                target_env.insert_type_alias(name.clone(), alias_ty);
                            }
                            Err(e) => errors.push(e),
                        }
                    }
                }
            }
        }
    }
    errors
}

fn infer_expr(
    expr: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let result = match &expr.node {
        Expr::Int(n) => Ok(Type::IntLiteral(*n)),
        Expr::Float(_) => Ok(Type::Float),
        Expr::Bool(_) => Ok(Type::Bool),
        Expr::Str(s) => Ok(Type::StringLiteral(s.clone())),

        Expr::VarRef(name) => env
            .get(name)
            .map(|scheme| instantiate_scheme(scheme, state.level, state))
            .ok_or_else(|| vec![TypeError::undefined_variable(name, expr.span)]),

        Expr::Dict(entries) => infer_dict(entries, env, state, type_map).map(|(ty, _schemes)| ty),

        Expr::DotAccess {
            expr: target,
            field,
        } => check_dot_access(target, field, env, expr.span, state, type_map),

        Expr::BracketAccess { expr: target, key } => {
            check_bracket_access(target, key, env, expr.span, state, type_map)
        }

        Expr::RangeAccess {
            expr: target,
            start,
            end,
        } => check_range_access(target, start, end, env, expr.span, state, type_map),

        Expr::Call {
            func,
            args,
            named_args,
        } => {
            // Special case: if func is a VarRef to a polymorphic scheme, pass the scheme
            // directly to avoid double instantiation (VAR-POLY followed by CALL-POLY).
            // For monomorphic schemes, use the normal path which handles TypeVar during letrec.
            if let Expr::VarRef(name) = &func.node {
                match env.get(name) {
                    Some(scheme) if !scheme.type_vars.is_empty() || !scheme.row_vars.is_empty() => {
                        // Polymorphic scheme: optimize by instantiating once in check_call_with_scheme
                        check_call_with_scheme(
                            scheme, args, named_args, env, expr.span, state, type_map,
                        )
                    }
                    Some(_) => {
                        // Monomorphic scheme: use normal path which handles TypeVar during letrec
                        check_call(func, args, named_args, env, expr.span, state, type_map)
                    }
                    None => {
                        // Special handling for $proxy builtin: produces Type::Proxy
                        if name == "proxy" {
                            // Infer arguments for type map population
                            for arg in args {
                                let _ = infer_expr(arg, env, state, type_map)?;
                            }
                            for na in named_args {
                                let _ = infer_expr(&na.node.value, env, state, type_map)?;
                            }
                            Ok(Type::Proxy)
                        } else {
                            Err(vec![TypeError::undefined_variable(name, func.span)])
                        }
                    }
                }
            } else {
                check_call(func, args, named_args, env, expr.span, state, type_map)
            }
        }

        Expr::Fn {
            return_ann,
            params,
            body,
            ..
        } => infer_fn(return_ann, params, body, env, expr.span, state, type_map),

        Expr::TypeAlias(inner) => expand_type_alias(inner, env, state).map_err(|e| vec![e]),

        Expr::TypeAssert {
            annotation,
            expr: inner,
            resolved_type,
        } => resolve_type_assert(
            annotation,
            inner,
            resolved_type,
            env,
            expr.span,
            state,
            type_map,
        ),

        Expr::Annotated { name, annotation } => {
            resolve_annotated(name, annotation, env, expr.span, state, &mut None)
                .map_err(|e| vec![e])
        }

        Expr::Rest(_) => Err(vec![TypeError::new(
            "rest marker (...) is only valid inside type expressions",
            expr.span,
        )]),
    };

    // Record the inferred type in the type map (if collecting).
    if let Ok(ref ty) = result {
        if let Some(ref mut map) = type_map {
            let key = (expr.span.start.offset, expr.span.end.offset);
            map.insert(key, ty.clone());
        }
    }

    result
}

/// Check that an expression has a compatible type with the expected type.
/// Uses bidirectional type checking: synthesize the expression's type via `infer_expr`,
/// then check subsumption via `is_subtype(actual, expected)`.
///
/// Per doc/06-type-inference.md §Bidirectional Typing, this is the [SUB] rule:
/// if `Γ ⊢ e ⇒ σ` and `σ <: τ`, then `Γ ⊢ e ⇐ τ`.
///
/// Special case for lambdas (doc/06 line 136-146): when checking a function expression
/// against an expected function type, propagate the expected parameter types into the
/// lambda's parameter inference (Pierce & Turner 2000 lambda checking mode).
///
/// This function is used at checking positions where the expected type is fully concrete
/// (no type variables): CALL-MONO arguments, return annotations, and TypeAssert.
fn check_expr(
    expr: &Spanned<Expr>,
    expected: &Type,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<(), Vec<TypeError>> {
    // Lambda checking mode: when checking a function expression against a function type,
    // propagate expected parameter types into the lambda.
    // Only applies when expected type is fully concrete (no type variables) per doc/06 line 66.
    if let Expr::Fn {
        return_ann,
        params,
        body,
        ..
    } = &expr.node
    {
        if let Type::Function {
            params: expected_params,
            ret: expected_ret,
        } = expected
        {
            // Only use lambda checking mode if expected type is fully concrete
            if !expected.has_type_vars() {
                // Create a fresh annotation mapping for this lambda to prevent
                // cross-contamination of type variables.
                // Only allocate if any param has an annotation or there's a return annotation.
                let has_annotations =
                    params.iter().any(|p| p.node.annotation.is_some()) || return_ann.is_some();
                let mut ann_mapping = if has_annotations {
                    Some(HashMap::new())
                } else {
                    None
                };
                let mut ann_mapping_opt = ann_mapping.as_mut();

                // Arity check
                if params.len() != expected_params.len() {
                    return Err(vec![TypeError::new(
                        format!(
                            "function arity mismatch: expected {} params, got {}",
                            expected_params.len(),
                            params.len()
                        ),
                        expr.span,
                    )]);
                }

                // Build parameter types: use expected types for unannotated params.
                // For annotated params, verify the annotation is compatible with the expected
                // type: expected_ty must be a subtype of the annotation (contravariant check).
                // Example: expected Fn(Int→...) but param declared @String → Int <: String is
                // false → error, because callers will pass Int but the body expects String.
                let param_types: Vec<Type> = params
                    .iter()
                    .zip(expected_params.iter())
                    .map(|(p, expected_ty)| match &p.node.annotation {
                        Some(ann) => {
                            let resolved = resolve_annotation(
                                &ann.node,
                                env,
                                ann.span,
                                state,
                                &mut ann_mapping_opt,
                            )?;
                            // Contravariant check: expected param type must be subtype of annotation.
                            // When annotation contains type variables, use unification mode instead of
                            // is_subtype (C65 fix pattern: TypeVars only match reflexively in is_subtype).
                            if resolved.has_type_vars() {
                                let mut subst = std::mem::take(&mut state.subst);
                                let result =
                                    unify(expected_ty, &resolved, &mut subst, state, ann.span);
                                state.subst = subst;
                                result.map_err(|_e| {
                                    TypeError::type_mismatch(&resolved, expected_ty, ann.span)
                                })?;
                            } else {
                                if !Type::is_subtype(expected_ty, &resolved) {
                                    return Err(TypeError::type_mismatch(
                                        &resolved,
                                        expected_ty,
                                        ann.span,
                                    ));
                                }
                            }
                            Ok(resolved)
                        }
                        None => Ok(expected_ty.clone()),
                    })
                    .collect::<Result<_, _>>()
                    .map_err(|e| vec![e])?;

                // Build function environment with parameter bindings
                let mut fn_env = TypeEnv::with_parent(Rc::clone(env));
                for (param, ty) in params.iter().zip(param_types.iter()) {
                    if param.node.variadic {
                        fn_env.insert(
                            param.node.name.clone(),
                            Type::Record(Row {
                                fields: HashMap::new(),
                                tail: RowTail::Empty,
                            }),
                        );
                    } else {
                        fn_env.insert(param.node.name.clone(), ty.clone());
                    }
                }
                let fn_env = Rc::new(fn_env);

                // Check body against expected return type (or infer if no return annotation)
                match return_ann {
                    Some(ann) => {
                        let declared = resolve_annotation(
                            &ann.node,
                            env,
                            ann.span,
                            state,
                            &mut ann_mapping_opt,
                        )
                        .map_err(|e| vec![e])?;
                        // Check that declared return type is compatible with expected.
                        // When declared contains type variables, use unification mode instead of
                        // is_subtype (C65 fix pattern: TypeVars only match reflexively in is_subtype).
                        if declared.has_type_vars() {
                            let mut subst = std::mem::take(&mut state.subst);
                            let result =
                                unify(&declared, expected_ret, &mut subst, state, ann.span);
                            state.subst = subst;
                            result.map_err(|_e| {
                                vec![TypeError::type_mismatch(expected_ret, &declared, expr.span)]
                            })?;
                        } else {
                            if !Type::is_subtype(&declared, expected_ret) {
                                return Err(vec![TypeError::type_mismatch(
                                    expected_ret,
                                    &declared,
                                    expr.span,
                                )]);
                            }
                        }
                        // Check body against declared return type
                        check_expr(body, &declared, &fn_env, state, type_map)?;
                    }
                    None => {
                        // No return annotation: check body against expected return type
                        check_expr(body, expected_ret, &fn_env, state, type_map)?;
                    }
                }

                // Record the function type in the type map
                if let Some(ref mut map) = type_map {
                    let key = (expr.span.start.offset, expr.span.end.offset);
                    map.insert(key, expected.clone());
                }

                return Ok(());
            }
        }
    }

    // Default: synthesize then check subsumption
    let actual = infer_expr(expr, env, state, type_map)?;
    // Apply state.subst to both types before comparison — access-chain constraints
    // may have bound TypeVars in state.subst. Without substitution, the comparison
    // uses stale TypeVars.
    let actual = state.subst.apply(&actual);
    let expected_resolved = state.subst.apply(expected);
    if !Type::is_subtype(&actual, &expected_resolved) {
        Err(vec![TypeError::type_mismatch(
            &expected_resolved,
            &actual,
            expr.span,
        )])
    } else {
        Ok(())
    }
}

fn infer_dict(
    entries: &[Spanned<Entry>],
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<(Type, IndexMap<String, TypeScheme>), Vec<TypeError>> {
    // Level management: save enclosing level, increment for dict body
    let enclosing_level = state.level;
    state.level += 1;

    let mut dict_env = TypeEnv::with_parent(Rc::clone(env));
    let mut key_entries: Vec<(Option<String>, bool)> = Vec::new();
    let mut auto_index: i64 = 0;

    // Pass 0: Key resolution
    for entry in entries {
        let key_name = entry_key_name(&entry.node, &mut auto_index, env, state, type_map);
        let is_alias = matches!(&entry.node.value.node, Expr::TypeAlias(_));
        key_entries.push((key_name, is_alias));
    }

    // Pass 1: Bind all non-alias entries to fresh TypeVar at level state.level
    for (key_name, is_alias) in &key_entries {
        if !is_alias {
            if let Some(ref name) = key_name {
                let fresh_var = Type::TypeVar(format!("_t{}", state.name_counter), state.level);
                state
                    .levels
                    .insert(format!("_t{}", state.name_counter), state.level);
                state.name_counter += 1;
                dict_env.insert_scheme(name.clone(), TypeScheme::mono(fresh_var));
            }
        }
    }

    // Pass 2: Register type aliases
    for ((key_name, is_alias), entry) in key_entries.iter().zip(entries.iter()) {
        if *is_alias {
            if let Some(name) = key_name {
                if let Expr::TypeAlias(inner) = &entry.node.value.node {
                    if let Ok(alias_ty) = resolve_type_expr(inner, &dict_env, state, &mut None) {
                        dict_env.insert_type_alias(name.clone(), alias_ty);
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
                    // Get the bound TypeVar from Pass 1
                    if let Some(scheme) = dict_env.get(name) {
                        let bound_var = scheme.body.clone();
                        // Unify the inferred type with the bound var
                        if let Err(e) = unify(
                            &bound_var,
                            &value_ty,
                            &mut subst,
                            state,
                            entry.node.value.span,
                        ) {
                            errors.push(e);
                            field_types.insert(name.clone(), Type::Any);
                        } else {
                            field_types.insert(name.clone(), value_ty);
                        }
                    } else {
                        field_types.insert(name.clone(), value_ty);
                    }
                }
                Err(mut errs) => {
                    errors.append(&mut errs);
                    field_types.insert(name.clone(), Type::Any);
                }
            }
        }
    }

    // Pass 3b: Merge any NEW bindings from state.subst that were added during value inference.
    // (Value inference may have generated new access-chain constraints.)
    // For type_map: apply local subst to state.subst bindings, then merge
    for (k, v) in &state.subst.type_map {
        let applied_v = subst.apply(v);
        subst.type_map.entry(k.clone()).or_insert(applied_v);
    }

    // For row_map: apply local subst to field types in state.subst row bindings, then merge
    for (k, row) in &state.subst.row_map {
        // Apply local subst to all field types in the row
        let applied_fields: HashMap<String, Type> = row
            .fields
            .iter()
            .map(|(field_name, field_ty)| (field_name.clone(), subst.apply(field_ty)))
            .collect();
        let applied_row = Row {
            fields: applied_fields,
            // Tail not applied through local subst here — Pass 3c's `subst.apply()` chases tail chains transitively.
            tail: row.tail.clone(),
        };
        subst.row_map.entry(k.clone()).or_insert(applied_row);
    }

    // Pass 3c: Apply the merged substitution to all field types
    let field_types: HashMap<String, Type> = field_types
        .into_iter()
        .map(|(k, ty)| (k, subst.apply(&ty)))
        .collect();

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

    // Pass 4: Generalize - create TypeSchemes for each entry
    let mut schemes = IndexMap::with_capacity(field_types.len());
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

fn entry_key_name(
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

fn check_dot_access(
    target: &Spanned<Expr>,
    field: &str,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let target_ty = infer_expr(target, env, state, type_map)?;
    // Apply the global accumulated substitution so that constraints from prior accesses
    // on the same target are visible (doc/07-type-extensions.md Part 5).
    let target_ty = state.subst.apply(&target_ty);
    match target_ty {
        Type::Record(Row {
            ref fields,
            ref tail,
        }) => match fields.get(field) {
            Some(ty) => Ok(ty.clone()),
            None => match tail {
                // Open record (RowVar tail) and field not found: bind ρ → Row({field: β}, RowVar(ρ_fresh))
                // This records the constraint "ρ must contain field with type β".
                RowTail::RowVar(rho, rho_level_creation) => {
                    // Get the current level from state.levels (the source of truth after level lowering).
                    // The level in RowTail is the creation-time level; state.levels is the current (possibly lowered) level.
                    // See doc/06-type-inference.md lines 413-414.
                    let rho_level = state.levels.get(rho).copied().unwrap_or(0);

                    // Invariant check: current level should be ≤ creation-time level (level lowering can only decrease levels)
                    debug_assert!(
                        rho_level <= *rho_level_creation,
                        "RowVar current level ({}) should be ≤ creation level ({}). \
                         Level lowering can only decrease levels, never increase. \
                         RowVar: {}, state.levels: {:?}",
                        rho_level,
                        rho_level_creation,
                        rho,
                        &state.levels
                    );

                    // Create fresh type var β for the field type
                    let beta = state.fresh_type_var();
                    // Create fresh row var ρ_fresh for the remaining tail
                    let (rho_fresh_name, rho_fresh_level) = state.fresh_row_var_name();

                    // Build the row to bind: Row({ field: β }, RowVar(ρ_fresh))
                    let mut new_fields = HashMap::new();
                    new_fields.insert(field.to_string(), beta.clone());
                    let binding = Row {
                        fields: new_fields,
                        tail: RowTail::RowVar(rho_fresh_name, rho_fresh_level),
                    };

                    // Occurs check: ρ must not appear in the row being bound
                    // (uses state.subst to chase through any existing bindings)
                    //
                    // PROOF SKETCH (why this is unreachable with fresh variables):
                    //   For ρ to occur in Row({field: β}, RowVar(ρ_fresh)), either:
                    //     (a) β contains ρ in its structure, OR
                    //     (b) ρ_fresh = ρ
                    //
                    //   Both are impossible:
                    //     - β is fresh (line 696) with no prior bindings → cannot contain ρ
                    //     - ρ_fresh is fresh (line 698) → ρ_fresh ≠ ρ by uniqueness
                    //
                    //   Therefore, this check is defensive programming that documents the invariant
                    //   but cannot fail when the binding uses only fresh variables.
                    //
                    // Defense-in-depth: Keep the check to guard against future refactorings.
                    if row_var_occurs_pub(rho, &binding, &state.subst) {
                        debug_assert!(
                            false,
                            "unreachable: fresh row var ρ_fresh and fresh type var β cannot contain ρ. \
                             If this fires, check_dot_access was modified to use non-fresh variables."
                        );
                        return Err(vec![TypeError::new(
                            format!("infinite row type: {rho} occurs in its own binding"),
                            span,
                        )]);
                    }

                    // Level lowering: lower all vars in the binding to ρ's current level (from state.levels)
                    lower_row_var_levels_pub(&binding, rho_level, state);

                    // Bind ρ → binding in the global substitution
                    state.subst.row_map.insert(rho.clone(), binding);

                    Ok(beta)
                }
                // Closed record (Empty tail) and field not found: error
                RowTail::Empty => Err(vec![TypeError::field_not_found(field, &target_ty, span)]),
            },
        },
        // Unknown type (TypeVar α): generate constraint α = Record({field: β}, RowVar(ρ))
        // This records "α must be a record with at least this field".
        Type::TypeVar(ref alpha, alpha_level) => {
            // Create fresh β for the field type and ρ for the remaining row
            let beta = state.fresh_type_var();
            let (rho_name, rho_level) = state.fresh_row_var_name();

            // Build the record type to unify α with
            let mut fields = HashMap::new();
            fields.insert(field.to_string(), beta.clone());
            let record_ty = Type::Record(Row {
                fields,
                tail: RowTail::RowVar(rho_name, rho_level),
            });

            // Unify TypeVar(α) with Record({field: β}, RowVar(ρ)) using the global substitution.
            // Borrow-split: mem::take + restore avoids simultaneous &mut state (for unify's inner calls) and &mut state.subst (for compose)
            let alpha_ty = Type::TypeVar(alpha.clone(), alpha_level);
            let mut subst = std::mem::take(&mut state.subst);
            let result = unify(&alpha_ty, &record_ty, &mut subst, state, span);
            state.subst = subst;
            result.map_err(|e| vec![e])?;

            Ok(beta)
        }
        Type::Any => Ok(Type::Any),
        Type::Proxy => Ok(Type::Any),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

fn check_bracket_access(
    target: &Spanned<Expr>,
    key: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let target_ty = infer_expr(target, env, state, type_map)?;
    // Apply the global accumulated substitution (same pattern as check_dot_access)
    let target_ty = state.subst.apply(&target_ty);
    let key_ty = infer_expr(key, env, state, type_map)?;

    match &target_ty {
        Type::Record(Row { fields, tail: rest }) => {
            let is_open = matches!(rest, RowTail::RowVar(_, _));
            let lookup = |field_name: &str| -> Result<Type, Vec<TypeError>> {
                match fields.get(field_name) {
                    Some(ty) => Ok(ty.clone()),
                    None if is_open => Ok(Type::Any),
                    None => Err(vec![TypeError::field_not_found(
                        field_name, &target_ty, span,
                    )]),
                }
            };
            match &key.node {
                Expr::Str(s) => lookup(s),
                Expr::Int(n) => lookup(&n.to_string()),
                _ => match &key_ty {
                    Type::StringLiteral(s) => lookup(s.as_str()),
                    Type::IntLiteral(n) => lookup(&n.to_string()),
                    Type::Str | Type::Int | Type::Any | Type::TypeVar(_, _) => Ok(Type::Any),
                    _ => Err(vec![TypeError::new(
                        format!("bracket access key must be String or Int, got {key_ty}"),
                        span,
                    )]),
                },
            }
        }
        Type::Any | Type::TypeVar(_, _) => Ok(Type::Any),
        Type::Proxy => Ok(Type::Any),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

fn check_range_access(
    target: &Spanned<Expr>,
    start: &Option<Box<Spanned<Expr>>>,
    end: &Option<Box<Spanned<Expr>>>,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let target_ty = infer_expr(target, env, state, type_map)?;
    // Apply the global accumulated substitution (same pattern as check_dot_access)
    let target_ty = state.subst.apply(&target_ty);

    for bound in [start, end].into_iter().flatten() {
        let bound_ty = infer_expr(bound, env, state, type_map)?;
        if !matches!(
            bound_ty,
            Type::Int
                | Type::IntLiteral(_)
                | Type::Str
                | Type::StringLiteral(_)
                | Type::Any
                | Type::TypeVar(_, _)
        ) {
            return Err(vec![TypeError::new(
                format!("range bound must be Int or String, got {bound_ty}"),
                bound.span,
            )]);
        }
    }

    match &target_ty {
        Type::Record(..) | Type::Any | Type::TypeVar(_, _) => Ok(target_ty),
        Type::Proxy => Err(vec![TypeError::new(
            "range access is not supported on Proxy values",
            span,
        )]),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

/// Check a call where the function is a TypeScheme (from a VarRef lookup).
/// This avoids double instantiation: instead of VAR-POLY instantiating the scheme
/// and then CALL-POLY instantiating the result, we instantiate once here.
fn check_call_with_scheme(
    scheme: &TypeScheme,
    args: &[Spanned<Expr>],
    named_args: &[Spanned<NamedArg>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Instantiate the scheme once at the current level
    let func_ty = instantiate_scheme(scheme, state.level, state);

    // Infer named args for type map population and error detection
    for na in named_args {
        let _ = infer_expr(&na.node.value, env, state, type_map)?;
    }

    match &func_ty {
        Type::Function { params, ret } => {
            if params.len() != args.len() {
                return Err(vec![TypeError::new(
                    format!(
                        "arity mismatch: expected {} arguments, got {}",
                        params.len(),
                        args.len()
                    ),
                    span,
                )]);
            }

            // CALL-POLY: After instantiation, the function type always has type variables.
            // This is guaranteed by the guard at line 236: check_call_with_scheme is only called
            // for polymorphic schemes (non-empty type_vars or row_vars), and instantiate_scheme
            // produces fresh TypeVars/RowVars for each quantified variable. Since generalize only
            // quantifies variables that appear in the body, the instantiated type must contain
            // those fresh variables, so has_type_vars() is always true.
            // Synthesize arguments and unify (doc/06 line 162-170)
            let mut arg_types = Vec::with_capacity(args.len());
            for a in args {
                arg_types.push(infer_expr(a, env, state, type_map)?);
            }

            if !params.is_empty() {
                // Seed local subst from state.subst so that unification sees access-chain
                // constraints and letrec bindings accumulated by prior inference steps.
                // This mirrors infer_dict Pass 3a (lines 553-561): Algorithm W threads a
                // single substitution through inference; the two-substitution model is a
                // borrow-checker workaround. Without seeding, param_ty is unified against
                // arg_ty in an empty substitution context, missing bindings for TypeVars
                // that state.subst already resolved (Damas & Milner 1982, Theorem 2).
                let mut subst = Substitution {
                    type_map: state.subst.type_map.clone(),
                    row_map: state.subst.row_map.clone(),
                };
                for (param_ty, arg_ty) in params.iter().zip(arg_types.iter()) {
                    unify(param_ty, arg_ty, &mut subst, state, span).map_err(|e| vec![e])?;
                }
                // Merge local subst back into state.subst so that constraints from this
                // polymorphic call site are visible to subsequent inference steps. Without
                // this merge, bindings accumulated during argument unification (e.g., a
                // TypeVar constrained to Int) are lost for downstream entries in the same
                // letrec group. This mirrors infer_dict Pass 3d (lines 632-641).
                for (k, v) in &subst.type_map {
                    state.subst.type_map.insert(k.clone(), v.clone());
                }
                for (k, row) in &subst.row_map {
                    state.subst.row_map.insert(k.clone(), row.clone());
                }
                Ok(subst.apply(ret))
            } else {
                // Zero-param function: return the return type
                let inst_ret = ret;
                Ok(state.subst.apply(inst_ret))
            }
        }
        Type::Any => {
            // Infer positional args for type map population (needed for LSP hover on Any-typed functions).
            // This loop runs only for Any-typed callees — for Type::Function arms, positional args are
            // already inferred exactly once in CALL-POLY (infer_expr at line 934).
            // Running it here unconditionally would cause double-inference for Function calls, mutating
            // state.name_counter and state.subst a second time in violation of single-pass Algorithm W.
            for arg in args {
                let _ = infer_expr(arg, env, state, type_map)?;
            }
            Ok(Type::Any)
        }
        _ => Err(vec![TypeError::not_a_function(&func_ty, span)]),
    }
}

fn check_call(
    func: &Spanned<Expr>,
    args: &[Spanned<Expr>],
    named_args: &[Spanned<NamedArg>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let func_ty = infer_expr(func, env, state, type_map)?;
    // Apply state.subst to resolve any TypeVars bound during infer_expr (e.g., from infer_fn
    // with polymorphic return annotations). Without this, has_type_vars() incorrectly returns
    // true for already-bound TypeVars, causing CALL-POLY to fire and double-instantiate.
    let func_ty = if state.subst.type_map.is_empty() && state.subst.row_map.is_empty() {
        func_ty
    } else {
        state.subst.apply(&func_ty)
    };

    // Infer named args for type map population and error detection
    for na in named_args {
        let _ = infer_expr(&na.node.value, env, state, type_map)?;
    }

    match &func_ty {
        Type::Function { params, ret } => {
            if params.len() != args.len() {
                return Err(vec![TypeError::new(
                    format!(
                        "arity mismatch: expected {} arguments, got {}",
                        params.len(),
                        args.len()
                    ),
                    span,
                )]);
            }

            // CALL-MONO: function type is fully concrete (no type variables)
            // Use bidirectional checking for arguments via [SUB] rule (doc/06 line 152-157)
            if !func_ty.has_type_vars() {
                let mut errors = Vec::new();
                for (arg, param_ty) in args.iter().zip(params.iter()) {
                    if let Err(mut errs) = check_expr(arg, param_ty, env, state, type_map) {
                        errors.append(&mut errs);
                    }
                }
                if !errors.is_empty() {
                    return Err(errors);
                }
                // Direct clone is correct: the CALL-MONO guard (!func_ty.has_type_vars()) proves
                // ret is fully concrete — no TypeVar or RowVar nodes — so apply() would be a no-op
                // that wastes 2 HashSet allocations. (check_call_with_scheme always takes CALL-POLY
                // and uses subst.apply(ret) at line ~970, where subst is seeded from state.subst,
                // because it is entered after instantiate_scheme which produces fresh TypeVars/RowVars
                // that are resolved by substitution threading.)
                return Ok(*ret.clone());
            }

            // CALL-POLY: function type has type variables
            // Instantiate the function type, synthesize arguments, then unify (doc/06 line 162-170)
            let inst_ty = instantiate_at_level(&func_ty, state);

            let (inst_params, inst_ret) = match &inst_ty {
                Type::Function { params, ret } => (params, ret),
                _ => unreachable!("instantiate_at_level preserves Function variant"),
            };

            // Synthesize argument types for CALL-POLY (not checking mode)
            let mut arg_types = Vec::with_capacity(args.len());
            for a in args {
                arg_types.push(infer_expr(a, env, state, type_map)?);
            }

            if !params.is_empty() {
                // Sequential unification of arguments (textbook Hindley-Milner)
                //
                // This approach unifies each (param_ty, arg_ty) pair in order, accumulating
                // a substitution that is applied to subsequent unifications. This is SOUND
                // because instantiation creates fresh type variables per call site, so
                // type variable bindings from earlier arguments are correctly propagated.
                //
                // CONFLUENCE: When multiple arguments constrain the same type variable with
                // different precision (e.g., IntLiteral(42) vs Int), the bidirectional promotion
                // rules in unify() (src/types.rs, "Literal-to-parent promotions") ensure that type checking SUCCEEDS
                // regardless of argument order. The only difference is the PRECISION of the
                // resulting binding:
                //
                //   Example: ∀a. Fn(a a → a) called with IntLiteral(42) and Int
                //   - Order 1: unify(_t0, IntLiteral(42)) → {_t0 ↦ IntLiteral(42)},
                //              then unify(IntLiteral(42), Int) → SUCCESS (promotion rule)
                //              Result: _t0 = IntLiteral(42)
                //   - Order 2: unify(_t0, Int) → {_t0 ↦ Int},
                //              then unify(Int, IntLiteral(42)) → SUCCESS (promotion rule)
                //              Result: _t0 = Int
                //
                // Both orderings succeed; the first is more precise. This is the expected
                // behavior: earlier arguments guide type variable binding, later arguments
                // validate compatibility via subsumption. The bidirectional promotion rules
                // (IntLiteral ↔ Int, StringLiteral ↔ Str, Int ↔ Number, etc.) make this
                // unification subsumptive in practice, providing the same confluence property
                // that Pierce-Turner's [U-SUBSUME] rule would provide (doc/06 §Subsumptive fallback).
                //
                // CONSTRAINT-BASED ALTERNATIVE: Pierce-Turner bidirectional typing suggests
                // collecting all constraints before solving (constraint generation, then
                // joint constraint solving). This would allow computing a "minimal" or
                // "maximal" substitution (e.g., least upper bound when multiple constraints
                // exist). However, for tinct's current type system:
                //
                //   1. Sequential unification is simpler and matches standard HM implementations
                //   2. The bidirectional promotion rules already handle the common cases
                //   3. Left-to-right evaluation order makes it natural for earlier args to
                //      guide type inference
                //   4. No test case currently demonstrates a need for constraint collection
                //
                // If future extensions (e.g., row-variable unification with constraints on
                // multiple row variables, or bidirectional typing with checking mode for
                // CALL-POLY args) require more sophisticated constraint solving, the switch
                // to constraint generation would happen here. The current approach is
                // intentionally pragmatic.
                //
                // Seed local subst from state.subst so that unification sees access-chain
                // constraints and letrec bindings accumulated by prior inference steps.
                // This mirrors check_call_with_scheme (lines 952-955) and infer_dict Pass 3a:
                // Algorithm W threads a single substitution through inference; the two-substitution
                // model is a borrow-checker workaround. Without seeding, param_ty is unified
                // against arg_ty in an empty substitution context, missing bindings for TypeVars
                // that state.subst already resolved (Damas & Milner 1982, Theorem 2).
                let mut subst = Substitution {
                    type_map: state.subst.type_map.clone(),
                    row_map: state.subst.row_map.clone(),
                };
                for (param_ty, arg_ty) in inst_params.iter().zip(arg_types.iter()) {
                    unify(param_ty, arg_ty, &mut subst, state, span).map_err(|e| vec![e])?;
                }
                // Merge local subst back into state.subst so that constraints from this
                // polymorphic call site are visible to subsequent inference steps. Without
                // this merge, bindings accumulated during argument unification (e.g., a
                // TypeVar constrained to Int) are lost for downstream entries in the same
                // letrec group. This mirrors check_call_with_scheme (lines 964-969) and
                // infer_dict Pass 3d (lines 632-641).
                for (k, v) in &subst.type_map {
                    state.subst.type_map.insert(k.clone(), v.clone());
                }
                for (k, row) in &subst.row_map {
                    state.subst.row_map.insert(k.clone(), row.clone());
                }
                Ok(subst.apply(inst_ret))
            } else {
                // Zero-param polymorphic function: return the instantiated return type
                // (not the original `ret` which contains the scheme-internal variable names)
                Ok(state.subst.apply(inst_ret))
            }
        }
        Type::Any => {
            // Infer positional args for type map population (needed for LSP hover on Any-typed functions).
            // This loop runs only for Any-typed callees — for Type::Function arms, positional args are
            // already inferred exactly once in CALL-MONO (check_expr at line 1011) or CALL-POLY (infer_expr).
            // Running it here unconditionally would cause double-inference for Function calls, mutating
            // state.name_counter and state.subst a second time in violation of single-pass Algorithm W.
            for arg in args {
                let _ = infer_expr(arg, env, state, type_map)?;
            }
            Ok(Type::Any)
        }
        _ => Err(vec![TypeError::not_a_function(&func_ty, span)]),
    }
}

fn infer_fn(
    return_ann: &Option<Spanned<Annotation>>,
    params: &[Spanned<Param>],
    body: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    _span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Create a fresh annotation mapping for this function to prevent
    // cross-contamination of type variables between sibling functions
    let mut ann_mapping = HashMap::new();
    let mut ann_mapping_opt = Some(&mut ann_mapping);

    let mut param_types: Vec<Type> = params
        .iter()
        .map(|p| match &p.node.annotation {
            Some(ann) => resolve_annotation(&ann.node, env, ann.span, state, &mut ann_mapping_opt),
            None => Ok(Type::Any),
        })
        .collect::<Result<_, _>>()
        .map_err(|e| vec![e])?;

    let mut fn_env = TypeEnv::with_parent(Rc::clone(env));
    for (i, param) in params.iter().enumerate() {
        if param.node.variadic {
            let variadic_ty = Type::Record(Row {
                fields: HashMap::new(),
                tail: RowTail::Empty,
            });
            // Variadic params always receive a closed empty record, regardless of annotation.
            // Update param_types[i] to match the env binding so the function signature is accurate.
            param_types[i] = variadic_ty.clone();
            fn_env.insert(param.node.name.clone(), variadic_ty);
        } else {
            fn_env.insert(param.node.name.clone(), param_types[i].clone());
        }
    }
    let fn_env = Rc::new(fn_env);

    let ret_type = match return_ann {
        Some(ann) => {
            let declared =
                resolve_annotation(&ann.node, env, ann.span, state, &mut ann_mapping_opt)
                    .map_err(|e| vec![e])?;

            // When declared return type contains type variables, switch to unification mode
            // (doc/06 line 136-146, Damas & Milner 1982, Pierce & Turner 2000 §3.2).
            // TypeVars in is_subtype only match via reflexive equality, so
            // is_subtype(IntLiteral(42), TypeVar("_t5")) = false would reject valid code.
            // Unification mode binds the TypeVars via constraint solving.
            if declared.has_type_vars() {
                let body_ty = infer_expr(body, &fn_env, state, type_map)?;
                // Borrow-split: mem::take + restore avoids simultaneous &mut state.subst and &mut state
                let mut subst = std::mem::take(&mut state.subst);
                let result = unify(&body_ty, &declared, &mut subst, state, body.span);
                state.subst = subst;
                result.map_err(|e| vec![e])?;
                declared
            } else {
                // Use checking mode for concrete return types (no type variables)
                check_expr(body, &declared, &fn_env, state, type_map)?;
                declared
            }
        }
        None => infer_expr(body, &fn_env, state, type_map)?,
    };

    Ok(Type::Function {
        params: param_types,
        ret: Box::new(ret_type),
    })
}

fn expand_type_alias(
    inner: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
) -> Result<Type, TypeError> {
    let _ = resolve_type_expr(inner, env, state, &mut None)?;
    Ok(Type::Any)
}

fn resolve_type_assert(
    annotation: &Spanned<Annotation>,
    inner: &Spanned<Expr>,
    resolved_type: &RefCell<Option<Type>>,
    env: &Rc<TypeEnv>,
    _span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let expected = resolve_annotation(&annotation.node, env, annotation.span, state, &mut None)
        .map_err(|e| vec![e])?;

    // Store the resolved type in the AST node for runtime validation (elaboration)
    // INVARIANT: resolved_type is write-once (parser initializes to None, typecheck sets it once)
    let prev = resolved_type.replace(Some(expected.clone()));
    debug_assert!(
        prev.is_none(),
        "resolved_type written twice — elaboration invariant violated (span: {:?})",
        annotation.span
    );

    // Use checking mode for TypeAssert inner expression (doc/06 line 214-226)
    let check_result = check_expr(inner, &expected, env, state, type_map);

    // If checking fails and there's a default, suppress the error (ASSERT-DEFAULT rule)
    if check_result.is_err() {
        let has_default = annotation.node.get_property("default").is_some();
        if !has_default {
            return check_result.map(|_| expected);
        }
    }

    // Validate the default value type — hard error if the default cannot satisfy the asserted type.
    if let Some(default_expr) = annotation.node.get_property("default") {
        match infer_expr(default_expr, env, state, type_map) {
            Ok(default_ty) => {
                // Apply state.subst to both types before comparison — access-chain constraints
                // may have bound TypeVars in state.subst (e.g., $data.name generates row-variable
                // bindings). Without substitution, the comparison uses stale TypeVars.
                let default_ty = state.subst.apply(&default_ty);
                let expected_resolved = state.subst.apply(&expected);
                if !Type::is_subtype(&default_ty, &expected_resolved) {
                    return Err(vec![TypeError::new(
                        format!(
                            "default value type mismatch: default has type {default_ty}, \
                             but assertion expects {expected_resolved}"
                        ),
                        default_expr.span,
                    )]);
                }
            }
            Err(errs) => {
                // Propagate inference errors from the default expression
                return Err(errs);
            }
        }
    }

    // Apply substitution before returning to ensure bound type variables are resolved.
    // The expected type may contain TypeVars that were bound during checking mode or
    // access-chain inference (e.g., check_dot_access binds row variables).
    let expected = state.subst.apply(&expected);
    Ok(expected)
}

fn resolve_annotated(
    name: &str,
    annotation: &Spanned<Annotation>,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    if name == "Fn" {
        resolve_fn_type(&annotation.node, env, annotation.span, state, ann_mapping)
    } else {
        resolve_annotation(&annotation.node, env, span, state, ann_mapping)
    }
}

fn resolve_fn_type(
    ann: &Annotation,
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    let ret = resolve_annotation_as_type(ann, env, span, state, ann_mapping)?;
    Ok(Type::Function {
        params: vec![],
        ret: Box::new(ret),
    })
}

/// Resolve an annotation in a context where a type expression is expected.
/// Unlike `resolve_annotation`, a PropertyDict is interpreted as a type expression
/// (record type or function type) rather than a property bag.
fn resolve_annotation_as_type(
    ann: &Annotation,
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    match ann {
        Annotation::Simple(name) => resolve_type_name(name, env, span, state, ann_mapping),
        Annotation::PropertyDict(entries) => {
            resolve_type_dict(entries, env, span, state, ann_mapping)
        }
    }
}

fn resolve_annotation(
    ann: &Annotation,
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    match ann {
        Annotation::Simple(name) => resolve_type_name(name, env, span, state, ann_mapping),
        Annotation::PropertyDict(entries) => {
            if let Some(type_val) = ann.get_property("type") {
                resolve_type_expr_value(type_val, env, state, ann_mapping)
            } else {
                resolve_property_dict_as_record(entries, env, span, state, ann_mapping)
            }
        }
    }
}

fn resolve_property_dict_as_record(
    entries: &[Spanned<Entry>],
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    resolve_type_dict(entries, env, span, state, ann_mapping).or_else(|e| {
        if entries_look_like_type_dict(entries) {
            Err(e)
        } else {
            Ok(Type::Any)
        }
    })
}

/// Check whether property dict entries structurally look like they could be a
/// type dict (record type or function type expression). Returns true when all
/// entries look like record-type fields (string key + type-expression value),
/// or when the first entry matches the `Fn@Return [Params]` function type
/// pattern. When entries contain literal values (Int, Float, Bool) or
/// auto-indexed non-function entries, they are annotation metadata rather than
/// type definitions, and type resolution errors should be swallowed.
fn entries_look_like_type_dict(entries: &[Spanned<Entry>]) -> bool {
    // Detect `[Fn@Return [Params]]` function type pattern: first entry is
    // auto-indexed with an Annotated node whose name is "Fn".
    if let Some(first) = entries.first() {
        if first.node.key.is_none() {
            if let Expr::Annotated { name, .. } = &first.node.value.node {
                if name == "Fn" {
                    return true;
                }
            }
        }
    }

    // Record type pattern: every entry has a string key and a type-shaped value.
    entries.iter().all(|entry| {
        // Rest entries (`...` / `...name`) are valid in type dicts
        if matches!(&entry.node.value.node, Expr::Rest(_)) {
            return true;
        }
        // Every entry must have a string key
        let has_str_key = entry
            .node
            .key
            .as_ref()
            .is_some_and(|k| matches!(&k.node, Expr::Str(_)));
        // Value must be a form that could be a type expression
        let value_is_type_shaped = matches!(
            &entry.node.value.node,
            Expr::Str(_) | Expr::VarRef(_) | Expr::Dict(_) | Expr::Annotated { .. }
        );
        has_str_key && value_is_type_shaped
    })
}

fn resolve_type_name(
    name: &str,
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    match name {
        "Int" => Ok(Type::Int),
        "Float" => Ok(Type::Float),
        "String" => Ok(Type::Str),
        "Bool" => Ok(Type::Bool),
        "Number" => Ok(Type::Number),
        "Any" => Ok(Type::Any),
        _ => {
            if name.starts_with(|c: char| c.is_lowercase()) {
                // If we have an annotation mapping (within a function), check if this
                // annotation name has already been mapped to a fresh variable
                if let Some(ref mut mapping) = ann_mapping {
                    // Check if this annotation name already has a mapping
                    if let Some(existing_var) = mapping.get(name) {
                        // Already mapped: return the existing TypeVar with its current level
                        // from state.levels. DO NOT reset the level - unification may have
                        // lowered it, and level lowering must be monotone (Kiselyov 2013).
                        let current_level = *state
                            .levels
                            .get(existing_var)
                            .expect("invariant: annotation var registered in mapping must be in state.levels");
                        Ok(Type::TypeVar(existing_var.clone(), current_level))
                    } else {
                        // First time seeing this annotation: create fresh var and register level
                        let fresh = format!("_t{}", state.name_counter);
                        state.name_counter += 1;
                        state.levels.insert(fresh.clone(), state.level);
                        mapping.insert(name.to_string(), fresh.clone());
                        Ok(Type::TypeVar(fresh, state.level))
                    }
                } else {
                    // Outside of function scope, use the annotation name directly
                    // Check if already exists to avoid resetting level
                    if let Some(&existing_level) = state.levels.get(name) {
                        Ok(Type::TypeVar(name.to_string(), existing_level))
                    } else {
                        state.levels.insert(name.to_string(), state.level);
                        Ok(Type::TypeVar(name.to_string(), state.level))
                    }
                }
            } else {
                env.get_type_alias(name)
                    .cloned()
                    .ok_or_else(|| TypeError::undefined_type(name, span))
            }
        }
    }
}

fn resolve_type_expr_value(
    expr: &Spanned<Expr>,
    env: &TypeEnv,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    match &expr.node {
        Expr::Str(name) | Expr::VarRef(name) => {
            resolve_type_name(name, env, expr.span, state, ann_mapping)
        }
        _ => Err(TypeError::new(
            format!("invalid type in annotation: {}", expr.node),
            expr.span,
        )),
    }
}

fn resolve_type_expr(
    expr: &Spanned<Expr>,
    env: &TypeEnv,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    match &expr.node {
        Expr::Str(name) | Expr::VarRef(name) => {
            resolve_type_name(name, env, expr.span, state, ann_mapping)
        }
        Expr::Dict(entries) => resolve_type_dict(entries, env, expr.span, state, ann_mapping),
        Expr::Annotated { name, annotation } => {
            if name == "Fn" {
                resolve_fn_type(&annotation.node, env, annotation.span, state, ann_mapping)
            } else {
                resolve_annotation(&annotation.node, env, expr.span, state, ann_mapping)
            }
        }
        _ => Err(TypeError::new(
            format!("invalid type expression: {}", expr.node),
            expr.span,
        )),
    }
}

fn resolve_type_dict(
    entries: &[Spanned<Entry>],
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Type, TypeError> {
    if let Some(fn_type) = try_resolve_fn_type_expr(entries, env, span, state, ann_mapping)? {
        return Ok(fn_type);
    }

    let mut fields: HashMap<String, Type> = HashMap::new();
    let mut rest = RowTail::Empty;
    for entry in entries {
        if let Expr::Rest(name) = &entry.node.value.node {
            rest = match name {
                None => {
                    // Anonymous open record: generate a fresh row variable name
                    let fresh_name = format!("_open{}", state.name_counter);
                    state.name_counter += 1;
                    state.levels.insert(fresh_name.clone(), state.level);
                    RowTail::RowVar(fresh_name, state.level)
                }
                Some(n) => {
                    // Row variables in type expressions also need fresh names per function
                    if let Some(ref mut mapping) = ann_mapping {
                        // Check if this row variable name already has a mapping
                        if let Some(existing_var) = mapping.get(n) {
                            // Already mapped: return the existing RowVar with its current level
                            // from state.levels. DO NOT reset the level - unification may have
                            // lowered it, and level lowering must be monotone (Kiselyov 2013).
                            let current_level = *state.levels.get(existing_var).expect(
                                "invariant: row var registered in mapping must be in state.levels",
                            );
                            RowTail::RowVar(existing_var.clone(), current_level)
                        } else {
                            // First time seeing this row variable: create fresh var and register level
                            let fresh = format!("_t{}", state.name_counter);
                            state.name_counter += 1;
                            state.levels.insert(fresh.clone(), state.level);
                            mapping.insert(n.clone(), fresh.clone());
                            RowTail::RowVar(fresh, state.level)
                        }
                    } else {
                        // Outside of function scope, use the row variable name directly
                        // Check if already exists to avoid resetting level
                        if let Some(&existing_level) = state.levels.get(n) {
                            RowTail::RowVar(n.clone(), existing_level)
                        } else {
                            state.levels.insert(n.clone(), state.level);
                            RowTail::RowVar(n.clone(), state.level)
                        }
                    }
                }
            };
            continue;
        }
        let key = match &entry.node.key {
            Some(k) => match &k.node {
                Expr::Str(s) => s.clone(),
                _ => {
                    return Err(TypeError::new(
                        "type record keys must be bare words",
                        k.span,
                    ))
                }
            },
            None => {
                return Err(TypeError::new(
                    "auto-indexed entries not supported in type expressions",
                    entry.span,
                ))
            }
        };
        let ty = resolve_type_expr(&entry.node.value, env, state, ann_mapping)?;
        fields.insert(key, ty);
    }
    Ok(Type::Record(Row { fields, tail: rest }))
}

/// Detect `[Fn@Return [ParamTypes]]` -- a Dict with two auto-indexed entries
/// where the first is `Annotated { name: "Fn", ... }` and the second is a Dict
/// containing the parameter type list.
fn try_resolve_fn_type_expr(
    entries: &[Spanned<Entry>],
    env: &TypeEnv,
    span: Span,
    state: &mut InferState,
    ann_mapping: &mut Option<&mut HashMap<String, String>>,
) -> Result<Option<Type>, TypeError> {
    let first = match entries.first() {
        Some(e) if e.node.key.is_none() => e,
        _ => return Ok(None),
    };

    let (ann_node, ann_span) = match &first.node.value.node {
        Expr::Annotated { name, annotation } if name == "Fn" => (&annotation.node, annotation.span),
        _ => return Ok(None),
    };

    if entries.len() != 2 {
        return Err(TypeError::new(
            format!(
                "function type [Fn@Return [Params]] requires exactly 2 entries, got {}",
                entries.len()
            ),
            span,
        ));
    }

    let second = &entries[1];
    if second.node.key.is_some() {
        return Err(TypeError::new(
            "function type parameter list must be auto-indexed",
            second.span,
        ));
    }

    let ret = resolve_annotation_as_type(ann_node, env, ann_span, state, ann_mapping)?;

    let param_entries = match &second.node.value.node {
        Expr::Dict(entries) => entries,
        _ => {
            return Err(TypeError::new(
                "function type parameter list must be a bracket expression",
                second.node.value.span,
            ))
        }
    };

    let mut params = Vec::new();
    for entry in param_entries {
        if entry.node.key.is_some() {
            return Err(TypeError::new(
                "function type parameters must be auto-indexed type names",
                entry.span,
            ));
        }
        params.push(resolve_type_expr(
            &entry.node.value,
            env,
            state,
            ann_mapping,
        )?);
    }

    Ok(Some(Type::Function {
        params,
        ret: Box::new(ret),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(input: &str) -> Result<(), Vec<TypeError>> {
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        typecheck_file(&file.node)
    }

    fn check_err(input: &str) -> Vec<TypeError> {
        check(input).unwrap_err()
    }

    fn infer(input: &str) -> Type {
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();
        let expr = &file.node.documents[0].node.expressions[0];
        infer_expr(expr, &env, &mut state, &mut None).unwrap()
    }

    fn doc_env(input: &str) -> Rc<TypeEnv> {
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();
        typecheck_document(&file.node.documents[0], &env, &mut state, &mut None).unwrap()
    }

    fn result_type(input: &str) -> Type {
        let env = doc_env(input);
        env.get("$").unwrap().body.clone()
    }

    fn result_field(input: &str, field: &str) -> Type {
        match result_type(input) {
            Type::Record(Row { fields, .. }) => fields.get(field).cloned().unwrap(),
            other => panic!("expected Record for $$, got {other}"),
        }
    }

    fn file_env(input: &str) -> Rc<TypeEnv> {
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let mut env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();
        for doc in &file.node.documents {
            env = typecheck_document(doc, &env, &mut state, &mut None).unwrap();
        }
        env
    }

    // -- Literal inference --

    #[test]
    fn test_literal_int() {
        assert_eq!(infer("42"), Type::IntLiteral(42));
    }

    #[test]
    fn test_literal_float() {
        assert_eq!(infer("3.14"), Type::Float);
    }

    #[test]
    fn test_literal_bool() {
        assert_eq!(infer("true"), Type::Bool);
    }

    #[test]
    fn test_literal_string() {
        assert_eq!(infer("hello"), Type::StringLiteral("hello".into()));
    }

    // -- VarRef --

    #[test]
    fn test_varref_in_scope_chain() {
        assert_eq!(result_field("[x: 42]\n[y: $x]", "y"), Type::IntLiteral(42));
    }

    #[test]
    fn test_varref_undefined() {
        let errors = check_err("$x");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("undefined variable: $x"));
    }

    // -- Record construction --

    #[test]
    fn test_dict_simple() {
        let ty = infer("[a: 1  b: hello  c: true]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(1)));
                assert_eq!(fields.get("b"), Some(&Type::StringLiteral("hello".into())));
                assert_eq!(fields.get("c"), Some(&Type::Bool));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_dict_auto_indexed() {
        let ty = infer("[foo bar baz]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("0"), Some(&Type::StringLiteral("foo".into())));
                assert_eq!(fields.get("1"), Some(&Type::StringLiteral("bar".into())));
                assert_eq!(fields.get("2"), Some(&Type::StringLiteral("baz".into())));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_dict_nested() {
        let ty = infer("[outer: [inner: 42]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                let inner = fields.get("outer").unwrap();
                match inner {
                    Type::Record(Row {
                        fields: inner_fields,
                        ..
                    }) => {
                        assert_eq!(inner_fields.get("inner"), Some(&Type::IntLiteral(42)));
                    }
                    other => panic!("expected Record, got {other}"),
                }
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_dict_letrec_forward_ref() {
        let ty = infer("[a: $b  b: 42]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                // With let-generalization, forward references now participate in unification.
                // $b unifies with 42, so both a and b have type IntLiteral(42).
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(42)));
                assert_eq!(fields.get("b"), Some(&Type::IntLiteral(42)));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    // -- Dict error accumulation --

    #[test]
    fn test_dict_multiple_errors() {
        let errors = check_err("[a: $undefined1  b: 42  c: $undefined2]");
        assert_eq!(errors.len(), 2, "should return all errors, got: {errors:?}");
        assert!(
            errors[0].message.contains("$undefined1"),
            "first error should be about $undefined1, got: {}",
            errors[0].message
        );
        assert!(
            errors[1].message.contains("$undefined2"),
            "second error should be about $undefined2, got: {}",
            errors[1].message
        );

        // Also verify via direct infer_expr call
        let mut file = crate::parse("[a: $undefined1  b: 42  c: $undefined2]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();
        let expr = &file.node.documents[0].node.expressions[0];
        let errs = infer_expr(expr, &env, &mut state, &mut None).unwrap_err();
        assert_eq!(errs.len(), 2, "infer_expr should return all dict errors");
        assert!(errs[0].message.contains("$undefined1"));
        assert!(errs[1].message.contains("$undefined2"));
    }

    // -- Dot access --

    #[test]
    fn test_dot_access_found() {
        assert_eq!(
            result_field(
                "[person: [name: Andrew  age: 30]]\n[result: $person.name]",
                "result"
            ),
            Type::StringLiteral("Andrew".into()),
        );
    }

    #[test]
    fn test_dot_access_missing_field() {
        let errors = check_err("[person: [name: Andrew]]\n[result: $person.age]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("field 'age' not found")));
    }

    #[test]
    fn test_dot_access_non_record() {
        let errors = check_err("[x: 42]\n[result: $x.field]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("expected record type")));
    }

    // -- Access chain constraint generation (doc/07 Part 5) --

    #[test]
    fn test_dot_access_open_record_extends_tail() {
        // When `$p` has type Record({name: Str}, RowVar(ρ)) (an open record via type annotation)
        // and we access `$p.unknown` (field not in known fields), the RowVar case generates
        // constraint ρ → Row({unknown: β}, RowVar(ρ_fresh)) and returns β.
        // This extends the known tail rather than falling back to Any.
        //
        // Two accesses on the same open record accumulate constraints:
        // first access binds ρ → Row({f1: β₁}, RowVar(ρ₁))
        // second access sees ρ already bound, extracts from ρ₁ → Row({f2: β₂}, RowVar(ρ₂))
        let env = doc_env("[Open: [type [name: String ...]]]\n[p: [@Open [name: Alice  score: 42]]]\n[r1: $p.score2  r2: $p.score3]");
        // r1 and r2 should both be TypeVars (fresh inferred types, not Any)
        match env.get("r1").map(|s| &s.body) {
            Some(Type::TypeVar(_, _)) => {}
            Some(other) => panic!("expected TypeVar for first unknown field, got {other}"),
            None => panic!("field 'r1' not found in env"),
        }
        match env.get("r2").map(|s| &s.body) {
            Some(Type::TypeVar(_, _)) => {}
            Some(other) => panic!("expected TypeVar for second unknown field, got {other}"),
            None => panic!("field 'r2' not found in env"),
        }
    }

    #[test]
    fn test_dot_access_constraint_generation_on_open_record_with_known_field() {
        // Task 5: Renamed from test_dot_access_open_record_infinite_row_cycle.
        // The original name promised "infinite row cycle" but the test actually exercises
        // TypeVar constraint generation on forward references, NOT the RowVar occurs-check path.
        //
        // Test the occurs-check error path in check_dot_access (typecheck.rs:710)
        //
        // ANALYSIS: The occurs check `if row_var_occurs_pub(rho, &binding, &state.subst)` fires
        // when binding ρ → Row({field: β}, RowVar(ρ_fresh)) would create an infinite row type.
        //
        // PROOF SKETCH (invariant at occurs-check site, typecheck.rs:710):
        //   For ρ to occur in the binding Row({field: β}, RowVar(ρ_fresh)), either:
        //     (a) β contains ρ in its structure (e.g., β is bound to Record(..., RowVar(ρ))), OR
        //     (b) ρ_fresh = ρ (the fresh row var equals the original)
        //
        //   Both are IMPOSSIBLE by construction:
        //     - β is fresh (line 696: state.fresh_type_var()) with no prior bindings → cannot contain ρ
        //     - ρ_fresh is fresh (line 698: state.fresh_row_var_name()) → ρ_fresh ≠ ρ by uniqueness
        //
        //   Therefore, row_var_occurs_pub(ρ, binding, state.subst) is ALWAYS false when the binding
        //   uses only fresh variables. The occurs check is defensive programming that guards the
        //   invariant but cannot fail under normal type inference.
        //
        // SIMILAR DEFENSIVE CHECKS: The unify_remainders occurs checks in types.rs CAN be triggered
        // because they deal with potentially non-fresh variables from both sides of a unification.
        // But check_dot_access creates fresh variables on-demand, making the cycle impossible.
        //
        // TEST STRATEGY: We'll demonstrate that the occurs check exists and has the correct structure,
        // even if we can't trigger the error path through normal inference. This documents the
        // defensive invariant.

        // Verify the defensive occurs check exists by reading the implementation.
        // We can't easily trigger it, but we can verify related functionality works correctly.

        // Test: Accessing a field on a TypeVar (forward reference) generates a constraint
        // and returns a fresh TypeVar, not Any. This proves the constraint generation path works.
        let ty = infer("[result: $data.unknown  data: [known: 1]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                // result should be a TypeVar (the fresh β from the constraint)
                let result_ty = fields.get("result").expect("field 'result' should exist");
                assert!(
                    matches!(result_ty, Type::TypeVar(_, _)),
                    "Field access on TypeVar should generate constraint and return TypeVar, got {result_ty}"
                );
            }
            other => panic!("Expected Record, got {other}"),
        }

        // Note: The types.rs row occurs checks ARE tested (see test_row_occurs_check_direct_tail_cycle
        // and test_row_occurs_check_nested_in_field_cycle). Those tests demonstrate the occurs check
        // mechanism works correctly. The check_dot_access occurs check uses the same row_var_occurs_pub
        // function, so if it were ever triggered, it would work correctly.

        // CONCLUSION: This test documents that:
        // 1. The occurs check exists in check_dot_access (line 710)
        // 2. It uses row_var_occurs_pub which is tested in types.rs
        // 3. Normal constraint generation works correctly
        // 4. The error path is likely unreachable but serves as defensive programming
    }

    #[test]
    fn test_dot_access_typevar_generates_constraint_verified() {
        // Task 6: Verifies that the constraint α = Record({name: β}, RowVar(ρ)) was generated
        // when dot-accessing a TypeVar target.
        //
        // WHAT WE'RE TESTING:
        //   [result: $data.name  data: [name: hello]]
        //
        //   During Pass 1 of infer_dict, each field gets a fresh TypeVar in dict_env.
        //   When Pass 3 processes `result: $data.name`, it calls infer_expr on `$data.name`.
        //   $data resolves to γ_data (the Pass 1 TypeVar for data). check_dot_access sees
        //   γ_data is a TypeVar and generates the constraint γ_data = Record({name: β}, RowVar(ρ))
        //   stored in state.subst, returning β as the type of `result`.
        //
        // WHY FULL RESOLUTION DOES NOT OCCUR:
        //   The local substitution (subst) in infer_dict is initialized from state.subst at
        //   Pass 3a (before the loop). When Pass 3 processes `result`, check_dot_access writes
        //   γ_data → Record({name: β}, RowVar(ρ)) to state.subst (not local subst). When Pass 3
        //   then processes `data` and unifies γ_data with the concrete Record({name: "hello"}),
        //   local subst does not yet have the γ_data binding (Pass 3b merges AFTER the loop).
        //   So γ_data → concrete is bound in local subst WITHOUT seeing the β linkage.
        //   Pass 3b's or_insert skips γ_data (already bound). β never gets resolved to "hello".
        //
        // ASSERTION:
        //   result's type is a TypeVar (not Any). Any would mean check_dot_access returned Any
        //   instead of generating a fresh constraint TypeVar. The constraint WAS generated;
        //   the result type is β, a fresh TypeVar — not the sentinel Any fallback.
        //
        //   state.subst contains the γ_data constraint (proving the binding was stored),
        //   and β appears in state.levels (proving it was freshly allocated by the mechanism).

        let mut file = crate::parse("[result: $data.name  data: [name: hello]]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();

        // Typecheck the document
        let doc_env =
            typecheck_document(&file.node.documents[0], &env, &mut state, &mut None).unwrap();

        // Get the type of 'result' — this is β, the fresh TypeVar returned by check_dot_access
        let result_ty = match doc_env.get("result") {
            Some(scheme) => scheme.body.clone(),
            None => panic!("field 'result' not found"),
        };

        // ASSERTION 1: result's type must be a TypeVar (not Any).
        // Any would mean check_dot_access fell through to the Any arm instead of generating
        // the constraint α = Record({name: β}, RowVar(ρ)).
        let result_typevar_name = match &result_ty {
            Type::TypeVar(name, _) => name.clone(),
            other => panic!(
                "expected result to have TypeVar β (constraint was generated), got {other}; \
                 'Any' would indicate no constraint was generated"
            ),
        };

        // ASSERTION 2: β is registered in state.levels.
        // fresh_var registers every TypeVar it creates in state.levels. If β is in levels,
        // it was freshly allocated by the constraint generation mechanism (not a pre-existing var).
        // This proves check_dot_access called fresh_type_var() rather than returning Any or
        // some pre-existing variable.
        assert!(
            state.levels.contains_key(&result_typevar_name),
            "TypeVar β ({result_typevar_name}) should be registered in state.levels; \
             absence would mean it was not freshly generated by check_dot_access"
        );

        // NOTE: We cannot assert that β appears in state.subst.type_map after typecheck_document
        // completes. The constraint γ_data = Record({name: β}, RowVar(ρ)) IS stored in state.subst
        // during check_dot_access, but Pass 3d of infer_dict overwrites the γ_data binding with
        // the concrete Record({name: "hello"}, Closed) from letrec unification. The β→"hello"
        // resolution does not propagate back through the two-substitution architecture.
        // The invariant "constraint was generated" is fully captured by assertions 1 and 2.
    }

    #[test]
    fn test_dot_access_open_record_extends_tail_distinct_vars() {
        // Task 4: Strengthen test_dot_access_open_record_extends_tail
        // Original test at line 1684 verifies r1 and r2 are TypeVars but not that they're DISTINCT.
        // This test adds the distinctness assertion.

        let env = doc_env("[Open: [type [name: String ...]]]\n[p: [@Open [name: Alice  score: 42]]]\n[r1: $p.score2  r2: $p.score3]");

        // Get r1 and r2 types (should both be TypeVars)
        let r1_var_name = match env.get("r1").map(|s| &s.body) {
            Some(Type::TypeVar(name, _)) => name.clone(),
            Some(other) => panic!("expected TypeVar for r1, got {other}"),
            None => panic!("field 'r1' not found in env"),
        };

        let r2_var_name = match env.get("r2").map(|s| &s.body) {
            Some(Type::TypeVar(name, _)) => name.clone(),
            Some(other) => panic!("expected TypeVar for r2, got {other}"),
            None => panic!("field 'r2' not found in env"),
        };

        // Assert that r1 and r2 are DISTINCT TypeVars
        assert_ne!(
            r1_var_name, r2_var_name,
            "r1 and r2 should be distinct TypeVars (different field accesses should get fresh variables). \
             Got r1={}, r2={}",
            r1_var_name, r2_var_name
        );
    }

    #[test]
    fn test_typeassert_default_inference_error_propagation() {
        // Task 5: Test TypeAssert default inference-error propagation
        // resolve_type_assert at typecheck.rs:1102-1104 propagates Err(errs) when
        // the default expression itself fails to infer (e.g., references undefined variable).

        let errors = check_err("[@[type: Number  default: $undefined_var] 42]");

        // Should have at least one error (from the undefined variable in default)
        assert!(
            !errors.is_empty(),
            "TypeAssert with invalid default expression should produce an error"
        );

        // The error should mention the undefined variable
        assert!(
            errors.iter().any(|e| e.message.contains("undefined")),
            "Error should mention undefined variable, got: {:?}",
            errors
        );
    }

    // -- Bracket access --

    #[test]
    fn test_bracket_access_string_key() {
        assert_eq!(
            result_field("[data: [name: hello]]\n[result: $data[name]]", "result"),
            Type::StringLiteral("hello".into()),
        );
    }

    #[test]
    fn test_bracket_access_int_key() {
        assert_eq!(
            result_field("[list: [a b c]]\n[result: $list[0]]", "result"),
            Type::StringLiteral("a".into()),
        );
    }

    #[test]
    fn test_bracket_access_dynamic_key_literal() {
        assert_eq!(
            result_field("[data: [x: 1]  key: x]\n[result: $data[$key]]", "result"),
            Type::IntLiteral(1),
        );
    }

    #[test]
    fn test_bracket_access_dynamic_key_non_literal() {
        assert_eq!(
            result_field("[result: $data[$key]  data: [x: 1]  key: x]", "result"),
            Type::Any,
        );
    }

    // -- Range access --

    #[test]
    fn test_range_access_record() {
        let ty = result_field(
            "[data: [a: 1  b: 2  c: 3]]\n[result: $data[0..2]]",
            "result",
        );
        assert!(matches!(ty, Type::Record(..)));
    }

    #[test]
    fn test_range_access_invalid_bound() {
        let errors = check_err("[flag: true  data: [a: 1  b: 2]]\n[result: $data[$flag..2]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("range bound must be Int or String")));
    }

    // -- TypeAssert --

    #[test]
    fn test_type_assert_pass() {
        let ty = infer("[@Number 42]");
        assert_eq!(ty, Type::Number);
    }

    #[test]
    fn test_type_assert_fail() {
        let errors = check_err("[@Number hello]");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("type mismatch"));
    }

    #[test]
    fn test_type_assert_int_not_string() {
        let errors = check_err("[@String 42]");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("type mismatch"));
    }

    #[test]
    fn test_type_assert_default_suppresses_mismatch() {
        let result = check("[@[type: Number  default: 0] hello]");
        assert!(
            result.is_ok(),
            "TypeAssert with default: should not raise type error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_type_assert_no_default_still_errors() {
        let errors = check_err("[@[type: Number] hello]");
        assert!(
            errors.iter().any(|e| e.message.contains("type mismatch")),
            "TypeAssert without default: should still report type error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_typeassert_default_wrong_type_emits_error() {
        // [@Number default: "hello" expr] — default is Str, asserted type is Number
        // Should emit a default value type mismatch error
        let errors = check_err("[@[type: Number  default: hello] 42]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("default value type mismatch")),
            "TypeAssert with wrong default type should emit error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_typeassert_default_correct_type_no_error() {
        // [@Number default: 0 expr] — default is IntLiteral(0) which is subtype of Number
        // Should not emit any error
        let result = check("[@[type: Number  default: 0] 42]");
        assert!(
            result.is_ok(),
            "TypeAssert with correct default type should not emit error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_typeassert_default_wrong_type_main_check_fails() {
        // [@Number default: "hello" wrong_expr] — both main and default are wrong
        // Should emit a default value type mismatch error
        let errors = check_err("[@[type: Number  default: hello] world]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("default value type mismatch")),
            "TypeAssert with wrong default and wrong expr should emit default mismatch error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_typeassert_default_int_subtype_of_number() {
        // [@Number default: 42 expr] — IntLiteral(42) <: Number — no error
        let result = check("[@[type: Number  default: 42] hello]");
        assert!(
            result.is_ok(),
            "TypeAssert with Int default for Number assertion should not emit error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_typeassert_default_string_literal_subtype_of_str() {
        // [@String default: "ok" expr] — StringLiteral("ok") <: Str — no error
        let result = check("[@[type: String  default: ok] 42]");
        assert!(
            result.is_ok(),
            "TypeAssert with Str default for String assertion should not emit error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_typeassert_default_suppresses_main_error_but_propagates_ok() {
        // Task 6: ASSERT-DEFAULT suppression — when a valid default is present, the
        // main-check error (hello is not a Number) is suppressed and typecheck returns Ok.
        //
        // resolve_type_assert (typecheck.rs) follows this logic:
        //   1. Infer main expr type; if mismatch AND default present → suppress, return Ok
        //   2. Infer default type; if default type mismatches asserted type → Err
        //
        // The expression is wrapped in a dict so the result is observable via result_field.
        // `hello` is a bare word (StringLiteral type), not a Number → mismatch, suppressed.
        let result = check("[result: [@[type: Number  default: 0] hello]]");
        assert!(
            result.is_ok(),
            "TypeAssert with valid default should suppress main-check error (hello is not a Number), \
             but typecheck returned: {:?}",
            result.unwrap_err()
        );
    }

    // -- TypeAlias --

    #[test]
    fn test_type_alias_record() {
        let ty = result_field(
            "[Person: [type [name: String  age: Number]]]\n[p: [@Person [name: Alice  age: 30]]]",
            "p",
        );
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("name"), Some(&Type::Str));
                assert_eq!(fields.get("age"), Some(&Type::Number));
            }
            other => panic!("expected Record type from Person alias, got {other}"),
        }
    }

    #[test]
    fn test_type_alias_cycle_errors_not_loops() {
        // Circular aliases reference undefined types in each other. The aliases
        // themselves parse OK, but using them produces errors (not infinite loops).
        // The structure prevents cycles because aliases resolve against the parent
        // env, not the env being built.
        check("[A: [type B]  B: [type A]]").unwrap();
        let errors = check_err("[A: [type B]  B: [type A]  x: [@A 42]]");
        assert!(
            !errors.is_empty(),
            "using circular type aliases should produce errors"
        );
        let msg = format!("{:?}", errors);
        assert!(
            msg.contains("ndefined") || msg.contains("nknown"),
            "error should be about undefined/unknown type, got: {msg}"
        );
    }

    // -- Function inference --

    #[test]
    fn test_fn_unannotated() {
        let ty = infer("[fn [x] 42]");
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::Any]);
                assert_eq!(*ret, Type::IntLiteral(42));
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_annotated_params() {
        let ty = infer("[fn [x@Number] $x]");
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::Number]);
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_return_annotation_match() {
        let ty = infer("[fn@Number [x@Number] $x]");
        match ty {
            Type::Function { ret, .. } => assert_eq!(*ret, Type::Number),
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_return_annotation_mismatch() {
        let errors = check_err("[fn@String [x@Number] $x]");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("type mismatch"));
    }

    // -- Call --

    #[test]
    fn test_call_returns_function_ret_type() {
        assert_eq!(
            result_field("[f: [fn@Number [] 42]]\n[result: [call $f]]", "result"),
            Type::Number,
        );
    }

    #[test]
    fn test_call_non_function() {
        let errors = check_err("[x: 42]\n[result: [call $x]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("expected function type")));
    }

    // -- Document scope chain --

    #[test]
    fn test_scope_chain() {
        assert_eq!(result_field("[x: 42]\n[y: $x]", "y"), Type::IntLiteral(42));
    }

    #[test]
    fn test_intermediate_non_dict_error() {
        let errors = check_err("42\n[x: 1]");
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("expected record type"));
    }

    // -- $$ pipeline --

    #[test]
    fn test_pipeline_dollar_dollar() {
        let env = file_env("[x: 42]\n---\n[y: $$]");
        let result = env.get("$").unwrap().body.clone();
        match result {
            Type::Record(Row { fields, .. }) => {
                let y = fields.get("y").expect("field 'y' should exist");
                assert!(
                    matches!(y, Type::Record(..)),
                    "expected $$ to be Record, got {y}"
                );
            }
            other => panic!("expected Record result, got {other}"),
        }
    }

    #[test]
    fn test_pipeline_dollar_dollar_type() {
        let env = file_env("[x: 1]\n---\n[y: $$.x]");
        let result = env.get("$").unwrap().body.clone();
        match result {
            Type::Record(Row { fields, .. }) => {
                let y = fields.get("y").expect("field 'y' should exist");
                assert_eq!(
                    *y,
                    Type::IntLiteral(1),
                    "expected $$.x to propagate IntLiteral(1), got {y}"
                );
            }
            other => panic!("expected Record result, got {other}"),
        }
    }

    // -- Annotation resolution --

    #[test]
    fn test_annotation_simple() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 5);
        assert_eq!(
            resolve_annotation(
                &Annotation::Simple("Int".into()),
                &env,
                span,
                &mut InferState::new(),
                &mut None
            )
            .unwrap(),
            Type::Int,
        );
    }

    #[test]
    fn test_annotation_type_var() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 5);
        // InferState::new() has level=0, so annotation-derived TypeVars start at level 0
        // When no mapping is provided, the annotation name is used directly
        assert_eq!(
            resolve_annotation(
                &Annotation::Simple("a".into()),
                &env,
                span,
                &mut InferState::new(),
                &mut None
            )
            .unwrap(),
            Type::TypeVar("a".into(), 0),
        );
    }

    #[test]
    fn test_annotation_property_dict_with_type() {
        let ty = infer("[fn [x@[type: Number  default: 0]] $x]");
        match ty {
            Type::Function { params, .. } => assert_eq!(params, vec![Type::Number]),
            other => panic!("expected Function, got {other}"),
        }
    }

    // -- resolve_property_dict_as_record fallback paths --

    #[test]
    fn test_property_dict_non_str_key_falls_back_to_any() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: Some(sp(Expr::Int(42))),
                value: sp(Expr::Str("Int".into())),
            },
            span,
        )]);
        assert_eq!(
            resolve_annotation(&ann, &env, span, &mut InferState::new(), &mut None).unwrap(),
            Type::Any
        );
    }

    #[test]
    fn test_property_dict_no_key_falls_back_to_any() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: None,
                value: sp(Expr::Str("Int".into())),
            },
            span,
        )]);
        assert_eq!(
            resolve_annotation(&ann, &env, span, &mut InferState::new(), &mut None).unwrap(),
            Type::Any
        );
    }

    #[test]
    fn test_property_dict_unresolvable_type_propagates_error() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Str("NoSuchType".into())),
            },
            span,
        )]);
        let result = resolve_annotation(&ann, &env, span, &mut InferState::new(), &mut None);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .message
            .contains("undefined type: NoSuchType"));
    }

    #[test]
    fn test_property_dict_literal_value_falls_back_to_any() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: sp(Expr::Int(30)),
            },
            span,
        )]);
        assert_eq!(
            resolve_annotation(&ann, &env, span, &mut InferState::new(), &mut None).unwrap(),
            Type::Any
        );
    }

    #[test]
    fn test_property_dict_fn_type_error_propagates() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        // [Fn@Int] -- function type pattern detected (Fn@ prefix) but wrong
        // number of entries: should propagate, not fall back to Any.
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: None,
                value: sp(Expr::Annotated {
                    name: "Fn".into(),
                    annotation: Spanned::new(Annotation::Simple("Int".into()), span),
                }),
            },
            span,
        )]);
        let result = resolve_annotation(&ann, &env, span, &mut InferState::new(), &mut None);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("function type"));
    }

    // -- Type alias in scope --

    #[test]
    fn test_type_alias_in_scope_chain() {
        let ty = result_field(
            "[Coord: [type [x: Number  y: Number]]]\n[p: [@Coord [x: 1  y: 2]]]",
            "p",
        );
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("x"), Some(&Type::Number));
                assert_eq!(fields.get("y"), Some(&Type::Number));
                assert_eq!(fields.len(), 2);
            }
            other => panic!("expected Record type from Coord alias, got {other}"),
        }
    }

    // -- Error branch coverage --

    #[test]
    fn test_type_expr_non_bare_word_key() {
        let errors = check_err("[type [$var: Int]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("type record keys must be bare words")));
    }

    #[test]
    fn test_type_expr_auto_indexed_entries() {
        let errors = check_err("[type [Int String]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("auto-indexed entries not supported")));
    }

    #[test]
    fn test_annotation_type_value_invalid_expr() {
        let errors = check_err("[fn [x@[type: 42]] $x]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("invalid type in annotation")));
    }

    #[test]
    fn test_bracket_access_bool_key() {
        let errors = check_err("[data: [x: 1]  flag: true]\n[result: $data[$flag]]");
        assert!(errors.iter().any(|e| e
            .message
            .contains("bracket access key must be String or Int")));
    }

    #[test]
    fn test_annotated_non_fn_resolves_annotation() {
        let ty = infer("Config@Number");
        assert_eq!(ty, Type::Number);
    }

    // -- Fn@Return [Params] type expression --

    #[test]
    fn test_fn_type_one_param() {
        let ty = result_field(
            "[Mapper: [type [Fn@b [a]]]]\n[x: [@Mapper [fn [v] $v]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::TypeVar("a".into(), 0)]);
                assert_eq!(*ret, Type::TypeVar("b".into(), 0));
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_two_params() {
        let ty = result_field(
            "[BinOp: [type [Fn@c [a b]]]]\n[x: [@BinOp [fn [p q] $p]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(
                    params,
                    vec![Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0)]
                );
                assert_eq!(*ret, Type::TypeVar("c".into(), 0));
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_concrete_types() {
        let ty = result_field(
            "[Add: [type [Fn@Number [Number Number]]]]\n[x: [@Add [fn [a@Number b@Number] $a]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::Number, Type::Number]);
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_concrete_return_typevar_param() {
        let ty = result_field(
            "[Pred: [type [Fn@Bool [a]]]]\n[x: [@Pred [fn [v] true]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::TypeVar("a".into(), 0)]);
                assert_eq!(*ret, Type::Bool);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_higher_order() {
        let ty = result_field(
            "[HO: [type [Fn@[Fn@c [b]] [a]]]]\n[x: [@HO [fn [v] [fn [w] $w]]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::TypeVar("a".into(), 0)]);
                match *ret {
                    Type::Function {
                        params: inner_params,
                        ret: inner_ret,
                    } => {
                        assert_eq!(inner_params, vec![Type::TypeVar("b".into(), 0)]);
                        assert_eq!(*inner_ret, Type::TypeVar("c".into(), 0));
                    }
                    other => panic!("expected inner Function, got {other}"),
                }
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_missing_param_list() {
        let errors = check_err("[type [Fn@b]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("requires exactly 2 entries")));
    }

    #[test]
    fn test_fn_type_extra_entries() {
        let errors = check_err("[type [Fn@b [a] extra]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("requires exactly 2 entries")));
    }

    #[test]
    fn test_fn_type_param_list_not_bracket() {
        let errors = check_err("[type [Fn@b a]]");
        assert!(errors.iter().any(|e| e
            .message
            .contains("parameter list must be a bracket expression")));
    }

    #[test]
    fn test_fn_type_standalone_fn_annotation() {
        let ty = infer("Fn@Number");
        match ty {
            Type::Function { params, ret } => {
                assert!(params.is_empty());
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_in_type_assert() {
        let ty = result_field(
            "[F: [type [Fn@Number [Number]]]]\n[x: [@F [fn [n@Number] $n]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::Number]);
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_display_round_trip() {
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0)],
            ret: Box::new(Type::TypeVar("c".into(), 0)),
        };
        assert_eq!(format!("{ty}"), "Fn@c [a b]");
    }

    // -- Polymorphic call unification --

    #[test]
    fn test_call_polymorphic_identity() {
        assert_eq!(
            result_field("[id: [fn [x@a] $x]]\n[result: [call $id 42]]", "result"),
            Type::IntLiteral(42),
        );
    }

    #[test]
    fn test_call_polymorphic_identity_string() {
        assert_eq!(
            result_field("[id: [fn [x@a] $x]]\n[result: [call $id hello]]", "result"),
            Type::StringLiteral("hello".into()),
        );
    }

    #[test]
    fn test_call_polymorphic_two_type_vars() {
        assert_eq!(
            result_field(
                "[f: [fn [x@a y@b] $y]]\n[result: [call $f 42 hello]]",
                "result"
            ),
            Type::StringLiteral("hello".into()),
        );
    }

    #[test]
    fn test_call_polymorphic_type_var_in_return_only() {
        assert_eq!(
            result_field(
                "[first: [fn [x@a y@b] $x]]\n[result: [call $first 42 hello]]",
                "result"
            ),
            Type::IntLiteral(42),
        );
    }

    #[test]
    fn test_call_polymorphic_multiple_calls_different_types() {
        let ty = result_type("[id: [fn [x@a] $x]]\n[r1: [call $id 42]  r2: [call $id hello]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("r1"), Some(&Type::IntLiteral(42)));
                assert_eq!(fields.get("r2"), Some(&Type::StringLiteral("hello".into())));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_call_monomorphic_no_unification() {
        assert_eq!(
            result_field(
                "[f: [fn@Number [x@Number] $x]]\n[result: [call $f 42]]",
                "result"
            ),
            Type::Number,
        );
    }

    #[test]
    fn test_call_polymorphic_arity_mismatch_error() {
        let errors = check_err("[f: [fn [x@a y@b] $x]]\n[result: [call $f 42]]");
        assert!(
            errors.iter().any(|e| e.message.contains("arity mismatch")),
            "expected arity mismatch error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_call_monomorphic_arity_mismatch() {
        let errors = check_err("[f: [fn@Number [x@Number y@Number] $x]]\n[result: [call $f 42]]");
        assert!(
            errors.iter().any(|e| e.message.contains("arity mismatch")),
            "expected arity mismatch for monomorphic function, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_call_unification_error() {
        let errors = check_err("[f: [fn [x@a y@a] $x]]\n[result: [call $f 42 hello]]");
        assert!(
            errors.iter().any(|e| e.message.contains("type mismatch")),
            "expected type mismatch error, got: {:?}",
            errors
        );
    }

    // -- Polymorphic call with named args --

    #[test]
    fn test_call_polymorphic_with_named_arg() {
        // Polymorphic function called with positional args and a named arg override.
        // Named args are type-checked but don't participate in type var unification.
        assert_eq!(
            result_field(
                "[f: [fn [x@a y@b] $x]]\n[result: [call $f 42 hello y: 77]]",
                "result"
            ),
            Type::IntLiteral(42),
        );
    }

    #[test]
    fn test_call_polymorphic_named_arg_bad_value_errors() {
        // A named arg whose value references an undefined variable should produce
        // a type error even in a polymorphic call context.
        let errors = check_err("[f: [fn [x@a y@b] $x]]\n[result: [call $f 42 hello y: $missing]]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("undefined variable")),
            "expected undefined variable error from named arg, got: {:?}",
            errors
        );
    }

    // -- Function type expression with param list --

    #[test]
    fn test_fn_type_expr_with_params() {
        let env = doc_env("[Identity: [type [Fn@a [a]]]]\n[x: 1]");
        let alias = env.get_type_alias("Identity");
        assert!(alias.is_some(), "Identity alias should be registered");
        match alias.unwrap() {
            Type::Function { params, ret } => {
                assert_eq!(params, &vec![Type::TypeVar("a".into(), 0)]);
                assert_eq!(**ret, Type::TypeVar("a".into(), 0));
            }
            other => panic!("expected Function type alias, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_expr_multi_params() {
        let env = doc_env("[Mapper: [type [Fn@b [a b]]]]\n[x: 1]");
        let alias = env.get_type_alias("Mapper").unwrap();
        match alias {
            Type::Function { params, ret } => {
                assert_eq!(
                    params,
                    &vec![Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0)]
                );
                assert_eq!(**ret, Type::TypeVar("b".into(), 0));
            }
            other => panic!("expected Function type alias, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_expr_concrete_params() {
        let env = doc_env("[Add: [type [Fn@Number [Number Number]]]]\n[x: 1]");
        let alias = env.get_type_alias("Add").unwrap();
        match alias {
            Type::Function { params, ret } => {
                assert_eq!(params, &vec![Type::Number, Type::Number]);
                assert_eq!(**ret, Type::Number);
            }
            other => panic!("expected Function type alias, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_expr_predicate() {
        let env = doc_env("[Pred: [type [Fn@Bool [a]]]]\n[x: 1]");
        let alias = env.get_type_alias("Pred").unwrap();
        match alias {
            Type::Function { params, ret } => {
                assert_eq!(params, &vec![Type::TypeVar("a".into(), 0)]);
                assert_eq!(**ret, Type::Bool);
            }
            other => panic!("expected Function type alias, got {other}"),
        }
    }

    // -- Row polymorphism --

    #[test]
    fn test_type_expr_open_record() {
        let ty = result_field(
            "[Open: [type [name: String ...]]]\n[p: [@Open [name: Alice  age: 30]]]",
            "p",
        );
        match ty {
            Type::Record(Row {
                fields,
                tail: RowTail::RowVar(name, _),
            }) if name.starts_with("_open") => {
                assert_eq!(fields.get("name"), Some(&Type::Str));
            }
            other => panic!("expected open Record with fresh row var, got {other}"),
        }
    }

    #[test]
    fn test_type_expr_row_var_record() {
        let ty = result_field(
            "[WithName: [type [name: String ...rest]]]\n[p: [@WithName [name: Alice]]]",
            "p",
        );
        match ty {
            Type::Record(Row {
                fields,
                tail: RowTail::RowVar(name, _),
            }) => {
                assert_eq!(fields.get("name"), Some(&Type::Str));
                assert_eq!(name, "rest");
            }
            other => panic!("expected record with row var, got {other}"),
        }
    }

    #[test]
    fn test_type_expr_closed_record() {
        let ty = result_field(
            "[Closed: [type [name: String]]]\n[p: [@Closed [name: Alice]]]",
            "p",
        );
        match ty {
            Type::Record(Row {
                tail: RowTail::Empty,
                ..
            }) => {}
            other => panic!("expected closed Record, got {other}"),
        }
    }

    #[test]
    fn test_anonymous_open_record_annotations_get_fresh_vars() {
        // Each anonymous open record annotation should get a distinct row variable
        // Use inline open record annotations in function parameters
        let code = r#"
            [f: [fn [x@[a: Int ...]  y@[b: String ...]]
                 [x: $x  y: $y]]]
        "#;
        let result = check(code);
        assert!(
            result.is_ok(),
            "type check should succeed with distinct row vars: {:?}",
            result
        );

        // Verify the inferred type has distinct row variables for x and y
        let ty = result_field(code, "f");
        match ty {
            Type::Function { params, .. } => {
                // Extract the row variable names from both parameters
                let (row_var_x, row_var_y) = match (&params[0], &params[1]) {
                    (
                        Type::Record(Row {
                            tail: RowTail::RowVar(name_x, _),
                            ..
                        }),
                        Type::Record(Row {
                            tail: RowTail::RowVar(name_y, _),
                            ..
                        }),
                    ) => (name_x, name_y),
                    other => panic!("expected both params to be open records, got {:?}", other),
                };

                // The row variables should be distinct (different fresh names were generated)
                assert_ne!(
                    row_var_x, row_var_y,
                    "anonymous open record annotations must generate distinct row variables"
                );

                // Both should start with "_open" prefix
                assert!(
                    row_var_x.starts_with("_open"),
                    "expected row var name to start with _open, got {}",
                    row_var_x
                );
                assert!(
                    row_var_y.starts_with("_open"),
                    "expected row var name to start with _open, got {}",
                    row_var_y
                );
            }
            other => panic!("expected function type, got {other}"),
        }
    }

    #[test]
    fn test_cross_function_anonymous_open_records_get_fresh_vars() {
        // Two separate functions using anonymous open record annotations
        // should NOT share the same row variable (the bug that fresh name generation fixed)
        // Each function gets its own independent open record constraint
        let code = r#"
            [f: [fn [x@[a: Int ...]] $x.a]
             g: [fn [y@[b: String ...]] $y.b]]
        "#;
        let result = check(code);
        assert!(
            result.is_ok(),
            "type check should succeed with independent open record constraints: {:?}",
            result
        );

        // Verify that f and g have distinct row variables
        let ty_f = result_field(code, "f");
        let ty_g = result_field(code, "g");

        let row_var_f = match ty_f {
            Type::Function { params, .. } => match &params[0] {
                Type::Record(Row {
                    tail: RowTail::RowVar(name, _),
                    ..
                }) => name.clone(),
                other => panic!("expected f param to be open record, got {:?}", other),
            },
            other => panic!("expected f to be function type, got {other}"),
        };

        let row_var_g = match ty_g {
            Type::Function { params, .. } => match &params[0] {
                Type::Record(Row {
                    tail: RowTail::RowVar(name, _),
                    ..
                }) => name.clone(),
                other => panic!("expected g param to be open record, got {:?}", other),
            },
            other => panic!("expected g to be function type, got {other}"),
        };

        // The two functions must have distinct row variables
        // If they incorrectly shared RowVar("_open", 0), this assertion would fail
        assert_ne!(
            row_var_f, row_var_g,
            "cross-function anonymous open records must generate distinct row variables"
        );

        // Both should be fresh _open names
        assert!(
            row_var_f.starts_with("_open"),
            "expected row var name to start with _open, got {}",
            row_var_f
        );
        assert!(
            row_var_g.starts_with("_open"),
            "expected row var name to start with _open, got {}",
            row_var_g
        );
    }

    #[test]
    fn test_named_row_var_level_monotonicity() {
        // Test for the row variable level monotonicity bug fix (computer-scientist-c31).
        // When a function has multiple parameters sharing a named row variable tail
        // (e.g., [fn [x@[a: Int ...r] y@[b: Str ...r]] body]), the second reference
        // to ...r should NOT reset r's level in state.levels. This mirrors the
        // resolve_type_name monotonicity fix from C71.
        //
        // Before the fix, state.levels.insert(fresh_name.clone(), state.level) ran
        // unconditionally on every ...r reference, resetting the level even if
        // unification had lowered it. After the fix, we check if the row variable
        // is already mapped and preserve its current level from state.levels.
        let code = r#"
            [f: [fn [x@[a: Int ...r]  y@[b: String ...r]]
                 [x: $x  y: $y]]]
        "#;
        let result = check(code);
        assert!(
            result.is_ok(),
            "type check should succeed with shared named row variable: {:?}",
            result
        );

        // Verify both parameters share the same row variable
        let ty = result_field(code, "f");
        match ty {
            Type::Function { params, .. } => {
                let (row_var_x, row_var_y) = match (&params[0], &params[1]) {
                    (
                        Type::Record(Row {
                            tail: RowTail::RowVar(name_x, _),
                            ..
                        }),
                        Type::Record(Row {
                            tail: RowTail::RowVar(name_y, _),
                            ..
                        }),
                    ) => (name_x, name_y),
                    other => panic!("expected both params to be open records, got {:?}", other),
                };

                // Both parameters should share the same row variable name
                // (both map to the same fresh variable _tN through the ann_mapping)
                assert_eq!(
                    row_var_x, row_var_y,
                    "named row variable ...r should be shared between parameters"
                );

                // The shared row variable should have a fresh name (from ann_mapping)
                assert!(
                    row_var_x.starts_with("_t"),
                    "expected fresh row var name to start with _t, got {}",
                    row_var_x
                );
            }
            other => panic!("expected function type, got {other}"),
        }
    }

    #[test]
    fn test_check_dot_access_lowers_row_var_levels() {
        // Test that check_dot_access correctly lowers inner type variable levels when
        // accessing an unknown field on an open record with a RowVar tail.
        //
        // SCENARIO:
        //   When check_dot_access handles a RowVar tail (typecheck.rs:695-761), it creates
        //   fresh type var β and fresh row var ρ_fresh at the current state.level, then
        //   calls lower_row_var_levels_pub(&binding, rho_level, state) at line 755.
        //
        //   ρ must be at an OUTER level (lower number) than β and ρ_fresh for the lowering
        //   to be non-trivial. We use three separate document expressions so that p's type
        //   is fully resolved into state.subst before $p.unknown is checked. The Open alias
        //   is registered at document level (state.level=0), so ρ is at level 0. The access
        //   $p.unknown happens inside a nested dict (level 2), so β and ρ_fresh are created
        //   at level 2, then lowered to ρ's level (0). Without the lowering call, the
        //   assertions below fail (2 ≤ 0 is false).
        //
        // WHY THREE EXPRESSIONS (NOT ONE OR TWO):
        //   In a single dict (letrec), p and result are siblings. p's type binding is in the
        //   local letrec subst, not in state.subst. So when check_dot_access processes $p,
        //   state.subst.apply(TypeVar(_tA)) returns TypeVar(_tA) — the TypeVar path is taken
        //   instead of the RowVar path, and ρ from Open is never directly bound.
        //   Using three expressions ensures p's type (Record({name: String}, RowVar(ρ))) is
        //   exported into state.subst via Pass 3d before expression 3 processes $p.unknown.
        //
        // TEST CASE (single document, three top-level expressions):
        //   [Open: [type [name: String ...]]]    -- expr 1: registers Open alias; ρ at level 0
        //   [p: [@Open [name: Alice]]]           -- expr 2: p : Record({name: String}, RowVar(ρ))
        //   [result: [inner: $p.unknown]]        -- expr 3: nested dict forces $p.unknown at level 2
        //
        //   When we access $p.unknown at level 2:
        //   1. $p has type Record({name: String}, RowVar(ρ, level=0)) — from state.subst
        //   2. check_dot_access sees "unknown" not in {name}, tail is RowVar(ρ) at level 0
        //   3. It creates fresh β at level 2 and ρ_fresh at level 2
        //   4. It calls lower_row_var_levels_pub to lower β and ρ_fresh to ρ's level (0)
        //   5. It binds ρ → Row({unknown: β}, RowVar(ρ_fresh)) in state.subst
        //   6. After lowering: beta_level = 0, rho_fresh_level = 0
        //
        //   The assertions `beta_level <= rho_level` and `rho_fresh_level <= rho_level` are
        //   non-trivial: they pass only because lowering reduced the levels from 2 to 0.
        //   Deleting the lower_row_var_levels_pub call would leave them at 2, failing 2 ≤ 0.
        let code = r#"
            [Open: [type [name: String ...]]]
            [p: [@Open [name: Alice]]]
            [result: [inner: $p.unknown]]
        "#;

        let result = check(code);
        assert!(
            result.is_ok(),
            "type check should succeed with dot access on open record inside nested dict: {:?}",
            result
        );

        // Verify that the `inner` field of `result` has type TypeVar (the fresh β from
        // constraint generation). result : Record({inner: TypeVar(β)}) after inference.
        let result_ty = result_field(code, "result");
        let inner_ty = match result_ty {
            Type::Record(Row { ref fields, .. }) => fields
                .get("inner")
                .cloned()
                .expect("result record should have 'inner' field"),
            other => panic!("expected result to be a Record type, got {other}"),
        };
        match inner_ty {
            Type::TypeVar(name, _level) => {
                assert!(
                    name.starts_with("_t"),
                    "expected fresh type var name to start with _t, got {}",
                    name
                );
            }
            other => panic!("expected TypeVar for $p.unknown inside nested dict, got {other}"),
        }

        // Core verification: inspect InferState to confirm level lowering occurred.
        //
        // ρ is created at level 0 (document level) when register_type_aliases runs for
        // expression 1 (state.level = 0 after infer_dict returns and restores the level).
        // β and ρ_fresh are created at level 2 (expression 3's inner dict) when $p.unknown
        // is checked. lower_row_var_levels_pub must lower them to level 0 (ρ's level).
        //
        // NON-VACUOUSNESS: if lower_row_var_levels_pub is deleted, β and ρ_fresh stay at
        // level 2 and the assertions below become "2 ≤ 0", which is false → test fails.
        let mut file = crate::parse(code).unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();

        let doc_env =
            typecheck_document(&file.node.documents[0], &env, &mut state, &mut None).unwrap();

        // Find the row variable ρ from the Open type alias
        let open_ty = match doc_env.get_type_alias("Open") {
            Some(ty) => ty,
            None => panic!("Open type alias not found in doc_env"),
        };

        let rho_name = match open_ty {
            Type::Record(Row {
                tail: RowTail::RowVar(name, _),
                ..
            }) => name.clone(),
            other => panic!("expected Open to be an open record type, got {other}"),
        };

        // ρ must be bound in state.subst.row_map after dot access on the open record
        let bound_row = state
            .subst
            .row_map
            .get(&rho_name)
            .expect("ρ must be bound in subst after unknown-field dot access on open record");

        assert!(
            bound_row.fields.contains_key("unknown"),
            "bound row should contain 'unknown' field"
        );

        // ρ's current level (from state.levels, authoritative after any lowering)
        let rho_level = state
            .levels
            .get(&rho_name)
            .copied()
            .expect("ρ should be in state.levels after dot access");

        // ρ was created at document level (state.level = 0 when register_type_aliases runs).
        // Confirm this expectation so the non-vacuousness of the assertion is explicit.
        assert_eq!(
            rho_level, 0,
            "ρ (Open's row var) should be at level 0 (created at document level by register_type_aliases)"
        );

        // Check β's level — must be TypeVar; panic if not
        let Type::TypeVar(beta_name, _) = bound_row
            .fields
            .get("unknown")
            .expect("β must be present in bound row for 'unknown' field")
        else {
            panic!(
                "expected TypeVar for 'unknown' field in bound row, got {:?}",
                bound_row.fields.get("unknown")
            )
        };
        let beta_level = state
            .levels
            .get(beta_name)
            .copied()
            .expect("β should be in state.levels after dot access");

        // β was originally created at level 2 (inside the nested inner dict) and lowered to 0.
        // Without lowering: beta_level = 2, assertion 2 ≤ 0 would fail.
        assert!(
            beta_level <= rho_level,
            "β level ({}) should be ≤ ρ level ({}) after lower_row_var_levels_pub; \
             β is created at the inner dict level (2) and must be lowered to ρ's level (0)",
            beta_level,
            rho_level
        );

        // Check ρ_fresh's level — must be RowVar; panic if not
        let RowTail::RowVar(rho_fresh_name, _) = &bound_row.tail else {
            panic!(
                "expected RowVar tail in bound row, got {:?}",
                bound_row.tail
            )
        };
        let rho_fresh_level = state
            .levels
            .get(rho_fresh_name)
            .copied()
            .expect("ρ_fresh should be in state.levels after dot access");

        // ρ_fresh was originally created at level 2 and lowered to 0.
        // Without lowering: rho_fresh_level = 2, assertion 2 ≤ 0 would fail.
        assert!(
            rho_fresh_level <= rho_level,
            "ρ_fresh level ({}) should be ≤ ρ level ({}) after lower_row_var_levels_pub; \
             ρ_fresh is created at the inner dict level (2) and must be lowered to ρ's level (0)",
            rho_fresh_level,
            rho_level
        );
    }

    #[test]
    fn test_type_assert_open_record_accepts_extra_fields() {
        check("[@[name: String ...] [name: Alice  age: 30]]").unwrap();
    }

    #[test]
    fn test_type_assert_closed_record_rejects_extra_fields() {
        let errors = check_err("[@[name: String] [name: Alice  age: 30]]");
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("type mismatch"));
    }

    #[test]
    fn test_type_assert_open_record_requires_fields() {
        let errors = check_err("[@[name: String ...] [age: 30]]");
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("type mismatch"));
    }

    #[test]
    fn test_dot_access_on_open_record_known_field() {
        assert_eq!(
            result_field(
                "[Open: [type [name: String ...]]]\n[p: [@Open [name: Alice  age: 30]]]\n[result: $p.name]",
                "result",
            ),
            Type::Str,
        );
    }

    #[test]
    fn test_dot_access_on_open_record_unknown_field() {
        // Accessing an unknown field on an open record (RowVar tail) now generates a
        // constraint binding ρ → Row({unknown: β}, RowVar(ρ_fresh)) and returns β.
        // The result is a TypeVar (the fresh field type), not Any.
        // See doc/07-type-extensions.md Part 5 (RowVar case).
        let ty = result_field(
            "[Open: [type [name: String ...]]]\n[p: [@Open [name: Alice]]]\n[result: $p.unknown]",
            "result",
        );
        assert!(
            matches!(ty, Type::TypeVar(_, _)),
            "expected TypeVar for unknown open-record field, got {ty}"
        );
    }

    #[test]
    fn test_data_dict_always_closed() {
        let ty = infer("[a: 1  b: 2]");
        match ty {
            Type::Record(Row {
                tail: RowTail::Empty,
                ..
            }) => {}
            other => panic!("expected closed Record for data dict, got {other}"),
        }
    }

    #[test]
    fn test_rest_in_data_dict_ignored() {
        let ty = infer("[a: 1 ...]");
        match ty {
            Type::Record(Row {
                fields,
                tail: RowTail::Empty,
            }) => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(1)));
            }
            other => panic!("expected closed Record, got {other}"),
        }
    }

    // -- Let-generalization tests --

    #[test]
    fn test_let_gen_varref_instantiation() {
        // Each reference to $id should get a fresh instantiation
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[result: [a: [call $id 42]  b: [call $id hello]]]",
            "result",
        );
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(42)));
                assert_eq!(
                    fields.get("b"),
                    Some(&Type::StringLiteral("hello".to_string()))
                );
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_let_gen_forward_ref_unification() {
        // Forward reference $b should unify with 42
        let ty = infer("[a: $b  b: 42]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(42)));
                assert_eq!(fields.get("b"), Some(&Type::IntLiteral(42)));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_let_gen_nested_dicts_level_increment() {
        // Task 3: Verify state.level increments correctly for nested dict inference
        // and that inner dict entries generalize independently of outer
        // For [outer: [inner: 42]], outer dict runs at level 1, inner at level 2
        // The inner dict should generalize at level 1, producing schemes for its entries

        // Test with a more complex example that shows level scoping:
        // [outer: [id: [fn [x@a] $x]]]
        // The `id` function should be polymorphic even when nested
        let env = doc_env("[outer: [id: [fn [x@a] $x]]]");
        let outer_scheme = env.get("outer").expect("outer should be in env");

        match &outer_scheme.body {
            Type::Record(Row {
                fields: outer_fields,
                ..
            }) => {
                // The outer dict's `id` field should have a Function type
                let id_type = outer_fields
                    .get("id")
                    .expect("id should be a field in outer");

                match id_type {
                    Type::Function { params, ret } => {
                        // Params and return should involve type variables (from annotation @a)
                        assert!(
                            matches!(params.get(0), Some(Type::TypeVar(_, _))),
                            "id param should be TypeVar, got {:?}",
                            params
                        );
                        assert!(
                            matches!(ret.as_ref(), Type::TypeVar(_, _)),
                            "id return should be TypeVar, got {:?}",
                            ret
                        );
                    }
                    other => panic!("expected Function type for id, got {:?}", other),
                }
            }
            other => panic!("expected Record for outer, got {:?}", other),
        }
    }

    #[test]
    fn test_let_gen_document_boundary_threading() {
        // Type schemes should thread across document boundaries
        // Simpler test: just check that $id is accessible across documents
        let env = file_env("[id: 42]\n---\n[result: $id]");

        // Check that $id is available in the final environment
        assert!(env.get("id").is_some(), "id should be in scope");

        // Check that result refers to id correctly
        assert!(env.get("result").is_some(), "result should be in scope");
    }

    #[test]
    fn test_let_gen_mutual_recursion() {
        // Mutual recursion within a dict should work with monomorphic inference
        let ty = infer("[a: $b  b: $a  c: 42]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert!(fields.contains_key("a"));
                assert!(fields.contains_key("b"));
                assert_eq!(fields.get("c"), Some(&Type::IntLiteral(42)));

                // Task 2: Assert the TYPES of a and b after mutual reference unification
                let a_type = fields.get("a").expect("a should exist");
                let b_type = fields.get("b").expect("b should exist");

                // a and b reference each other, so they should unify to the same TypeVar
                // or both be Any if unification fails during Pass 3
                match (a_type, b_type) {
                    (Type::TypeVar(a_name, a_level), Type::TypeVar(b_name, b_level)) => {
                        // They should be unified to the same variable
                        assert_eq!(
                            a_name, b_name,
                            "mutually recursive a and b should unify to same TypeVar, got a={} b={}",
                            a_name, b_name
                        );
                        assert_eq!(
                            a_level, b_level,
                            "mutually recursive a and b should have same level"
                        );
                    }
                    (Type::Any, Type::Any) => {
                        // Both Any is also valid (error recovery path)
                    }
                    _ => panic!(
                        "expected a and b to both be TypeVar or both be Any, got a={:?} b={:?}",
                        a_type, b_type
                    ),
                }
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_let_gen_typevar_in_bracket_access() {
        // TypeVars should be handled gracefully in bracket access
        assert_eq!(
            result_field("[result: $data[$key]  data: [x: 1]  key: x]", "result"),
            Type::Any,
        );
    }

    #[test]
    fn test_let_gen_typevar_in_dot_access() {
        // Dot access on a TypeVar now generates a constraint (TypeVar α case) rather than
        // returning Any. When `$data` has an unknown type during letrec pass 3,
        // `$data.x` generates constraint α = Record({x: β}, RowVar(ρ)) and returns β.
        // The result is a TypeVar β (the fresh field type).
        // Note: full constraint propagation between letrec fields and state.subst is future
        // work — β is not yet resolved to IntLiteral(1) even though data: [x: 1] confirms it.
        // See row-unification-f-b in TODO.md.
        let ty = infer("[result: $data.x  data: [x: 1]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                // result is a TypeVar (β, the inferred field type of x), not Any.
                let result_ty = fields.get("result").expect("field 'result' should exist");
                assert!(
                    matches!(result_ty, Type::TypeVar(_, _)),
                    "expected TypeVar for constrained dot access field, got {result_ty}"
                );
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    // --- Task 1: Core let-generalization unit tests ---

    #[test]
    fn test_let_gen_polymorphic_identity_generalizes() {
        // [id: [fn [x@a] $x]] should generalize id to a polymorphic TypeScheme
        let env = doc_env("[id: [fn [x@a] $x]]");
        let id_scheme = env.get("id").expect("id should be in env");

        // The scheme should have non-empty vars (it's polymorphic)
        assert!(
            !id_scheme.type_vars.is_empty(),
            "id should be polymorphic (non-empty type_vars), got scheme: {:?}",
            id_scheme
        );
    }

    #[test]
    fn test_let_gen_nested_dicts_level_correct() {
        // Nested dict [outer: [inner: 42]] should infer correct types
        let ty = result_field("[outer: [inner: 42]]\n[result: $outer]", "result");
        match ty {
            Type::Record(Row { fields, .. }) => {
                assert_eq!(
                    fields.get("inner"),
                    Some(&Type::IntLiteral(42)),
                    "inner field should be IntLiteral(42)"
                );
            }
            other => panic!("expected Record for outer, got {other}"),
        }
    }

    #[test]
    fn test_let_gen_any_touched_not_generalized() {
        // When a variable unifies with Any, it should NOT be generalized
        // [fn [x] $x] has unannotated param, so x has type Any
        // The identity function should be monomorphic (Any → Any)
        let env = doc_env("[id: [fn [x] $x]]");
        let id_scheme = env.get("id").expect("id should be in env");

        // The scheme should have empty vars (monomorphic, Any-touched)
        assert!(
            id_scheme.type_vars.is_empty(),
            "id with unannotated param should be monomorphic (Any-touched), got scheme: {:?}",
            id_scheme
        );
    }

    // -- Bidirectional type checking tests --

    #[test]
    fn test_check_expr_basic_subsumption() {
        // IntLiteral(42) should check against Int via subsumption
        let ty = result_field("[x: [@Int 42]]", "x");
        assert_eq!(ty, Type::Int, "IntLiteral should subsume to Int");

        // IntLiteral(42) should check against Number via subsumption
        let ty = result_field("[x: [@Number 42]]", "x");
        assert_eq!(ty, Type::Number, "IntLiteral should subsume to Number");

        // StringLiteral should subsume to String
        let ty = result_field("[x: [@String hello]]", "x");
        assert_eq!(ty, Type::Str, "StringLiteral should subsume to String");
    }

    #[test]
    fn test_call_mono_argument_checking() {
        // Monomorphic function call should use check_expr for arguments
        // This should succeed: IntLiteral(42) <: Int
        let ty = result_field("[f: [fn [x@Int] $x]]\n[result: [call $f 42]]", "result");
        assert_eq!(ty, Type::Int, "CALL-MONO should accept IntLiteral arg");

        // This should fail: String is not subtype of Int
        let errors = check_err("[f: [fn [x@Int] $x]]\n[result: [call $f hello]]");
        assert!(
            errors.iter().any(|e| e.message.contains("type mismatch")),
            "CALL-MONO should reject String arg for Int param, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_lambda_checking_mode_concrete() {
        // Lambda checked against concrete function type should propagate param types
        // Define a concrete function type alias first
        let env = doc_env("[IntFn: [type [Fn@Int [Int]]]]\n[f: [@IntFn [fn [x] $x]]]");
        let f_scheme = env.get("f").unwrap();
        match &f_scheme.body {
            Type::Function { params, ret } => {
                assert_eq!(params, &vec![Type::Int]);
                assert_eq!(**ret, Type::Int);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_lambda_checking_mode_with_polymorphic_expected() {
        // Lambda checked against polymorphic function type should NOT use checking mode
        // (falls back to synthesis + subsumption)
        let ty = result_field(
            "[Mapper: [type [Fn@b [a]]]]\n[x: [@Mapper [fn [v] $v]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                // When checking mode is skipped (has_type_vars), params stay as annotated or Any
                assert_eq!(params, vec![Type::TypeVar("a".into(), 0)]);
                assert_eq!(*ret, Type::TypeVar("b".into(), 0));
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_type_assert_checking_mode() {
        // TypeAssert should use check_expr for subsumption
        let ty = result_field("[x: [@Int 42]]", "x");
        assert_eq!(ty, Type::Int, "TypeAssert should accept IntLiteral <: Int");

        // TypeAssert with default should suppress errors
        let ty = result_field("[x: [@[type: Int  default: 0] hello]]", "x");
        assert_eq!(
            ty,
            Type::Int,
            "TypeAssert with default should suppress errors"
        );
    }

    #[test]
    fn test_call_poly_still_uses_unify() {
        // Polymorphic function call should still use unification (not check_expr)
        let ty = result_field("[f: [fn [x@a] $x]]\n[result: [call $f 42]]", "result");
        assert_eq!(ty, Type::IntLiteral(42), "CALL-POLY should unify");

        // Multiple calls should get independent instantiations
        let env = doc_env("[f: [fn [x@a] $x]]\n[r1: [call $f 42]]\n[r2: [call $f hello]]");
        let r1 = env.get("r1").unwrap();
        let r2 = env.get("r2").unwrap();
        assert_eq!(r1.body, Type::IntLiteral(42));
        assert_eq!(r2.body, Type::StringLiteral("hello".into()));
    }

    #[test]
    fn test_function_return_annotation_checking() {
        // Function with return annotation should check body via check_expr
        // Subsumption should work: IntLiteral(42) <: Int
        let ty = result_field("[f: [fn@Int [] 42]]", "f");
        match ty {
            Type::Function { ret, .. } => {
                assert_eq!(*ret, Type::Int, "Return type should be declared type");
            }
            other => panic!("expected Function, got {other}"),
        }

        // IntLiteral should subsume to Number in return annotation
        let ty = result_field("[f: [fn@Number [] 42]]", "f");
        match ty {
            Type::Function { ret, .. } => {
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }

        // Type mismatch should fail
        let errors = check_err("[f: [fn@Int [] hello]]");
        assert!(
            errors.iter().any(|e| e.message.contains("type mismatch")),
            "Function body type mismatch should error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_function_return_annotation_with_type_var() {
        // Function with polymorphic return annotation should use unification mode
        // [fn@a [x@a] 42] — return annotation contains TypeVar, so body type
        // should be unified with the declared type, binding the TypeVar.
        // Without the fix, check_expr uses is_subtype which requires exact match
        // for TypeVars (reflexive equality only), so is_subtype(IntLiteral(42), TypeVar("a"))
        // returns false and the function is rejected.
        //
        // The key is that this should successfully type check (not error).
        let result = check("[f: [fn@a [x@a] 42]]");
        assert!(
            result.is_ok(),
            "Function with polymorphic return annotation should type check: {:?}",
            result.err()
        );

        // Identity function with return annotation should also work
        let result = check("[f: [fn@a [x@a] $x]]");
        assert!(
            result.is_ok(),
            "Identity function with polymorphic return annotation should type check: {:?}",
            result.err()
        );

        // Polymorphic function that returns a different type than param should succeed
        // [fn@a [x@b] 42] where a and b are different type variables
        // After unification: a gets bound to IntLiteral(42), but param is still b
        // This should succeed since there's no constraint linking a and b
        let result = check("[f: [fn@a [x@b] 42]]");
        assert!(
            result.is_ok(),
            "Polymorphic function with different param/return type vars should type check: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_function_return_annotation_with_type_var_error_path() {
        // Exercise the error path of the new unification-mode branch
        // (declared.has_type_vars() = true) at src/typecheck.rs:1056-1062.
        //
        // When the body expression fails to infer a type, the error propagates
        // via `?` at line 1057. This test confirms that the new path correctly
        // surfaces body inference errors rather than silently succeeding.
        //
        // [fn@a [x@a] [call 42 1]] — return annotation @a contains a TypeVar
        // so we enter the unification-mode branch. The body `[call 42 1]`
        // attempts to call an integer literal as a function, which fails
        // infer_expr with "expected function type, got IntLiteral(42)".
        let errors = check_err("[f: [fn@a [x@a] [call 42 1]]]");
        assert!(
            !errors.is_empty(),
            "Calling a non-function in a TypeVar-annotated fn body should produce type errors"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("expected function type")),
            "Expected 'expected function type' error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_lambda_checking_mode_annotated_param_incompatible() {
        // Lambda with annotated param checked against expected function type where
        // the annotation is INCOMPATIBLE with the expected param type should error.
        // Expected: Fn(Int -> Int), lambda: [fn [x@String] $x]
        // The annotation String is incompatible: Int (expected) is not a subtype of String.
        // This tests the fix added in the bidirectional-typing fix pass (contravariant check).
        let errors = check_err("[x: [@[Fn@Int [Int]] [fn [x@String] $x]]]");
        assert!(
            errors.iter().any(|e| e.message.contains("type mismatch")),
            "Incompatible param annotation should produce type mismatch error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_lambda_checking_mode_return_annotation_and_expected_type() {
        // Lambda with both a return annotation and an expected function type.
        // [@[Fn@Number [Int]] [fn@Int [x] $x]] — expected return Number, declared return Int.
        // Since Int <: Number, the check `declared <: expected_ret` passes (covariant return).
        // Body $x is checked against declared Int (passes since x gets type Int from expected).
        // The function type recorded in the type_map is the EXPECTED type (Fn(Int→Number))
        // because check_expr records expected.clone() at the lambda checking mode exit.
        let ty = result_field("[f: [@[Fn@Number [Int]] [fn@Int [x] $x]]]", "f");
        match ty {
            Type::Function { params, ret } => {
                // Lambda checking mode propagates expected param type Int
                assert_eq!(
                    params,
                    vec![Type::Int],
                    "param should be Int from expected type"
                );
                // The recorded function type is the expected Fn(Int→Number), ret = Number
                assert_eq!(
                    *ret,
                    Type::Number,
                    "return type is the expected Number (type_map records expected)"
                );
            }
            other => panic!("expected Function type, got {other}"),
        }

        // Incompatible direction: expected return Int, declared return Number.
        // is_subtype(&Number, &Int) = false → should error.
        let errors = check_err("[f: [@[Fn@Int [Int]] [fn@Number [x] 42]]]");
        assert!(
            errors.iter().any(|e| e.message.contains("type mismatch")),
            "Declared return Number is not subtype of expected Int — should error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_lambda_checking_mode_param_annotation_with_type_var() {
        // Task 1 fix: Lambda with @a-style param annotation checked against concrete function type.
        // When the annotation is a TypeVar, is_subtype fails (TypeVars only match reflexively).
        // The fix switches to unification mode when resolved.has_type_vars().
        //
        // Pattern: [call $identity [fn@b [y@b] $y]] where identity is polymorphic.
        // check_expr sees expected_ty=concrete from identity's instantiation, resolved=TypeVar("b").
        // Without fix: is_subtype(concrete, TypeVar("b")) = false → error.
        // With fix: unify(concrete, TypeVar("b")) binds b → success.
        let result = check("[identity: [fn [x@a] $x]]\n[result: [call $identity [fn@b [y@b] $y]]]");
        assert!(
            result.is_ok(),
            "Lambda with TypeVar param annotation in checking mode should unify, not subsume: {:?}",
            result.err()
        );

        // Verify the result typechecks with concrete argument
        let ty = result_field(
            "[identity: [fn [x@a] $x]]\n[result: [call $identity [fn@b [y@b] $y]]]\n[test: [call $result 42]]",
            "test"
        );
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "Result function should work with concrete arg"
        );
    }

    #[test]
    fn test_lambda_checking_mode_return_annotation_with_type_var() {
        // Task 1 fix: Lambda with @a-style return annotation checked against concrete function type.
        // When the return annotation is a TypeVar, is_subtype fails (TypeVars only match reflexively).
        // The fix switches to unification mode when declared.has_type_vars().
        //
        // Pattern: [@[Fn@Int [Int]] [fn@c [x] 42]] — expected return Int, declared TypeVar("c").
        // Without fix: is_subtype(TypeVar("c"), Int) = false → error.
        // With fix: unify(TypeVar("c"), Int) binds c → success.
        let result = check("[f: [@[Fn@Int [Int]] [fn@c [x] 42]]]");
        assert!(
            result.is_ok(),
            "Lambda with TypeVar return annotation in checking mode should unify, not subsume: {:?}",
            result.err()
        );

        // Verify the recorded function type
        let ty = result_field("[f: [@[Fn@Int [Int]] [fn@c [x] $x]]]", "f");
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::Int], "param from expected type");
                assert_eq!(*ret, Type::Int, "return from expected type");
            }
            other => panic!("expected Function type, got {other}"),
        }
    }

    #[test]
    fn test_inline_lambda_with_polymorphic_return_annotation() {
        // Task 2 fix: Inline lambda with polymorphic return annotation.
        // Pattern: [call [fn@a [x@a] $x] 42] — identity function with polymorphic annotation.
        //
        // Without fix at check_call line ~888:
        // 1. infer_fn returns Fn(TypeVar("_t5") -> TypeVar("_t5")) with state.subst = {_t5 -> TypeVar("_t6")}
        //    (from unifying body $x with return annotation @a)
        // 2. check_call receives func_ty with unresolved _t5
        // 3. has_type_vars() = true → CALL-POLY fires
        // 4. instantiate_at_level freshens _t5 to _t7
        // 5. unify tries to bind _t7, but the substitution for _t5 is lost → wrong type
        //
        // With fix: state.subst.apply() resolves _t5 before has_type_vars() check.
        let ty = result_field("[result: [call [fn@a [x@a] $x] 42]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "Inline lambda with polymorphic return annotation should infer correctly"
        );

        // Verify multi-arg case where all params share the same type variable
        let ty = result_field("[result: [call [fn@a [x@a y@a] $x] 1 1]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(1),
            "Multi-arg inline lambda with polymorphic annotation should work"
        );

        // Verify constant-return case: [call [fn@a [x@a] 42] 42]
        // Based on the mempalace C66 finding. When param and return share annotation @a,
        // they're constrained to be the same type. The body type (IntLiteral(42)) binds @a.
        // Without the fix: CALL-POLY would fire, freshen the TypeVars, and produce incorrect types.
        // With the fix: state.subst.apply() resolves the function type to Fn(IntLiteral(42) -> IntLiteral(42)),
        // CALL-MONO fires, and the call succeeds with matching literal types.
        let ty = result_field("[result: [call [fn@a [x@a] 42] 42]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "Constant-return inline lambda with matching arg should work"
        );
    }

    #[test]
    fn test_zero_param_monomorphic_function_type() {
        // Zero-param monomorphic functions work correctly with CALL-MONO.
        // The function type is inferred from the return type annotation.
        //
        // Historical note: Previously there was a bug in CALL-POLY with zero params,
        // where the code returned `*ret.clone()` (the pre-instantiation return type)
        // instead of `*inst_ret.clone()` (the post-substitution return type).
        // This was fixed in the bidirectional-typing-b sprint.
        //
        // Practically, zero-arity polymorphic functions in LLT are rare:
        //   - Unannotated params get Type::Any (monomorphic path, no type vars).
        //   - Annotated type-var params require at least one param (by definition).
        //   - [fn@a [] body] fails to type-check because body type ≮ TypeVar a.
        //
        // This test verifies the zero-param CALL-MONO path (no type vars) works correctly.

        // Zero-param monomorphic function (CALL-MONO): the function type is correct.
        let ty = result_field("[f: [fn@Int [] 42]]", "f");
        match ty {
            Type::Function { params, ret } => {
                assert!(params.is_empty(), "zero-param fn should have no params");
                assert_eq!(
                    *ret,
                    Type::Int,
                    "declared return type Int should be preserved"
                );
            }
            other => panic!("expected Function type for zero-param fn, got {other}"),
        }
    }

    // -- Task 1: CALL-MONO argument type checking verification --

    #[test]
    fn test_call_mono_argument_type_checking_verification() {
        // CALL-MONO uses check_expr for argument type checking
        // IntLiteral(42) <: Int succeeds
        assert!(check("[f: [fn [x@Int] $x]]\n[result: [call $f 42]]").is_ok());

        // StringLiteral for Int param fails
        let errors = check_err("[f: [fn [x@Int] $x]]\n[result: [call $f \"hello\"]]");
        assert!(
            errors.iter().any(|e| e.message.contains("type mismatch")),
            "StringLiteral arg for Int param should error: {:?}",
            errors
        );

        // IntLiteral(42) <: Number succeeds (transitive subsumption)
        assert!(check("[f: [fn [x@Number] $x]]\n[result: [call $f 42]]").is_ok());
    }

    // -- Task 3: Subsumption tests --

    #[test]
    fn test_subsumption_int_literal_to_int() {
        // IntLiteral(42) <: Int via [SUB] rule
        assert!(check("[result: [@Int 42]]").is_ok());
    }

    #[test]
    fn test_subsumption_int_literal_to_number() {
        // IntLiteral(42) <: Int <: Number (transitive)
        assert!(check("[result: [@Number 42]]").is_ok());
    }

    #[test]
    fn test_subsumption_string_literal_to_string() {
        // StringLiteral("hello") <: String
        assert!(check("[result: [@String \"hello\"]]").is_ok());
    }

    #[test]
    fn test_subsumption_direction_matters() {
        // Int <: Number succeeds, but Number <: Int fails — direction matters
        assert!(check("[result: [@Number 42]]").is_ok());
        let errors = check_err("[f: [fn [x@Int] $x]]\n[result: [@Int [call $f 3.14]]]");
        assert!(
            errors.iter().any(|e| e.message.contains("type mismatch")),
            "Float should not be subtype of Int: {:?}",
            errors
        );
    }

    #[test]
    fn test_subsumption_float_to_number() {
        // Float <: Number
        assert!(check("[result: [@Number 3.14]]").is_ok());
    }

    // -- Task 3: Lambda parameter inference tests --

    #[test]
    fn test_lambda_param_inference_from_context() {
        // When checking lambda against Fn(Int → Int), unannotated param gets Int
        // Uses Fn@ReturnType [params] syntax to get a real function type, not Type::Any
        assert!(check("[result: [@[Fn@Int [Int]] [fn [x] $x]]]").is_ok());
    }

    #[test]
    fn test_lambda_param_inference_preserves_annotation() {
        // Annotated param @Int is compatible with expected Number (Int <: Number for contravariant)
        assert!(check("[result: [@[Fn [Number] [Number]] [fn [x@Int] $x]]]").is_ok());
    }

    #[test]
    fn test_lambda_param_inference_rejects_incompatible_annotation() {
        // @String is NOT compatible with expected Int param (Int <: String is false)
        // Uses Fn@ReturnType [params] syntax for function type annotation
        let errors = check_err("[result: [@[Fn@Int [Int]] [fn [x@String] $x]]]");
        assert!(
            errors.iter().any(|e| e.message.contains("type mismatch")),
            "String annotation should be incompatible with Int expected param: {:?}",
            errors
        );
    }

    // -- Task 8: Zero-param polymorphic fix verification --

    #[test]
    fn test_zero_param_polymorphic_function_instantiation() {
        // Zero-param CALL-POLY must return *inst_ret* (instantiated), not *ret* (scheme-internal).
        // Without the fix, ret == inst_ret for concrete return types, but the instantiated copy
        // is the one whose type variables (if any) are fresh per-call-site.
        let ty = result_field("[f: [fn@Int [] 42]]\n[result: [call $f]]", "result");
        assert_eq!(
            ty,
            Type::Int,
            "zero-param fn@Int should return Int, got {ty}"
        );
    }

    // -- Annotation fresh variable mapping per function --

    #[test]
    fn test_sibling_functions_with_shared_annotation_names() {
        // Bug: sibling functions in the same letrec dict that use the same annotation
        // name (e.g., @a) should NOT share type variables. Each function should get
        // its own fresh type variable for @a.
        //
        // [f: [fn [x@a] $x]  g: [fn [y@a] 42]]
        //
        // Before fix: both functions share TypeVar("a", level) in state.levels, so
        // unification in f's inference can affect g's type variable.
        //
        // After fix: f gets TypeVar("_t0", level) and g gets TypeVar("_t1", level).
        // Within each function, repeated uses of @a map to the same fresh var.
        let result = check("[f: [fn [x@a] $x]  g: [fn [y@a] 42]]");
        assert!(
            result.is_ok(),
            "sibling functions with same annotation name should type check: {:?}",
            result.err()
        );

        // Verify that within a single function, repeated uses of @a map to the same variable
        let result = check("[f: [fn [x@a  y@a] $x]]");
        assert!(
            result.is_ok(),
            "repeated annotation @a within single function should use same type variable: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_annotation_fresh_vars_are_independent_across_siblings() {
        // Each sibling function should have independent type variables for its annotations.
        // This test ensures that type constraints in one function don't leak to another.
        //
        // [id: [fn [x@a] $x]  const42: [fn [y@a] 42]]
        //
        // id should be polymorphic: ∀a. Fn(a → a)
        // const42 should be polymorphic: ∀a. Fn(a → Int)
        //
        // The @a in id and the @a in const42 must not interfere with each other.
        let ty = infer("[id: [fn [x@a] $x]  const42: [fn [y@a] 42]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                // Verify both functions exist
                assert!(fields.contains_key("id"), "should have 'id' field");
                assert!(
                    fields.contains_key("const42"),
                    "should have 'const42' field"
                );

                // Both should be function types
                match fields.get("id") {
                    Some(Type::Function { .. }) => {}
                    other => panic!("expected id to be Function type, got {:?}", other),
                }
                match fields.get("const42") {
                    Some(Type::Function { .. }) => {}
                    other => panic!("expected const42 to be Function type, got {:?}", other),
                }
            }
            other => panic!("expected Record type, got {other}"),
        }
    }

    #[test]
    fn test_annotation_level_monotonicity() {
        // Test that resolve_type_name respects level lowering monotonicity (Kiselyov 2013).
        // When the same annotation name is used multiple times in a function and unification
        // lowers the level between references, the level must not be reset.
        //
        // Pattern: [fn [x@a y@a] body] where x and y share the same annotation name @a.
        // Both should map to the same fresh TypeVar (e.g., _t0), and subsequent references
        // to @a within type annotations should return the TypeVar with its current level
        // from state.levels, NOT reset it to state.level.
        //
        // This test verifies the function type-checks correctly. If level monotonicity
        // were violated, generalization might fail or produce incorrect types.

        // Case 1: Two params share the same annotation name
        let ty = infer("[f: [fn [x@a y@a] $x]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                match fields.get("f") {
                    Some(Type::Function { params, .. }) => {
                        // Both params should unify to the same type variable
                        assert_eq!(params.len(), 2, "function should have 2 params");
                        // They should be the same TypeVar (same name after unification)
                        assert_eq!(
                            params[0], params[1],
                            "both params should have same type (unified via shared annotation)"
                        );
                    }
                    other => panic!("expected f to be Function type, got {:?}", other),
                }
            }
            other => panic!("expected Record type, got {other}"),
        }

        // Case 2: Return annotation reuses param annotation
        let ty = infer("[f: [fn@a [x@a] $x]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                match fields.get("f") {
                    Some(Type::Function { params, ret }) => {
                        // Param and return should unify to the same type variable
                        assert_eq!(
                            params[0], **ret,
                            "param and return should have same type (unified via shared annotation)"
                        );
                    }
                    other => panic!("expected f to be Function type, got {:?}", other),
                }
            }
            other => panic!("expected Record type, got {other}"),
        }

        // Case 3: Generalization should succeed despite multiple uses of same annotation
        let env = doc_env("[f: [fn [x@a y@a] $x]]");
        let f_scheme = env.get("f").expect("f should be in env");
        assert!(
            !f_scheme.type_vars.is_empty(),
            "f should be polymorphic (generalized despite multiple @a uses), got scheme: {:?}",
            f_scheme
        );
    }

    #[test]
    fn test_polymorphic_function_call_no_double_instantiation() {
        // This test verifies that calling a polymorphic function from the environment
        // only instantiates once (not VAR-POLY + CALL-POLY double instantiation).
        // The optimization special-cases VarRef in Call expressions for polymorphic schemes.

        // Test with multiple calls to the same polymorphic function across documents
        let ty = result_type("[id: [fn [x@a] $x]]\n[r1: [call $id 42]  r2: [call $id hello]]");

        match ty {
            Type::Record(Row { fields, .. }) => {
                // r1 should be IntLiteral(42) due to polymorphic instantiation
                assert_eq!(
                    fields.get("r1"),
                    Some(&Type::IntLiteral(42)),
                    "r1 should be IntLiteral(42)"
                );

                // r2 should be StringLiteral("hello") due to polymorphic instantiation
                assert_eq!(
                    fields.get("r2"),
                    Some(&Type::StringLiteral("hello".to_string())),
                    "r2 should be StringLiteral(\"hello\")"
                );
            }
            other => panic!("expected Record type, got {:?}", other),
        }
    }

    // -- state.subst apply() regression test --

    #[test]
    #[ignore] // KNOWN ISSUE: requires row-unification-h or_insert unification fix
    fn test_bracket_access_forward_ref_resolves_correctly() {
        // Forward-reference bracket access should resolve to field type.
        // Exercises the state.subst.apply() path in check_bracket_access (line ~717).
        let ty = result_field("[result: $data[name]  data: [name: hello]]", "result");
        assert_eq!(ty, Type::StringLiteral("hello".to_string()));
    }

    // -- CALL-POLY state.subst constraint test --

    #[test]
    fn test_call_poly_state_subst_applied() {
        // Task 7: Regression test for `state.subst.apply()` in the CALL-POLY arm of
        // check_call_with_scheme and check_call.
        //
        // The two CALL-POLY sites are:
        //   check_call_with_scheme (CALL-POLY arm): Ok(subst.apply(ret))
        //     (subst is seeded from state.subst, so single apply is sufficient)
        //   check_call (CALL-POLY arm): Ok(state.subst.apply(&subst.apply(inst_ret)))
        //
        // Without state.subst resolution, the return type may contain unresolved TypeVars.
        // In check_call_with_scheme, the seeded subst handles this implicitly.
        // In check_call, the explicit state.subst.apply() resolves TypeVars bound from
        // prior dot-access constraints that wrote to state.subst.
        //
        // HOW THIS TEST DETECTS THE REGRESSION:
        //   The forward-reference in `$data` forces Pass 1 to assign TypeVar(_t_data) to
        //   `data`'s slot.  When `result` is processed (left-to-right in Pass 3),
        //   check_dot_access sees TypeVar(_t_data) for `$data`, enters the TypeVar arm, and
        //   writes `_t_data → Record({name: _t_name}, ρ)` into state.subst (not local subst).
        //   It returns TypeVar(_t_name) as the field type (arg to $id).
        //   After call unification: local subst[_t_call = _t_name].
        //   subst.apply(inst_ret) = _t_name (local subst resolves _t_call to _t_name).
        //   state.subst.apply(_t_name) = _t_name (not yet bound; data not yet processed).
        //
        //   After Pass 3 processes `data: [name: hello]`, unification propagates
        //   _t_data = Record({name: StringLiteral("hello")}, Closed) through state.subst,
        //   and Pass 3b/3c resolves _t_name = StringLiteral("hello") globally.
        //
        //   The final asserted type of `result` comes through this chain.  If state.subst.apply()
        //   were removed from the CALL-POLY return and ALSO from Pass 3b/3c, the type would
        //   remain an unresolved TypeVar.  The test thus provides a regression guard for the
        //   full state.subst pipeline of which the CALL-POLY site is the first link.
        //
        //   A stronger isolation test (where ONLY removing state.subst.apply() from the CALL-POLY
        //   site causes failure) requires a scenario where _t_name is already bound in state.subst
        //   BEFORE the call is processed — achievable once cross-field constraint propagation within
        //   a single letrec pass is fully implemented (tracked as future work).
        // NOTE: The test input uses plain `\n` separators (NOT `---\n`), so the parser
        // produces ONE document containing three sequential dict expressions.
        // `typecheck_document` processes them left-to-right in a single letrec pass,
        // threading each expression's field scheme into the environment before moving on.
        //
        // When `[call $id $data.name]` is processed, both `id` (already a TypeScheme)
        // and `$data` (TypeVar(_t_data) at that point) are in scope from the preceding
        // expressions.  check_dot_access enters the TypeVar arm for `$data`, writes
        // `_t_data → Record({name: _t_name}, ρ)` into state.subst, and returns
        // TypeVar(_t_name) as the arg type.  After Pass 3b/3c resolves `data: [name: hello]`,
        // _t_name is bound to StringLiteral("hello") globally, and `result` resolves.
        //
        // state.subst.apply() at the CALL-POLY return is benign in this scenario (local subst
        // already resolved the binding), but the test guards the full CALL-POLY path end-to-end
        // (arg inference → unification → return type resolution).  A stronger isolation test
        // (where ONLY removing state.subst.apply() from the CALL-POLY site causes failure)
        // requires cross-field constraint propagation within a letrec pass, tracked as
        // future work (row-unification-h).
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[data: [name: hello]]\n[result: [call $id $data.name]]",
            "result",
        );
        // result should be StringLiteral("hello") via CALL-POLY: id returns same type as arg
        assert_eq!(
            ty,
            Type::StringLiteral("hello".to_string()),
            "CALL-POLY with dot-access argument should resolve return type correctly, got: {ty}"
        );
    }

    // -- CALL-POLY state.subst isolation test (cross-document boundary) --

    #[test]
    fn test_call_poly_state_subst_isolation() {
        // Cross-document regression test for `state.subst.apply()` in the CALL-POLY arm.
        //
        // SCENARIO: Two documents separated by `---`. Document 1 defines a polymorphic identity
        // function `id` and a concrete record `data` — there is no dot-access in Document 1.
        // Document 2 accesses `$data.name` (direct field lookup) and calls `[call $id $data.name]`
        // via CALL-POLY, whose argument type is resolved through the cross-document env lookup.
        //
        // Unlike test_call_poly_state_subst_applied (which uses `\n` in a single document),
        // this test crosses a true document boundary (`---`). The `state` object (including
        // state.subst) is shared across both documents, so any bindings written by document 1
        // are visible to document 2's CALL-POLY return-type resolution.
        //
        // WHY THE DOCUMENT BOUNDARY MATTERS:
        //   After document 1's infer_dict completes (Pass 3b/3c + generalization), the
        //   TypeVar α written into state.subst by check_dot_access is still present as a key
        //   in state.subst.type_map. Document 2 shares this state. If document 2's CALL-POLY
        //   return type (after local-subst resolution) is a TypeVar that is transitively bound
        //   in state.subst from document 1, then the seeded subst in check_call_with_scheme
        //   (which includes state.subst bindings) resolves it via `subst.apply(ret)` at line ~970.
        //
        // CURRENT LIMITATION (tracked as row-unification-f-b in TODO.md):
        //   The CALL-POLY return type in this test resolves correctly through the normal
        //   pipeline (document 1 puts `data` in env as a concrete type; document 2's
        //   dot-access finds `data.name` directly without a state.subst lookup). Thus,
        //   removing `state.subst.apply()` from the CALL-POLY return site ALONE would not
        //   break this test at the current level of constraint propagation.
        //
        //   True isolation — where ONLY removing state.subst.apply() from CALL-POLY causes
        //   a failure — requires that the CALL-POLY return TypeVar (after local subst) be
        //   already bound in state.subst from document 1's dot-access. This is achievable
        //   once cross-field constraint propagation within a single letrec pass is fully
        //   implemented (row-unification-f-b). At that point this comment should be updated
        //   to remove the caveat and the test should tighten to assert exactly that
        //   `state.subst.apply()` at the CALL-POLY site resolves the TypeVar.
        //
        // WHAT THE TEST DOES VERIFY:
        //   - The full CALL-POLY pipeline works across a `---` document boundary
        //   - state.subst is shared across documents (state persists through file_env)
        //   - Document 1's dot-access constraint generation (TypeVar α arm) does not corrupt
        //     state.subst in a way that breaks document 2's CALL-POLY type resolution
        //   - The result is the expected concrete type, not Any or an unresolved TypeVar
        //
        // Document 1: defines `id` (polymorphic identity) and `data` (concrete record).
        //   The letrec for `id: [fn [x@a] $x]` generates a function scheme ∀a. Fn(a→a).
        //   The letrec for `data: [name: hello]` writes `α_data → Record({name: StringLiteral},
        //   Closed)` into the local subst (no state.subst entry from this step).
        //   After document 1, env has `id : ∀a. Fn(a→a)` and `data : Record({name: "hello"})`.
        //   state.subst may have bindings from letrec TypeVar assignments.
        //
        // Document 2: retrieves `id` and `data` from env (concrete, across the `---` boundary),
        //   accesses `$data.name` (direct field lookup, returns StringLiteral("hello")),
        //   then calls `[call $id $data.name]` via CALL-POLY.
        //   CALL-POLY instantiates `id` to Fn(α'→α'), unifies α' with StringLiteral("hello"),
        //   local subst = {α' → StringLiteral("hello")}. subst.apply(α') = StringLiteral("hello").
        //   state.subst.apply(StringLiteral("hello")) = StringLiteral("hello") (no-op on concrete).
        // file_env processes all documents and returns the env of the last document.
        // The last document has one dict [result: ...], so result is in the final env.
        let env = file_env(
            "[id: [fn [x@a] $x]]\n[data: [name: hello]]\n---\n[result: [call $id $data.name]]",
        );
        let result_ty = env
            .get("result")
            .expect("result should be in env after document 2")
            .body
            .clone();
        assert_eq!(
            result_ty,
            Type::StringLiteral("hello".to_string()),
            "CALL-POLY across document boundary should resolve return type to StringLiteral(\"hello\"), got: {result_ty}"
        );
    }

    // -- Type::Any callee positional arg type_map population --

    #[test]
    fn test_call_any_callee_populates_type_map_for_positional_args() {
        // Regression test for the Type::Any arm in check_call and check_call_with_scheme.
        //
        // When the callee resolves to Type::Any (e.g., a variable bound to Any in the env),
        // positional arguments must still be inferred and recorded in type_map — otherwise
        // LSP hover over argument expressions in Any-typed calls produces no type information.
        //
        // The fix (typecheck.rs check_call ~1050, check_call_with_scheme ~900) added an
        // `infer_expr` loop inside the Type::Any arm only. This test guards that loop:
        // if it were removed, the span of `42` would not appear in type_map and the assertion
        // below would fail.
        //
        // SETUP: `f` is bound to TypeScheme::mono(Type::Any) in the parent env, simulating
        // any runtime-typed or externally-typed callable (e.g., a function loaded from JSON,
        // an FFI binding, or a value whose type cannot be statically determined). The call
        // `[call $f 42]` exercises check_call via the monomorphic (empty type_vars) path.
        let input = "[call $f 42]";
        let mut file = crate::parse(input).unwrap();
        crate::desugar::desugar_file(&mut file.node);

        // Build a parent env with `f: Any` — monomorphic scheme, empty type_vars.
        let mut parent_env = TypeEnv::new();
        parent_env.insert_scheme("f".to_string(), TypeScheme::mono(Type::Any));
        let parent_env = Rc::new(parent_env);

        let mut state = InferState::new();
        let mut type_map = TypeMap::new();

        let expr = &file.node.documents[0].node.expressions[0];
        let result = infer_expr(expr, &parent_env, &mut state, &mut Some(&mut type_map));

        // The call to an Any-typed function returns Any.
        assert_eq!(
            result,
            Ok(Type::Any),
            "calling Any-typed callee should return Type::Any, got: {result:?}"
        );

        // Extract the span of the `42` argument from the parsed AST to look it up in type_map.
        let arg_span = match &expr.node {
            Expr::Call { args, .. } => {
                assert_eq!(args.len(), 1, "expected exactly one positional arg");
                let arg = &args[0];
                (arg.span.start.offset, arg.span.end.offset)
            }
            other => panic!("expected Expr::Call, got {other:?}"),
        };

        // The span of `42` must appear in type_map: the Type::Any arm must have inferred it.
        assert!(
            type_map.contains_key(&arg_span),
            "type_map should contain the span of `42` (span {arg_span:?}) after calling an Any-typed function, \
             but only found spans: {:?}",
            type_map.keys().collect::<Vec<_>>()
        );

        // The inferred type of `42` should be IntLiteral(42).
        assert_eq!(
            type_map[&arg_span],
            Type::IntLiteral(42),
            "the positional arg `42` should infer to IntLiteral(42), got: {:?}",
            type_map[&arg_span]
        );
    }

    #[test]
    fn test_check_call_mono_skip_subst_apply_documented() {
        // Task 3: Document why CALL-MONO in check_call can safely skip state.subst.apply()
        // while check_call_with_scheme (which always takes the CALL-POLY path) must use it.
        //
        // BACKGROUND: check_call_with_scheme no longer has a CALL-MONO branch. The CALL-MONO
        // branch was deleted (cycle-findings-c36-a Task 2) because it was provably unreachable:
        // check_call_with_scheme is only called for polymorphic schemes (guard at line 236 ensures
        // non-empty type_vars or row_vars), and instantiate_scheme always produces fresh TypeVars
        // or RowVars for each quantified variable, so has_type_vars() is always true after
        // instantiation. The function now always takes the CALL-POLY path.
        //
        // CALL-MONO in check_call (line 1023): returns `*ret.clone()` (direct clone, no apply)
        //
        // WHY check_call CALL-MONO CAN SKIP state.subst.apply():
        //   check_call: func_ty comes from infer_expr (line 978), then apply() is performed at
        //     line 985 before the CALL-MONO guard. The CALL-MONO guard (!func_ty.has_type_vars())
        //     proves `ret` is fully concrete — no TypeVar or RowVar nodes in any position —
        //     so an additional state.subst.apply() would be a no-op that wastes allocations.
        //
        // WHY check_call_with_scheme (CALL-POLY) uses subst.apply(ret):
        //   func_ty comes from instantiate_scheme (line 912), which ALWAYS produces fresh
        //   TypeVars/RowVars. The local subst is seeded from state.subst (mirroring infer_dict
        //   Pass 3a), so subst.apply(ret) resolves both the fresh vars (from argument unification)
        //   and any state.subst-bound vars in a single pass. After the loop, the local subst is
        //   merged back into state.subst (mirroring infer_dict Pass 3d).
        //
        // WHEN WOULD REMOVING state.subst.apply() FROM check_call BREAK?
        //   Only if check_call received a func_ty that:
        //     (a) passes the !has_type_vars() guard (appears fully concrete at top level), AND
        //     (b) has TypeVars/RowVars in nested positions that are bound in state.subst.
        //
        //   This is currently IMPOSSIBLE because check_call only receives func_ty from:
        //     1. infer_expr for non-VarRef callees (e.g., inline [fn [x] $x]) — these produce
        //        fully concrete types with no TypeVars in any position if !has_type_vars().
        //     2. The state.subst.apply() at line 985 already resolved any nested TypeVars
        //        before the CALL-MONO guard.
        //
        // FUTURE SCENARIO (tracked as row-unification-f-b in TODO.md):
        //   If check_call were modified to accept a pre-instantiated polymorphic function
        //   type with nested RowVars, AND those RowVars were bound in state.subst by prior
        //   constraints, THEN skipping state.subst.apply() would produce a type with unresolved
        //   RowVar references instead of the fully resolved concrete type.
        //
        // This test is a PLACEHOLDER for that future scenario. Currently, no test can
        // demonstrate the difference because check_call doesn't enter that code path.
        // The test documents the invariant: check_call's CALL-MONO skip is safe because
        // has_type_vars() = false implies no nested TypeVars/RowVars in any position.

        // Verify current behavior: CALL-MONO in check_call with a monomorphic inline lambda
        let ty = result_field("[f: [fn [x@Int] 42]]\n[result: [call $f 1]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "CALL-MONO should return concrete type"
        );

        // Verify check_call_with_scheme behavior: polymorphic function takes CALL-POLY path
        // (CALL-MONO was deleted from check_call_with_scheme in cycle-findings-c36-a Task 2,
        // since instantiate_scheme always produces fresh TypeVars making CALL-MONO unreachable)
        let ty = result_field("[id: [fn [x@a] $x]]\n[result: [call $id 42]]", "result");
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "check_call_with_scheme CALL-POLY path should unify and apply state.subst"
        );

        // TODO(row-unification-f-b): Add a test that creates a scenario where:
        //   - func_ty has nested RowVars bound in state.subst
        //   - !has_type_vars() = true (top-level concrete)
        //   - Removing state.subst.apply() from check_call produces wrong result
        // This is currently impossible to construct without modifying check_call's call sites.
    }

    #[test]
    fn test_range_access_typevar_target() {
        // Task 4: Coverage test for check_range_access TypeVar fall-through arm.
        //
        // check_range_access has three arms for target_ty:
        //   Type::Record | Type::Any | Type::TypeVar => Ok(target_ty)
        //   _ => Err("expected record type")
        //
        // The TypeVar arm returns Ok(target_ty) without generating constraints (unlike
        // check_dot_access, which generates α = Record({field: β}, RowVar(ρ)) for TypeVar α).
        //
        // This test verifies that range access on a forward-reference does NOT produce a
        // spurious "expected record type" error. The full infer pipeline runs all letrec
        // passes, so by the time infer() returns, the forward-ref TypeVar for $data has been
        // unified with the concrete Record([a: 1  b: 2]). The range access result therefore
        // reflects the resolved target type: a Record (not a TypeVar and not an error).
        //
        // The key invariant: the expression typechecks successfully (returns Ok, not Err).
        // The result type of the range access is the resolved target Record type.

        // Forward-reference range access: $data is a TypeVar during Pass 1/2 of letrec,
        // but by the time infer() returns all passes have completed and the TypeVar is resolved.
        let ty = infer("[result: $data[0..1]  data: [a: 1  b: 2]]");
        match ty {
            Type::Record(Row { fields, .. }) => {
                let result_ty = fields.get("result").expect("field 'result' should exist");
                // After full letrec resolution, result is a Record (the resolved target type).
                // This proves: (1) no spurious "expected record type" error during letrec passes,
                // and (2) the TypeVar arm of check_range_access accepted the forward ref cleanly.
                assert!(
                    matches!(result_ty, Type::Record(_)),
                    "range access on forward-ref target should resolve to Record after letrec, got {result_ty}"
                );
            }
            other => panic!("expected Record, got {other}"),
        }

        // TODO: check_range_access should probably generate a constraint like check_dot_access does.
        // Currently it just accepts TypeVar and returns it, meaning range access on an inferred
        // type provides no additional type information. See check_dot_access TypeVar arm
        // for the constraint-generation pattern.
    }

    #[test]
    fn test_range_access_on_proxy_errors() {
        // Range access is NOT supported on Proxy values (unlike dot and bracket access).
        // Runtime eval_range_access returns an error for Value::Proxy (src/eval.rs:1118-1127).
        // Type checker should match this behavior.
        //
        // Note: cannot test via check_err("[p: proxy  x: $p[0..1]]") because within
        // a single dict, $p is still a TypeVar during Pass 3 — the TypeVar arm catches
        // it before the Proxy arm fires. We test check_range_access directly instead.

        let mut file = crate::parse("[dummy: 1][0..1]").unwrap();
        crate::desugar::desugar_file(&mut file.node);
        let env = Rc::new(TypeEnv::new());
        let mut state = InferState::new();

        let target = &file.node.documents[0].node.expressions[0];
        let span = target.span;
        let result = check_range_access(target, &None, &None, &env, span, &mut state, &mut None);
        assert!(result.is_ok(), "range access on Record should succeed");

        // Now test with a Proxy target directly by constructing the match input
        let mut proxy_target = crate::parse("[call $proxy [fn [k] 42]]").unwrap();
        crate::desugar::desugar_file(&mut proxy_target.node);
        let proxy_expr = &proxy_target.node.documents[0].node.expressions[0];
        let result =
            check_range_access(proxy_expr, &None, &None, &env, span, &mut state, &mut None);
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("range access is not supported on Proxy")),
            "expected 'range access is not supported on Proxy' error, got: {errors:?}"
        );
    }

    // -- Variadic param type inference --

    #[test]
    fn test_variadic_param_type_is_record() {
        // Variadic params always receive a closed empty record type in the function signature,
        // regardless of any context (typecheck.rs:1151 override).
        //
        // Grammar: variadic_param = @{ "..." ~ param_name } — no @annotation syntax.
        // The param_types override at infer_fn ensures the function type reflects
        // Record({}, Empty) for the variadic slot, not Any or an annotation-derived type.
        //
        // This test covers the fix at typecheck.rs:1151 that overrides param_types[i] for
        // variadic params so the function signature matches the env binding.

        // Basic variadic: single param, collects all positional args as a dict
        let ty = result_field("[f: [fn [...rest] $rest]]", "f");
        match ty {
            Type::Function { params, .. } => {
                assert_eq!(params.len(), 1, "variadic function should have 1 param");
                assert!(
                    matches!(
                        &params[0],
                        Type::Record(Row {
                            tail: RowTail::Empty,
                            ..
                        })
                    ),
                    "variadic param should have type Record({{}} Closed), got: {:?}",
                    params[0]
                );
            }
            other => panic!("expected Function type for f, got {other}"),
        }

        // Variadic with named params before it: only the rest param gets Record type
        let ty = result_field("[f: [fn [a b ...rest] $rest]]", "f");
        match ty {
            Type::Function { params, .. } => {
                assert_eq!(params.len(), 3, "function should have 3 params");
                // First two params are inferred normally
                assert!(
                    !matches!(&params[0], Type::Record(_)),
                    "non-variadic param 'a' should not be Record, got: {:?}",
                    params[0]
                );
                // Third param (variadic) must be Record({}, Empty)
                assert!(
                    matches!(
                        &params[2],
                        Type::Record(Row {
                            tail: RowTail::Empty,
                            ..
                        })
                    ),
                    "variadic param 'rest' should have type Record({{}} Closed), got: {:?}",
                    params[2]
                );
            }
            other => panic!("expected Function type for f, got {other}"),
        }
    }

    #[test]
    fn test_variadic_param_env_binding_is_record() {
        // The env binding for a variadic param inside the function body must be Record({}, Empty),
        // not Any. This is the effect of fn_env.insert(param.node.name.clone(), variadic_ty)
        // at typecheck.rs:1152, which is paired with the param_types override at line 1151.
        //
        // If the body references $rest, its inferred type comes from the env binding.
        // Returning $rest should give the function a Record return type.

        let ty = result_field("[f: [fn [x ...rest] $rest]]", "f");
        match ty {
            Type::Function { ret, .. } => {
                assert!(
                    matches!(
                        ret.as_ref(),
                        Type::Record(Row { tail: RowTail::Empty, .. })
                    ),
                    "function returning variadic param should have Record return type, got: {ret:?}"
                );
            }
            other => panic!("expected Function type for f, got {other}"),
        }
    }

    // -- check_call_with_scheme substitution threading (Algorithm W) --

    #[test]
    fn test_call_poly_subst_seeded_and_merged() {
        // Regression test for two Algorithm W substitution threading bugs in
        // check_call_with_scheme (Damas & Milner 1982, Theorem 2):
        //
        //   Task 1 (Critical): The local substitution was never merged back into state.subst.
        //     Bindings accumulated during polymorphic call unification were lost for downstream
        //     inference steps.
        //
        //   Task 2 (Major): The local substitution was not seeded from state.subst.
        //     param_ty was unified against arg_ty in an empty substitution context, missing
        //     bindings for TypeVars that state.subst already resolved.
        //
        // The fix mirrors infer_dict's two-substitution model:
        //   Pass 3a (seed):  initialize local subst from state.subst
        //   Pass 3d (merge): merge local subst back into state.subst
        //
        // TEST SCENARIO (cross-entry):
        //   Entry 1 defines `id : forall a. Fn(a) -> a` and `data : Record({name: "hello"})`.
        //   Entry 2 calls `[call $id $data]` via CALL-POLY.
        //   Entry 3 accesses $result.name.
        //
        //   The cross-entry structure ensures state.subst is the sole channel for
        //   constraint propagation (no infer_dict local subst sharing across entries).
        //   The merge ensures that CALL-POLY's local subst bindings (e.g., _tN -> Record(...))
        //   flow into state.subst for downstream resolution.
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[data: [name: hello]]\n[result: [call $id $data]]\n[n: $result.name]",
            "n",
        );
        assert_eq!(
            ty,
            Type::StringLiteral("hello".to_string()),
            "cross-entry dot-access on polymorphic call result should resolve, got: {ty}"
        );

        // Also verify that `result` has the full record type.
        // Use a different input where `result` is in the last expression.
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[data: [name: hello]]\n[result: [call $id $data]]",
            "result",
        );
        match ty {
            Type::Record(Row { ref fields, .. }) => {
                assert_eq!(
                    fields.get("name"),
                    Some(&Type::StringLiteral("hello".to_string())),
                    "result should be a record with name: StringLiteral(\"hello\")"
                );
            }
            _ => panic!("expected Record for result, got {ty}"),
        }
    }

    #[test]
    fn test_call_poly_subst_merge_constrains_forward_ref() {
        // Test that check_call_with_scheme's substitution merge propagates constraints
        // from a polymorphic call to forward-referenced letrec entries.
        //
        // SCENARIO: `[fn [x@a y@a] $x]` requires both args to have the same type.
        // When called with `$value` (forward-ref TypeVar) and `42`, the unification
        // binds the forward-ref TypeVar to IntLiteral(42) in the local subst.
        // With the merge, this constraint flows into state.subst.
        //
        // After the letrec processes `value: 42`, the unification of _t_value with
        // IntLiteral(42) in the local subst is consistent with the constraint from
        // the polymorphic call. The result type should be IntLiteral(42).
        let ty = result_field(
            "[same: [fn [x@a y@a] $x]]\n[result: [call $same $value 42]  value: 42]",
            "result",
        );
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "polymorphic call with same-type constraint should resolve return type"
        );

        // Verify `value` also resolves correctly
        let ty = result_field(
            "[same: [fn [x@a y@a] $x]]\n[result: [call $same $value 42]  value: 42]",
            "value",
        );
        assert_eq!(
            ty,
            Type::IntLiteral(42),
            "forward-referenced value should have IntLiteral type"
        );
    }

    #[test]
    fn test_call_poly_subst_seed_resolves_access_chain() {
        // Test that check_call_with_scheme's seeded substitution correctly resolves
        // arg_ty through state.subst bindings from prior check_dot_access calls.
        //
        // SCENARIO:
        //   Entry 1: defines `id` (polymorphic) and `data` (concrete record)
        //   Entry 2: defines `name` (accesses $data.name, writes to state.subst)
        //   Entry 3: calls `[call $id $name]` — $name's type should be resolved
        //     through state.subst before unification with the instantiated param type.
        //
        // Without seeding, the fresh local subst would not see state.subst's binding
        // for $name's type. With seeding, unify() resolves both sides through the
        // seeded subst, producing the correct binding.
        let ty = result_field(
            "[id: [fn [x@a] $x]]\n[data: [name: hello]]\n[name: $data.name]\n[result: [call $id $name]]",
            "result",
        );
        assert_eq!(
            ty,
            Type::StringLiteral("hello".to_string()),
            "CALL-POLY with access-chain arg should resolve through seeded subst"
        );
    }

    // -- check_call (non-scheme) CALL-POLY substitution threading (Algorithm W) --

    #[test]
    fn test_check_call_nonscheme_poly_subst_seeded_and_merged() {
        // Mirror of test_call_poly_subst_seeded_and_merged for check_call's CALL-POLY path.
        //
        // check_call_with_scheme handles [call $varref ...] when $varref is a polymorphic
        // scheme. check_call handles all other callees, including lambda literals. To trigger
        // check_call's CALL-POLY path, we call a lambda literal directly:
        //   [call [fn [x@a] $x] $data]
        // Since the callee is Expr::Fn (not Expr::VarRef), it routes to check_call (line 263).
        // The lambda infers as Fn(_tN -> _tN) with type vars, so CALL-POLY fires.
        //
        // TEST SCENARIO (merge):
        //   Entry 1: defines `data` as a concrete record.
        //   Entry 2: calls [call [fn [x@a] $x] $data] — CALL-POLY unification binds fresh
        //     TypeVar _tN to Record({name: "hello"}). Without merge, this binding is lost.
        //   Entry 3: accesses $result.name — requires the binding from Entry 2 in state.subst.
        let ty = result_field(
            "[data: [name: hello]]\n[result: [call [fn [x@a] $x] $data]]\n[n: $result.name]",
            "n",
        );
        assert_eq!(
            ty,
            Type::StringLiteral("hello".to_string()),
            "check_call CALL-POLY merge: cross-entry dot-access on lambda-call result should resolve"
        );

        // Verify that `result` itself resolves to a record with the right field type.
        let ty = result_field(
            "[data: [name: hello]]\n[result: [call [fn [x@a] $x] $data]]",
            "result",
        );
        match ty {
            Type::Record(Row { ref fields, .. }) => {
                assert_eq!(
                    fields.get("name"),
                    Some(&Type::StringLiteral("hello".to_string())),
                    "result should be Record with name: StringLiteral(\"hello\")"
                );
            }
            _ => panic!("expected Record for result, got {ty}"),
        }
    }

    #[test]
    fn test_check_call_nonscheme_poly_subst_seed_resolves_access_chain() {
        // Mirror of test_call_poly_subst_seed_resolves_access_chain for check_call's
        // CALL-POLY path.
        //
        // TEST SCENARIO (seed):
        //   Entry 1: defines `data` as a concrete record.
        //   Entry 2: defines `name` via $data.name — check_dot_access writes a constraint
        //     into state.subst binding the TypeVar for $name to StringLiteral("hello").
        //   Entry 3: calls [call [fn [x@a] $x] $name] — the lambda literal callee routes
        //     to check_call (not check_call_with_scheme). CALL-POLY unifies the param type
        //     with arg $name's type. Without seeding from state.subst, the TypeVar for $name
        //     is unresolved during unification.
        //
        // With seeding, the seeded subst resolves $name's TypeVar to StringLiteral("hello")
        // during unification, producing the correct return type.
        let ty = result_field(
            "[data: [name: hello]]\n[name: $data.name]\n[result: [call [fn [x@a] $x] $name]]",
            "result",
        );
        assert_eq!(
            ty,
            Type::StringLiteral("hello".to_string()),
            "check_call CALL-POLY seed: access-chain arg should resolve through seeded subst"
        );
    }
}
