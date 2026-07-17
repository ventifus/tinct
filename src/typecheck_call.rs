//! Call type checking.
//!
//! Contains:
//! - `is_concrete_type` — boundary guard predicate for gradual typing
//! - `check_call_args` — shared argument checker (CALL-MONO and CALL-POLY paths)
//! - `widen_literal_types` — literal type widening for variadic argument unification

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use super::{check_surface_expr, contains_unknown_or_top, infer_surface_expr, TypeMap};
use crate::ast::{Span, Spanned, SurfaceExpression, SurfaceNamedArg, SurfaceNode};
use crate::env::Env;
use crate::type_errors::{ArityMismatch, TypeErrorTyped, UnificationFailure};
use crate::types::{unify, Constraint, InferState, Row, Type, TypeError};

/// Widen literal types in a type, recursively through Record fields.
///
/// Promotes `IntLiteral(n)` to `Int` and `StringLiteral(s)` to `Str` at every
/// level of the type.
///
/// This is needed in all positional argument unification paths where a polymorphic
/// type variable could be bound to a literal type by one argument and then fail to
/// unify against a different literal value for the next argument.  Classic example:
/// `[> x 10]` where `x = 5` infers `IntLiteral(5)` and the literal `10` infers
/// `IntLiteral(10)`.  Without widening, the `>` builtin's `∀a. Fn(a a → Bool)`
/// instantiates `?₀` to `IntLiteral(5)` on the first argument, then
/// `unify(IntLiteral(5), IntLiteral(10))` fails on the second.  Widening both
/// to `Int` before unification lets them unify correctly. (B-384)
///
/// Also needed for variadic arguments: `[f [1 2] [3 4]]` where the first record
/// argument would bind the variadic TypeVar to
/// `Record({0: IntLiteral(1), 1: IntLiteral(2)})` and the second would fail with
/// `IntLiteral(1) ≠ IntLiteral(3)`.  Widening both records to
/// `Record({0: Int, 1: Int})` lets them unify correctly.
pub(crate) fn widen_literal_types(ty: Type) -> Type {
    match ty {
        Type::IntLiteral(_) => Type::Int,
        Type::StringLiteral(_) => Type::Str,
        Type::Dict(row) => {
            let widened_fields = row
                .fields
                .into_iter()
                .map(|(k, v)| (k, widen_literal_types(v)))
                .collect();
            Type::Dict(Row {
                fields: widened_fields,
                tail: row.tail,
            })
        }
        other => other,
    }
}

/// Check if a type is concrete (not Unknown, not a TypeVar, not Top).
/// Used for boundary guard detection in gradual typing.
pub(crate) fn is_concrete_type(ty: &Type) -> bool {
    match ty {
        // Non-concrete: open inference variables or imprecise top types.
        // Top is the "any" type (like dynamic/unknown) — not a concrete constraint.
        Type::Unknown | Type::TypeVar(_, _) | Type::Any => false,
        // Composite types: recurse into components.
        Type::Function { params, ret, .. } => {
            params.iter().all(|(_, p)| is_concrete_type(p)) && is_concrete_type(ret)
        }
        Type::Dict(row) => row.fields.values().all(is_concrete_type),
        Type::App(f, arg) => is_concrete_type(f) && is_concrete_type(arg),
        Type::TyCon(_) => true, // TyCon is always concrete
        Type::Union(types) => types.iter().all(is_concrete_type),
        Type::Intersection(types) => types.iter().all(is_concrete_type),
        // Ground types: Int, Float, Str, Bool, Never, Negation, App, TypeStageApp, etc.
        // TypeStageApp is treated as concrete here: it is constructed by the resolver
        // cache from ground types, so it is fully determined at the point boundary
        // guards are checked.
        _ => true,
    }
}

/// Shared argument checker for the CALL-MONO and CALL-POLY paths.
///
/// Takes already-unwrapped function type fields (`params`, `ret`, `variadic`,
/// `required_count`) and the `is_poly` flag to select the argument checking strategy:
///
/// - `is_poly: true` — CALL-POLY path: infer each arg with `infer_surface_expr` then
///   unify against the param type.
/// - `is_poly: false` — CALL-MONO path: use `check_surface_expr` for lambda args and
///   the subsumption-based path for all other args.
///
/// **Deferred arity check:** all supplied args are inferred before the arity check fires,
/// so `got_types` in the `ArityMismatch` error is naturally populated with the actual
/// argument types instead of being left empty.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn check_call_args(
    params: &[(Option<String>, Type)],
    ret: &Type,
    variadic: bool,
    required_count: usize,
    func_name: Option<&str>,
    args: &[Arc<SurfaceNode>],
    named_args: &[Spanned<SurfaceNamedArg>],
    env: &Arc<RwLock<Env>>,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    type_map: &mut Option<&mut TypeMap>,
    constraints_start: usize,
    is_poly: bool,
) -> Result<Type, Vec<TypeError>> {
    let total_supplied = args.len() + named_args.len();
    // B-349: use required_count (params without default values) as the minimum.
    // For variadic functions, the last (variadic) param is not required.
    let min_required = if variadic && !params.is_empty() {
        required_count.saturating_sub(1)
    } else {
        required_count
    };
    let non_variadic_param_count = if variadic && !params.is_empty() {
        params.len() - 1
    } else {
        params.len()
    };

    // Infer ALL positional arg types (even extras), so that:
    //   1. `got_types` is populated for arity-mismatch errors with meaningful messages.
    //   2. Type map entries are recorded for all args regardless of arity.
    //   3. Unification still runs for in-range args on the happy path.
    //
    // On the CALL-POLY path we always infer+unify.
    // On the CALL-MONO path we use check_surface_expr / subsumption for in-range args.
    let mut arg_types: Vec<Type> = Vec::with_capacity(args.len());
    let mut arg_errors: Option<Vec<TypeError>> = None;

    if is_poly {
        // CALL-POLY: infer every arg, collect errors (don't stop on first failure).
        // Errors are stored inside the Error(errs) node for later extraction.
        for a in args {
            match infer_surface_expr(a, env, state, type_map).await {
                Ok(ty) => arg_types.push(ty),
                Err(errs) => {
                    arg_types.push(Type::error_with(
                        errs.into_iter()
                            .map(|e| crate::type_errors::TypeErrorTyped::from(e))
                            .collect(),
                    ));
                }
            }
        }
    } else {
        // CALL-MONO: for args that fall within the valid param range, use the proper
        // bidirectional / subsumption check; for extra args, just infer for got_types.
        for (idx, arg) in args.iter().enumerate() {
            let param_ty_opt = if idx < non_variadic_param_count {
                params.get(idx).map(|(_, ty)| ty)
            } else if variadic {
                // Variadic extra args — no per-arg infer here, handled below
                None
            } else {
                None // beyond params.len() — extra arg
            };

            match param_ty_opt {
                Some(param_ty) => {
                    // In-range non-variadic arg: bidirectional check.
                    match &arg.expr {
                        SurfaceExpression::Fn { .. } => {
                            // Lambda: use check_surface_expr for bidirectional lambda checking.
                            if let Err(mut errs) =
                                check_surface_expr(arg, param_ty, env, state, type_map).await
                            {
                                arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                            }
                            // Infer for type-string display (check_surface_expr doesn't return type).
                            // Use Unknown as placeholder; the check already ran above.
                            arg_types.push(Type::Unknown);
                        }
                        _ => {
                            // Non-lambda: infer once, apply subst, boundary-guard, subsume.
                            match infer_surface_expr(arg, env, state, type_map).await {
                                Ok(arg_ty) => {
                                    let arg_ty_resolved = if state.subst_is_empty() {
                                        arg_ty.clone()
                                    } else {
                                        state.apply(&arg_ty)
                                    };
                                    // Boundary guard: Unknown→concrete boundary.
                                    if is_concrete_type(param_ty)
                                        && matches!(arg_ty_resolved, Type::Unknown)
                                    {
                                        arg.type_guard.set(Some(param_ty.clone()));
                                    }
                                    let param_ty_resolved = if state.subst_is_empty() {
                                        param_ty.clone()
                                    } else {
                                        state.apply(param_ty)
                                    };
                                    let sub_passes =
                                        Type::is_subtype(
                                            &arg_ty_resolved,
                                            &param_ty_resolved,
                                            None,
                                        ) || ((contains_unknown_or_top(&arg_ty_resolved)
                                            || contains_unknown_or_top(&param_ty_resolved))
                                            && Type::is_consistent(
                                                &arg_ty_resolved,
                                                &param_ty_resolved,
                                            ));
                                    if !sub_passes {
                                        arg_errors.get_or_insert_with(Vec::new).push(
                                            TypeError::from(TypeErrorTyped::UnificationFailure(
                                                UnificationFailure {
                                                    expected: param_ty_resolved,
                                                    got: arg_ty_resolved.clone(),
                                                    span: arg.span.clone(),
                                                    notes: vec![],
                                                    call_stack: vec![],
                                                },
                                            )),
                                        );
                                    }
                                    arg_types.push(arg_ty_resolved);
                                }
                                Err(errs) => {
                                    arg_types.push(Type::error_with(
                                        errs.into_iter()
                                            .map(crate::type_errors::TypeErrorTyped::from)
                                            .collect(),
                                    ));
                                }
                            }
                        }
                    }
                }
                None => {
                    // Extra arg beyond param range (or variadic handled below): just infer.
                    match infer_surface_expr(arg, env, state, type_map).await {
                        Ok(ty) => arg_types.push(ty),
                        Err(errs) => {
                            arg_types.push(Type::error_with(
                                errs.into_iter()
                                    .map(crate::type_errors::TypeErrorTyped::from)
                                    .collect(),
                            ));
                        }
                    }
                }
            }
        }
    }

    // Build got_types string representations from the inferred types.
    let got_type_strs: Vec<String> = arg_types.iter().map(|ty| format!("{}", ty)).collect();

    // Extract errors stored inside Error(errs) type nodes and fold them into arg_errors.
    // These are inference failures from the zip loop above; they were stored in the type node
    // (rather than discarded) so that got_type_strs above can show a meaningful display string.
    for ty in &arg_types {
        let payload = ty.error_payload();
        if !payload.is_empty() {
            arg_errors
                .get_or_insert_with(Vec::new)
                .extend(payload.iter().map(|e| TypeError::from(e.clone())));
        }
    }

    // Arity mismatch: return only the arity error. Inference errors from Error(errs) arg
    // nodes are visible in got_types and are reported at their original call sites through
    // the normal type-checking pipeline — re-propagating them here causes cascade.
    if total_supplied < min_required || (!variadic && total_supplied > params.len()) {
        let param_descriptions: Vec<String> = params
            .iter()
            .map(|(name, ty)| {
                if let Some(n) = name {
                    format!("{}: {}", n, ty)
                } else {
                    format!("{}", ty)
                }
            })
            .collect();
        return Err(vec![TypeError::from(TypeErrorTyped::ArityMismatch(
            ArityMismatch {
                expected: min_required,
                got: total_supplied,
                span,
                notes: if variadic || !named_args.is_empty() {
                    vec![format!(
                        "{} positional, {} named",
                        args.len(),
                        named_args.len()
                    )]
                } else {
                    vec![]
                },
                call_stack: vec![],
                callee: func_name.map(|s| s.to_string()),
                params: param_descriptions,
                got_types: got_type_strs,
            },
        ))]);
    }

    if params.is_empty() {
        // Zero-param: no arguments to unify.
        if let Some(errors) = arg_errors {
            return Err(errors);
        }
        return if state.subst_is_empty() {
            Ok((*ret).clone())
        } else {
            Ok(state.apply(ret))
        };
    }

    if is_poly {
        // CALL-POLY unification pass.
        //
        // All bindings go directly into state.type_vars — no separate substitution needed.
        // unify() reads and writes state.type_vars, which already contains all accumulated
        // bindings from prior inference steps (Damas & Milner 1982, Theorem 2).
        //
        // Track consumed param indices to prevent named args from overlapping with positional args.
        // C-NO-OVERLAP: positional args consume params 0..args.len(). Named args searching
        // ALL params by name could accidentally match a positional-consumed param.
        let mut consumed_params = std::collections::HashSet::new();

        // T013 Task 4: Pre-collect the type vars in each param type so we can update
        // constraint origin_span to per-argument spans after unification.
        let param_vars_per_idx: Vec<HashSet<String>> = params
            .iter()
            .take(non_variadic_param_count)
            .map(|(_, param_ty)| {
                let mut vars = HashSet::new();
                param_ty.collect_type_vars(&mut vars);
                vars
            })
            .collect();

        for (idx, ((_, param_ty), arg_ty)) in params
            .iter()
            .take(non_variadic_param_count)
            .zip(arg_types.iter())
            .enumerate()
        {
            consumed_params.insert(idx);

            // Boundary guard tracking.
            if matches!(arg_ty, Type::Unknown) && is_concrete_type(param_ty) {
                if idx < args.len() {
                    args[idx].type_guard.set(Some(param_ty.clone()));
                }
            }

            // Error-typed args absorb silently (unify(Error, T) = Ok(())).
            if let Err(e) =
                Box::pin(unify(param_ty, arg_ty, state, constraints, span.clone())).await
            {
                arg_errors.get_or_insert_with(Vec::new).push(e);
            }
        }

        // T013 Task 4: Update constraint origin_span to per-argument span.
        let mut var_to_arg_span: HashMap<String, Span> =
            HashMap::with_capacity(param_vars_per_idx.len() * 2);
        for (idx, param_vars) in param_vars_per_idx.iter().enumerate() {
            if idx < args.len() {
                for var in param_vars {
                    var_to_arg_span
                        .entry(var.clone())
                        .or_insert_with(|| args[idx].span.clone());
                }
            }
        }
        if !var_to_arg_span.is_empty() {
            for c in constraints[constraints_start..].iter_mut() {
                if let crate::type_class::Constraint::Class {
                    vars, origin_span, ..
                } = c
                {
                    let best_span = vars
                        .iter()
                        .find_map(|v| v.as_var().and_then(|s| var_to_arg_span.get(s)));
                    if let Some(new_span) = best_span {
                        *origin_span = Some(new_span.clone());
                    }
                }
            }
        }

        // Check variadic args: unify all arg_types starting at non_variadic_param_count
        // against the variadic param element type.
        if variadic && arg_types.len() > non_variadic_param_count {
            if let Some((_, variadic_param_ty)) = params.last() {
                let elem_ty: Option<Type> = if let Type::Dict(row) = variadic_param_ty {
                    match &row.tail {
                        crate::type_def::RowTail::Uniform { value, .. } => Some(*value.clone()),
                        _ => None,
                    }
                } else if let Type::App(f, arg) = variadic_param_ty {
                    if matches!(f.as_ref(), Type::TyCon(n) if n == "Seq") {
                        Some(*arg.clone())
                    } else {
                        None
                    }
                } else if matches!(variadic_param_ty, Type::TypeVar(_, _)) {
                    Some(variadic_param_ty.clone())
                } else {
                    None
                };
                if let Some(elem_ty) = elem_ty {
                    for arg_ty in arg_types.iter().skip(non_variadic_param_count) {
                        let widened_ty = match arg_ty {
                            Type::IntLiteral(_) => Type::Int,
                            Type::StringLiteral(_) => Type::Str,
                            other => other.clone(),
                        };
                        if let Err(e) = Box::pin(unify(
                            &elem_ty,
                            &widened_ty,
                            state,
                            constraints,
                            span.clone(),
                        ))
                        .await
                        {
                            arg_errors.get_or_insert_with(Vec::new).push(e);
                        }
                    }
                }
            }
        }

        // Check for duplicate named argument names.
        let mut seen_names: HashSet<&str> = HashSet::new();
        for na in named_args {
            if !seen_names.insert(&na.node.name) {
                arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                    format!("duplicate named argument: '{}'", na.node.name),
                    na.span.clone(),
                ));
            }
        }

        // Unify named args by matching them to params by name.
        for na in named_args {
            let arg_name = &na.node.name;
            let param_match = params.iter().enumerate().find_map(|(idx, (pname, pty))| {
                if pname.as_ref() == Some(arg_name) {
                    Some((idx, pty))
                } else {
                    None
                }
            });

            match param_match {
                Some((param_idx, param_ty)) => {
                    if consumed_params.contains(&param_idx) {
                        arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                            format!("named argument '{}' conflicts with positional argument at position {}", arg_name, param_idx),
                            na.span.clone(),
                        ));
                        continue;
                    }
                    consumed_params.insert(param_idx);
                    match infer_surface_expr(&na.node.value, env, state, type_map).await {
                        Ok(arg_ty) => {
                            if let Err(e) = Box::pin(unify(
                                &arg_ty,
                                param_ty,
                                state,
                                constraints,
                                na.span.clone(),
                            ))
                            .await
                            {
                                arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                                    format!(
                                        "named argument '{}' type mismatch: {}",
                                        arg_name, e.message
                                    ),
                                    na.span.clone(),
                                ));
                            }
                        }
                        Err(mut errs) => {
                            arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                        }
                    }
                }
                None => {
                    // B-310: Variadic functions accept arbitrary named args.
                    if !variadic {
                        arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                            format!(
                                "unknown named argument: function has no parameter named '{}'",
                                arg_name
                            ),
                            na.span.clone(),
                        ));
                    } else {
                        if let Err(mut errs) =
                            infer_surface_expr(&na.node.value, env, state, type_map).await
                        {
                            arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                        }
                    }
                }
            }
        }

        state.check_type_vars_size(span).map_err(|e| vec![e])?;
        if let Some(errors) = arg_errors {
            return Err(errors);
        }
        Ok(state.apply(ret))
    } else {
        // CALL-MONO: arg types already checked above (bidirectional / subsumption).
        // Now handle variadic extra args (they were inferred but not checked above).
        if variadic && args.len() > non_variadic_param_count {
            let last_seq_elem = params.last().and_then(|(_, t)| {
                if let Type::Dict(row) = t {
                    if let crate::type_def::RowTail::Uniform { value, .. } = &row.tail {
                        return Some(*value.clone());
                    }
                }
                if let Type::App(f, arg) = t {
                    if matches!(f.as_ref(), Type::TyCon(n) if n == "Seq") {
                        return Some(*arg.clone());
                    }
                }
                None
            });
            if let Some(elem_ty) = last_seq_elem {
                for (idx, arg) in args.iter().enumerate().skip(non_variadic_param_count) {
                    let arg_ty = &arg_types[idx];
                    let widened_ty = widen_literal_types(arg_ty.clone());
                    if let Err(e) = Box::pin(unify(
                        &widened_ty,
                        &elem_ty,
                        state,
                        constraints,
                        arg.span.clone(),
                    ))
                    .await
                    {
                        arg_errors.get_or_insert_with(Vec::new).push(e);
                    }
                }
            }
        }

        // Check for duplicate named argument names.
        let mut seen_names: HashSet<&str> = HashSet::new();
        for na in named_args {
            if !seen_names.insert(&na.node.name) {
                arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                    format!("duplicate named argument: '{}'", na.node.name),
                    na.span.clone(),
                ));
            }
        }

        // Track consumed param indices.
        let mut consumed_params: HashSet<usize> =
            (0..args.len().min(non_variadic_param_count)).collect();

        // Check named args by matching them to params by name.
        for na in named_args {
            let arg_name = &na.node.name;
            let param_match = params.iter().enumerate().find_map(|(idx, (pname, pty))| {
                if pname.as_ref() == Some(arg_name) {
                    Some((idx, pty))
                } else {
                    None
                }
            });

            match param_match {
                Some((param_idx, param_ty)) => {
                    if consumed_params.contains(&param_idx) {
                        arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                            format!("named argument '{}' conflicts with positional argument at position {}", arg_name, param_idx),
                            na.span.clone(),
                        ));
                        continue;
                    }
                    consumed_params.insert(param_idx);

                    match infer_surface_expr(&na.node.value, env, state, type_map).await {
                        Ok(arg_ty) => {
                            // Boundary guard check.
                            if is_concrete_type(param_ty) {
                                let resolved_arg_ty = if state.subst_is_empty() {
                                    arg_ty.clone()
                                } else {
                                    state.apply(&arg_ty)
                                };
                                if matches!(resolved_arg_ty, Type::Unknown) {
                                    na.node.value.type_guard.set(Some(param_ty.clone()));
                                }
                            }
                            let result = Box::pin(unify(
                                &arg_ty,
                                param_ty,
                                state,
                                constraints,
                                na.span.clone(),
                            ))
                            .await;
                            if let Err(e) = result {
                                arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                                    format!(
                                        "named argument '{}' type mismatch: {}",
                                        arg_name, e.message
                                    ),
                                    na.span.clone(),
                                ));
                            }
                        }
                        Err(mut errs) => {
                            arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                        }
                    }
                }
                None => {
                    // B-310: Variadic functions accept arbitrary named args.
                    if !variadic {
                        arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                            format!(
                                "unknown named argument: function has no parameter named '{}'",
                                arg_name
                            ),
                            na.span.clone(),
                        ));
                    } else {
                        if let Err(mut errs) =
                            infer_surface_expr(&na.node.value, env, state, type_map).await
                        {
                            arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                        }
                    }
                }
            }
        }

        if let Some(errors) = arg_errors {
            return Err(errors);
        }
        // Apply state.subst for defensive consistency.
        Ok(state.apply(ret))
    }
}
