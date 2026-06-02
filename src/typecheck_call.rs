//! Call and dot-access type checking.
//!
//! Contains:
//! - `check_dot_access` — field access type inference (string and integer keys)
//! - `check_dot_access_int` — integer dot access (`$data.0`)
//! - `is_concrete_type` — boundary guard predicate for gradual typing
//! - `check_call_with_scheme` — polymorphic call with single-shot scheme instantiation
//! - `check_call` — general function call type checking (CALL-MONO and CALL-POLY)

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use super::{check_surface_expr, contains_unknown_or_top, infer_surface_expr, TypeMap};
use crate::ast::{DotKey, Span, Spanned, SurfaceExpression, SurfaceNamedArg, SurfaceNode};
use crate::types::{
    instantiate_at_level, instantiate_scheme, unify, InferState, Row, Substitution, Type, TypeEnv,
    TypeError, TypeScheme,
};

pub(crate) fn check_dot_access(
    target: &Arc<SurfaceNode>,
    field: &DotKey,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Convert DotKey to string for field lookup
    let field_str = match field {
        DotKey::Ident(s) => s.as_str(),
        DotKey::Int(n) => return check_dot_access_int(target, *n, env, span, state, type_map),
    };

    // [DOT-POLY] fast-path: if target is a VarRef and its scheme has inner_schemes,
    // instantiate the field's scheme polymorphically
    if let SurfaceExpression::VarRef { name, .. } = &target.expr {
        if let Some(scheme) = env.get(name) {
            if let Some(ref inner_schemes) = scheme.inner_schemes {
                if let Some(field_scheme) = inner_schemes.get(field_str) {
                    // Thread origin info for T013 diagnostics: origin_name is the dot-access
                    // expression (e.g., "record.field"), origin_span is the whole access span.
                    // (No separate field-key span is available from DotKey; the whole-expression
                    // span is the closest approximation.)
                    let origin_name = format!("{}.{}", name, field_str);
                    let instantiated = instantiate_scheme(
                        field_scheme,
                        state.level,
                        state,
                        Some(origin_name.as_str()),
                        Some(span.clone()),
                    );
                    return Ok(instantiated);
                }
            }
        }
    }

    let target_ty = infer_surface_expr(target, env, state, type_map)?;
    // Apply the global accumulated substitution so that constraints from prior accesses
    // on the same target are visible (doc/07-type-extensions.md Part 5).
    let target_ty = state.subst.apply(&target_ty);
    match target_ty {
        Type::Record(Row { ref fields, .. }) => match fields.get(field_str) {
            Some(ty) => Ok(ty.clone()),
            // Gradual: BAS width subtyping — field not found in known fields, return Unknown
            // (the field may be present in the concrete value via extra fields)
            None => Ok(Type::Unknown),
        },
        // TypeVar α: generate constraint α = Record({field: β}).
        // Under BAS, no row variable needed — empty record type covers the requirement.
        Type::TypeVar(ref alpha, alpha_level) => {
            // Create fresh β for the field type
            let beta = state.fresh_type_var();

            // Build the record type to unify α with (BAS: no RowVar tail)
            let mut fields = HashMap::new();
            fields.insert(field_str.to_string(), beta.clone());
            let record_ty = Type::Record(Row { fields });

            // Unify TypeVar(α) with Record({field: β})
            let alpha_ty = Type::TypeVar(alpha.clone(), alpha_level);
            let mut subst = std::mem::take(&mut state.subst);
            let result = unify(&alpha_ty, &record_ty, &mut subst, state, span);
            state.subst = subst;
            result.map_err(|e| vec![e])?;

            Ok(beta)
        }
        // Gradual: Unknown dict — field type Unknown
        Type::Unknown => Ok(Type::Unknown),
        // Gradual: Proxy dict — field type Unknown (Proxy is opaque handle)
        Type::Proxy => Ok(Type::Unknown),
        // Intersection type: search each member for the field.
        // An intersection value satisfies all members, so any member that has the field
        // statically provides its type.  Return the first match; if no member has the
        // field statically, fall back to Unknown (a member with an open row tail may
        // accept the field dynamically, and we cannot resolve it at compile time without
        // full constraint propagation into each member's row variable).
        Type::Intersection(ref members) => {
            for member in members {
                if let Type::Record(Row { ref fields, .. }) = member {
                    if let Some(ty) = fields.get(field_str) {
                        return Ok(ty.clone());
                    }
                }
            }
            // Gradual: no Intersection member had the field statically
            Ok(Type::Unknown)
        }
        // Gradual: Negation type ~A narrows inhabitance, not field structure.
        // We cannot extract field types from a negation, so fall back to Unknown.
        Type::Negation(_) => Ok(Type::Unknown),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

/// Type check integer dot access: `$data.0`
pub(crate) fn check_dot_access_int(
    target: &Arc<SurfaceNode>,
    index: i64,
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let target_ty = infer_surface_expr(target, env, state, type_map)?;
    let target_ty = state.subst.apply(&target_ty);

    let field_name = index.to_string();

    match &target_ty {
        Type::Record(Row { ref fields, .. }) => {
            if let Some(ty) = fields.get(field_name.as_str()) {
                return Ok(ty.clone());
            }
            // Gradual: BAS width subtyping — field not found in known fields
            // (the field may be present in the concrete value via extra fields)
            Ok(Type::Unknown)
        }
        Type::TypeVar(ref alpha, alpha_level) => {
            let beta = state.fresh_type_var();

            let mut fields = HashMap::new();
            fields.insert(field_name, beta.clone());
            let record_ty = Type::Record(Row { fields });

            let alpha_ty = Type::TypeVar(alpha.clone(), *alpha_level);
            let mut subst = std::mem::take(&mut state.subst);
            let result = unify(&alpha_ty, &record_ty, &mut subst, state, span);
            state.subst = subst;
            result.map_err(|e| vec![e])?;
            Ok(beta)
        }
        // Gradual: Unknown dict — integer field type Unknown
        Type::Unknown => Ok(Type::Unknown),
        // Gradual: Proxy dict — integer field type Unknown (Proxy is opaque handle)
        Type::Proxy => Ok(Type::Unknown),
        // Intersection type: search each member for the numeric field.
        Type::Intersection(ref members) => {
            for member in members {
                if let Type::Record(Row { ref fields, .. }) = member {
                    if let Some(ty) = fields.get(field_name.as_str()) {
                        return Ok(ty.clone());
                    }
                }
            }
            // Gradual: no Intersection member had the numeric field
            Ok(Type::Unknown)
        }
        // Gradual: Negation type — fall back to Unknown for integer field access
        Type::Negation(_) => Ok(Type::Unknown),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

/// Check if a type is concrete (not Unknown, not a TypeVar, not Top).
/// Used for boundary guard detection in gradual typing.
pub(crate) fn is_concrete_type(ty: &Type) -> bool {
    match ty {
        // Non-concrete: open inference variables or imprecise top types.
        // Top is the "any" type (like dynamic/unknown) — not a concrete constraint.
        Type::Unknown | Type::TypeVar(_, _) | Type::Top => false,
        // Composite types: recurse into components.
        Type::Function { params, ret, .. } => {
            params.iter().all(|(_, p)| is_concrete_type(p)) && is_concrete_type(ret)
        }
        Type::Record(row) => row.fields.values().all(is_concrete_type),
        Type::Seq(elem) => is_concrete_type(elem),
        Type::Map(k, v) => is_concrete_type(k) && is_concrete_type(v),
        Type::Union(types) => types.iter().all(is_concrete_type),
        Type::Intersection(types) => types.iter().all(is_concrete_type),
        // Ground types: Int, Float, Str, Bool, Never, Negation, App, TypeStageApp, etc.
        // TypeStageApp is treated as concrete here: it is constructed by the resolver
        // cache from ground types, so it is fully determined at the point boundary
        // guards are checked.
        _ => true,
    }
}

/// Check a call where the function is a TypeScheme (from a VarRef lookup).
/// This avoids double instantiation: instead of VAR-POLY instantiating the scheme
/// and then CALL-POLY instantiating the result, we instantiate once here.
#[allow(clippy::too_many_arguments)] // Signature matches check_call pattern
pub(crate) fn check_call_with_scheme(
    scheme: &TypeScheme,
    func_span: Span,
    func_name: Option<&str>,
    args: &[Arc<SurfaceNode>],
    named_args: &[Spanned<SurfaceNamedArg>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Instantiate the scheme once at the current level.
    // instantiate_at_level uses the call-site level (state.level) to ensure fresh type vars
    // are created at the correct generalization depth: vars at depth > enclosing_level will
    // be generalized by the enclosing let binding, while vars at shallower depth won't be.
    //
    // Thread origin info: func_name provides the function name for T013 diagnostics,
    // ensuring "argument to `g` has unconstrained type" messages cite the callee name.
    // Record the constraint count before instantiation so we can update origin_span on
    // the new constraints to per-argument spans after argument unification (T013 Task 4).
    let constraints_start = state.constraints.len();
    let func_ty = instantiate_scheme(
        scheme,
        state.level,
        state,
        func_name,
        Some(func_span.clone()),
    );

    // Record the function expression's type in the type map for LSP hover.
    // This mirrors check_dot_access recording the target span (line ~835).
    // check_call handles this via infer_expr, which records to type_map automatically.
    // check_call_with_scheme bypasses infer_expr (to avoid double instantiation), so
    // we must record explicitly here.
    if let Some(ref mut tm) = type_map {
        let key = (func_span.start.offset, func_span.end.offset);
        tm.insert(key, func_ty.clone());
    }

    // Error cascade suppression: if the instantiated type is Error (e.g., a scheme with
    // Type::Error body — unlikely but possible if a prelude binding was recorded as Error),
    // infer arguments for side effects and return Error to cascade the failure.
    // This prevents spurious "expected function type, got <error>" on call sites when the
    // function definition itself failed type-checking. The root cause has already been reported.
    if matches!(func_ty, Type::Error) {
        // Infer positional args for type map population and error propagation.
        for arg in args {
            let _ = infer_surface_expr(arg, env, state, type_map);
        }
        // Infer named args for type map population and error propagation.
        for na in named_args {
            let _ = infer_surface_expr(&na.node.value, env, state, type_map);
        }
        return Ok(Type::Error);
    }

    match &func_ty {
        Type::Function {
            params,
            ret,
            variadic,
        } => {
            // Arity check: named args fill remaining parameter slots by name (Kotlin model in
            // eval_call.rs), so count positional + named against total params.
            let total_supplied = args.len() + named_args.len();
            // For variadic functions, the last param accepts arbitrary extra args.
            // Require at least (params.len() - 1) args for variadic functions.
            let min_required = if *variadic && !params.is_empty() {
                params.len() - 1
            } else {
                params.len()
            };
            if total_supplied < min_required || (!*variadic && total_supplied != params.len()) {
                return Err(vec![TypeError::new(
                    format!(
                        "arity mismatch: expected {}{} argument(s), got {} ({} positional, {} named)",
                        if *variadic { "at least " } else { "" },
                        min_required,
                        total_supplied,
                        args.len(),
                        named_args.len(),
                    ),
                    span,
                )]);
            }

            // CALL-POLY: After instantiation, the function type always has type variables.
            // This is guaranteed by the guard at line 236: check_call_with_scheme is only called
            // for polymorphic schemes (non-empty type_vars or row_vars), and instantiate_scheme
            // produces fresh TypeVars/RowVars for each quantified variable. Since generalize only
            // quantifies variables that appear in the body, the instantiated type must contain
            // those fresh variables, so has_inference_vars() is always true.
            // Synthesize arguments and unify (doc/06 §[CALL-POLY])
            //
            // Cascade prevention: if an argument fails inference, use Type::Error as its type
            // (the error has already been recorded in type_map by infer_expr) rather than
            // propagating the error immediately. Collect all argument errors, then report them.
            // unify(Error, param_ty) = Ok(()) by the Error-absorption rule in unify(), so the
            // rest of argument unification continues without spurious additional errors.
            debug_assert!(
                func_ty.has_inference_vars(),
                "check_call_with_scheme: func_ty must have inference vars after instantiation (invariant violated)"
            );
            let mut arg_types = Vec::with_capacity(args.len());
            let mut arg_errors: Option<Vec<TypeError>> = None;
            for a in args {
                match infer_surface_expr(a, env, state, type_map) {
                    Ok(ty) => arg_types.push(ty),
                    Err(mut errs) => {
                        arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                        arg_types.push(Type::Error);
                    }
                }
            }

            if !params.is_empty() {
                // Seed local subst from state.subst so that unification sees access-chain
                // constraints and letrec bindings accumulated by prior inference steps.
                // This mirrors infer_dict Pass 3a (lines 553-561): Algorithm W threads a
                // single substitution through inference; the two-substitution model is a
                // borrow-checker workaround. Without seeding, param_ty is unified against
                // arg_ty in an empty substitution context, missing bindings for TypeVars
                // that state.subst already resolved (Damas & Milner 1982, Theorem 2).
                //
                // Fresh type vars from instantiate_scheme are call-site-local and should not escape.
                // The local substitution is consumed by subst.apply(ret) and does not need to propagate
                // upstream — only the constraints accumulated during argument unification (merged back
                // into state.subst at lines 1475-1480) need to be visible to downstream inference.
                let mut subst = Substitution {
                    type_map: std::cell::RefCell::new(state.subst.type_map.borrow().clone()),
                };
                // Track consumed param indices to prevent named args from overlapping with positional args.
                // C-NO-OVERLAP: positional args consume params 0..args.len(). Named args searching
                // ALL params by name could accidentally match a positional-consumed param.
                let mut consumed_params = std::collections::HashSet::new();
                // If the function is variadic, stop before the last param (which is the variadic param itself).
                let non_variadic_param_count = if *variadic && !params.is_empty() {
                    params.len() - 1
                } else {
                    params.len()
                };
                // T013 Task 4: Pre-collect the type vars in each param type so we can update
                // constraint origin_span to per-argument spans after unification. Collecting
                // before the loop avoids borrow-checker conflicts with the state borrows inside.
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

                    // Boundary guard tracking: if argument is Unknown and parameter expects
                    // a concrete type, record this as a gradual typing boundary.
                    if matches!(arg_ty, Type::Unknown) && is_concrete_type(param_ty) {
                        // Record the argument span and expected type for gradual typing
                        // boundary guard insertion at eval time. HashMap ensures O(1)
                        // lookup per span in eval_core_expr.
                        if idx < args.len() {
                            state
                                .boundary_guards
                                .insert(args[idx].span.clone(), param_ty.clone());
                        }
                    }

                    // Error-typed args absorb silently (unify(Error, T) = Ok(())),
                    // so we only propagate unification errors from non-Error args.
                    if let Err(e) = unify(param_ty, arg_ty, &mut subst, state, span.clone()) {
                        arg_errors.get_or_insert_with(Vec::new).push(e);
                    }
                }
                // T013 Task 4: Update constraint origin_span to per-argument span.
                // instantiate_scheme set origin_span to func_span for all constraints. Here
                // we refine that to the individual argument span: for each constraint whose
                // vars appear in param[i]'s type, set origin_span to args[i].span.
                // First-argument-wins for type vars shared across multiple params.
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
                    for c in state.constraints[constraints_start..].iter_mut() {
                        if let crate::type_class::Constraint::Class {
                            vars, origin_span, ..
                        } = c
                        {
                            // Find the arg span for this constraint's vars. first-match wins
                            // (preserves the lowest argument index for shared type vars).
                            let best_span = vars.iter().find_map(|v| var_to_arg_span.get(v));
                            if let Some(new_span) = best_span {
                                *origin_span = Some(new_span.clone());
                            }
                        }
                    }
                }
                // Check variadic args: if the function is variadic, unify all arg_types starting at
                // non_variadic_param_count against the Seq element type. Widen literals first.
                if *variadic && arg_types.len() > non_variadic_param_count {
                    // The last param is the variadic param — extract its Seq element type
                    if let Some((_, Type::Seq(elem_ty))) = params.last() {
                        for arg_ty in arg_types.iter().skip(non_variadic_param_count) {
                            // Widen literal types before unifying
                            let widened_ty = match arg_ty {
                                Type::IntLiteral(_) => Type::Int,
                                Type::StringLiteral(_) => Type::Str,
                                other => other.clone(),
                            };
                            if let Err(e) =
                                unify(elem_ty, &widened_ty, &mut subst, state, span.clone())
                            {
                                arg_errors.get_or_insert_with(Vec::new).push(e);
                            }
                        }
                    }
                }
                // Check for duplicate named argument names
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
                // Mirrors check_call CALL-POLY named-arg loop (same pattern, same error messages).
                // `params` here are already the instantiated params from instantiate_scheme above.
                for na in named_args {
                    let arg_name = &na.node.name;
                    // Find the param with matching name, tracking its index to detect overlap
                    let param_match = params.iter().enumerate().find_map(|(idx, (pname, pty))| {
                        if pname.as_ref() == Some(arg_name) {
                            Some((idx, pty))
                        } else {
                            None
                        }
                    });

                    match param_match {
                        Some((param_idx, param_ty)) => {
                            // C-NO-OVERLAP check: if this param was already consumed by a positional arg,
                            // emit a type error and skip unification (the positional check already ran).
                            if consumed_params.contains(&param_idx) {
                                arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                                    format!(
                                        "named argument '{}' conflicts with positional argument at position {}",
                                        arg_name, param_idx
                                    ),
                                    na.span.clone(),
                                ));
                                continue;
                            }
                            // Mark param as consumed (Task 1: Robinson idempotency)
                            consumed_params.insert(param_idx);
                            // Infer named arg type and unify
                            match infer_surface_expr(&na.node.value, env, state, type_map) {
                                Ok(arg_ty) => {
                                    // Task 2: merge state.subst updates from infer_surface_expr into local subst
                                    subst
                                        .type_map
                                        .borrow_mut()
                                        .extend(state.subst.type_map.borrow().clone());
                                    if let Err(e) =
                                        unify(&arg_ty, param_ty, &mut subst, state, na.span.clone())
                                    {
                                        arg_errors.get_or_insert_with(Vec::new).push(
                                            TypeError::new(
                                                format!(
                                                    "named argument '{}' type mismatch: {}",
                                                    arg_name, e.message
                                                ),
                                                na.span.clone(),
                                            ),
                                        );
                                    }
                                }
                                Err(mut errs) => {
                                    arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                                }
                            }
                        }
                        None => {
                            arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                                format!(
                                    "unknown named argument: function has no parameter named '{}'",
                                    arg_name
                                ),
                                na.span.clone(),
                            ));
                        }
                    }
                }
                if let Some(errors) = arg_errors {
                    return Err(errors);
                }
                // Merge local subst back into state.subst so that constraints from this
                // polymorphic call site are visible to subsequent inference steps. Without
                // this merge, bindings accumulated during argument unification (e.g., a
                // TypeVar constrained to Int) are lost for downstream entries in the same
                // letrec group. This mirrors infer_dict Pass 3d (lines 764-773).
                for (k, v) in subst.type_map.borrow().iter() {
                    state
                        .subst
                        .type_map
                        .borrow_mut()
                        .insert(k.clone(), v.clone());
                }
                state.subst.check_size(span).map_err(|e| vec![e])?;
                // After merging local subst into state.subst, state.subst is a superset of subst.
                // Applying state.subst directly is sufficient — a prior double-application
                // (subst.apply then state.subst.apply) was redundant because state.subst already
                // contains everything subst mapped.
                Ok(state.subst.apply(ret))
            } else {
                // Zero-param: no arguments to unify, return type needs no substitution applied
                // from local argument unification (there are no arguments). Apply state.subst
                // for access-chain constraints that may bind type vars in the return type.
                if state.subst.is_empty() {
                    Ok((**ret).clone())
                } else {
                    Ok(state.subst.apply(ret))
                }
            }
        }
        // Gradual: callee type is Unknown — infer args for LSP hover, return Unknown
        Type::Unknown => {
            // Infer positional args for type map population (needed for LSP hover on Unknown-typed functions).
            // This loop runs only for Unknown-typed callees — for Type::Function arms, positional args are
            // already inferred exactly once in CALL-POLY (infer_surface_expr at line 934).
            // Running it here unconditionally would cause double-inference for Function calls, mutating
            // state.name_counter and state.subst a second time in violation of single-pass Algorithm W.
            // Cascade prevention: ignore arg errors (already recorded as Error in type_map).
            for arg in args {
                let _ = infer_surface_expr(arg, env, state, type_map);
            }
            // Infer named arg values exactly once here for type map population and error propagation.
            // Previously these were inferred in a pre-dispatch loop before `match &func_ty`, which
            // caused double-inference when the function type was resolved (CALL-POLY arm infers named
            // args again). Moving inference to each arm ensures single-pass Algorithm W.
            let mut named_arg_errors: Vec<TypeError> = Vec::new();
            for na in named_args {
                if let Err(mut errs) = infer_surface_expr(&na.node.value, env, state, type_map) {
                    named_arg_errors.append(&mut errs);
                }
            }
            if !named_arg_errors.is_empty() {
                return Err(named_arg_errors);
            }
            Ok(Type::Unknown)
        }
        // Unit variant constructor call: [Ok 42] wraps 42 as Ok's payload.
        // Runtime behavior (eval_call.rs:136-196): variant constructors with __variant_tag__
        // marker accept exactly 1 positional arg and wrap it as Value::Variant payload.
        // Type checking mirrors this: NominalVariant with empty fields + 1 positional arg
        // → result type is NominalVariant with inferred arg type as single-field payload.
        Type::NominalVariant { tag, fields } if fields.fields.is_empty() => {
            // Only allow exactly 1 positional arg, no named args (matches runtime validation)
            if args.len() != 1 {
                return Err(vec![TypeError::new(
                    format!(
                        "unit variant constructor takes exactly 1 argument, got {}",
                        args.len()
                    ),
                    span,
                )]);
            }
            if !named_args.is_empty() {
                return Err(vec![TypeError::new(
                    "unit variant constructor does not accept named arguments".to_string(),
                    span,
                )]);
            }

            // Infer the argument type
            let arg_ty = infer_surface_expr(&args[0], env, state, type_map)?;

            // Result type: NominalVariant with the arg type as payload.
            // Runtime stores payload as Some(payload_id), so we model it as a single-field
            // record with numeric field "0" (consistent with single-positional payload convention).
            let mut payload_fields = HashMap::new();
            payload_fields.insert("0".to_string(), arg_ty);
            Ok(Type::NominalVariant {
                tag: tag.clone(),
                fields: Row {
                    fields: payload_fields,
                },
            })
        }
        // Non-unit variant constructor: already has payload, cannot be called.
        // Example: [Ok 42] where Ok already constructed → type error.
        Type::NominalVariant { .. } => Err(vec![TypeError::not_a_function(&func_ty, func_span)]),
        _ => Err(vec![TypeError::not_a_function(&func_ty, func_span)]),
    }
}

/// Check a function call expression.
///
/// Inline lambdas with type annotations (e.g., `[call [fn [x@a] $x] 42]`) go through
/// this function, not `check_call_with_scheme`, because the callee is a `Fn` expression
/// (not a `VarRef` to a polymorphic scheme). `infer_expr` on the `Fn` synthesizes a type
/// with fresh TypeVars from annotations, which then enters the CALL-POLY path for
/// instantiation. This is a double-instantiation (annotation TypeVars + CALL-POLY TypeVars)
/// but is harmless for single-call sites: the extra freshening produces equivalent
/// constraints. The `check_call_with_scheme` optimization (instantiate once) only applies
/// to `VarRef` callees where the scheme is looked up from the environment.
///
/// Named argument type checking fires in three paths:
/// - CALL-MONO (here): for each named arg, finds the matching parameter by name in `params`
///   and unifies the arg type against the parameter type via `infer_expr` + `unify`; emits
///   `TypeError` on name mismatch ("unknown named argument") or type mismatch.
/// - CALL-POLY (here): same name-based lookup and unify on the instantiated params.
/// - `check_call_with_scheme` Function arm: same name-based lookup and unify after positional
///   arg unification; uses `params` from the already-instantiated `func_ty`.
///
///   Note: named-arg checking fires only for resolved function types; same-dict letrec forward
///   references fall through to the `TypeVar` arm and skip named-arg validation.
pub(crate) fn check_call(
    func: &Arc<SurfaceNode>,
    args: &[Arc<SurfaceNode>],
    named_args: &[Spanned<SurfaceNamedArg>],
    env: &Rc<TypeEnv>,
    span: Span,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let func_ty = infer_surface_expr(func, env, state, type_map)?;
    // Apply state.subst to resolve any TypeVars bound during infer_expr (e.g., from infer_fn
    // with polymorphic return annotations). Without this, has_inference_vars() incorrectly returns
    // true for already-bound TypeVars, causing CALL-POLY to fire and double-instantiate.
    let func_ty = if state.subst.is_empty() {
        func_ty
    } else {
        state.subst.apply(&func_ty)
    };

    // Error cascade suppression: if the function type is Error (e.g., `include` failed prelude
    // type-checking and was recorded as Type::Error in TypeEnv), infer arguments for side effects
    // and return Unknown rather than propagating "expected function type, got <error>" (T003).
    // This prevents spurious T003 errors on every [include %libdir "..."] call when the prelude's
    // self-type-check encounters errors. The underlying cause (prelude type error) has already
    // been reported; return Error to cascade the failure rather than going gradual.
    if matches!(func_ty, Type::Error) {
        // Infer positional args for type map population and error propagation.
        for arg in args {
            let _ = infer_surface_expr(arg, env, state, type_map);
        }
        // Infer named args for type map population and error propagation.
        for na in named_args {
            let _ = infer_surface_expr(&na.node.value, env, state, type_map);
        }
        return Ok(Type::Error);
    }

    match &func_ty {
        Type::Function {
            params,
            ret,
            variadic,
        } => {
            // Arity check: named args fill remaining parameter slots by name (Kotlin model in
            // eval_call.rs), so count positional + named against total params.
            let total_supplied = args.len() + named_args.len();
            // For variadic functions, the last param accepts arbitrary extra args.
            // Require at least (params.len() - 1) args for variadic functions.
            let min_required = if *variadic && !params.is_empty() {
                params.len() - 1
            } else {
                params.len()
            };
            if total_supplied < min_required || (!*variadic && total_supplied != params.len()) {
                return Err(vec![TypeError::new(
                    format!(
                        "arity mismatch: expected {}{} argument(s), got {} ({} positional, {} named)",
                        if *variadic { "at least " } else { "" },
                        min_required,
                        total_supplied,
                        args.len(),
                        named_args.len(),
                    ),
                    span,
                )]);
            }

            // CALL-MONO: function type is fully concrete (no type variables)
            // Use bidirectional checking for arguments via [SUB] rule (doc/06 §[CALL-MONO])
            //
            // ASYMMETRY: CALL-MONO collects all argument errors before returning (errors Vec
            // accumulates then is returned at once), while CALL-POLY (below) stops at the first
            // unification failure (map_err returns immediately). CALL-MONO's multi-error approach
            // is preferred for user-facing type errors; CALL-POLY's early-exit is a limitation of
            // sequential unification where later argument types may be meaningless if earlier
            // unification fails (type variables left unbound). A future improvement would
            // collect CALL-POLY errors too, but requires constraint-based solving (see comment below).
            if !func_ty.has_inference_vars() {
                let mut errors = Vec::new();
                // Track consumed param indices to prevent named args from overlapping with positional args.
                // C-NO-OVERLAP: positional args consume params 0..args.len(). Named args searching
                // ALL params by name could accidentally match a positional-consumed param if the call
                // supplies both positional and named args (e.g., [call $f 42 x: 99] where param 0 is
                // named x would check param 0 twice).
                let mut consumed_params = std::collections::HashSet::new();
                // Check positional args against non-variadic params.
                // If the function is variadic, stop before the last param (which is the variadic param itself).
                let non_variadic_param_count = if *variadic && !params.is_empty() {
                    params.len() - 1
                } else {
                    params.len()
                };
                for (idx, (arg, (_param_name, param_ty))) in args
                    .iter()
                    .zip(params.iter().take(non_variadic_param_count))
                    .enumerate()
                {
                    consumed_params.insert(idx);

                    // Boundary guard tracking and bidirectional checking:
                    // - For lambda args: use check_expr (lambda checking mode, no inference needed)
                    // - For non-lambda args: infer, check for Unknown, then subsume (avoids double-inference)
                    match &arg.expr {
                        SurfaceExpression::Fn { .. } => {
                            // Lambda: use check_surface_expr for bidirectional lambda checking mode.
                            // Lambdas can't be Unknown, so no boundary guard needed.
                            // param_ty is ground under the CALL-MONO invariant (!func_ty.has_inference_vars()),
                            // so no explicit state.subst.apply() is needed here — check_surface_expr applies
                            // state.subst to its expected type internally, which is a no-op on ground types.
                            if let Err(mut errs) =
                                check_surface_expr(arg, param_ty, env, state, type_map)
                            {
                                errors.append(&mut errs);
                            }
                        }
                        _ => {
                            // Non-lambda: infer once, check Unknown, then subsume (no double-inference).
                            match infer_surface_expr(arg, env, state, type_map) {
                                Ok(arg_ty) => {
                                    // Apply substitution before Unknown check and subsumption.
                                    let arg_ty_resolved = if state.subst.is_empty() {
                                        arg_ty
                                    } else {
                                        state.subst.apply(&arg_ty)
                                    };
                                    // Boundary guard: Unknown→concrete boundary needs runtime guard.
                                    if is_concrete_type(param_ty)
                                        && matches!(arg_ty_resolved, Type::Unknown)
                                    {
                                        state
                                            .boundary_guards
                                            .insert(arg.span.clone(), param_ty.clone());
                                    }
                                    // CALL-MONO guarantees func_ty has no inference vars, so
                                    // param_ty (drawn from func_ty.params) is always ground.
                                    // Applying state.subst to a ground type is a no-op, but we
                                    // do it for consistency with the arg side above.
                                    // Unification is never needed here — use subsumption directly.
                                    let param_ty_resolved = if state.subst.is_empty() {
                                        param_ty.clone()
                                    } else {
                                        state.subst.apply(param_ty)
                                    };
                                    // Subsumption: arg_ty <: param_ty OR consistency if Unknown/Top present.
                                    let sub_passes =
                                        Type::is_subtype(&arg_ty_resolved, &param_ty_resolved)
                                            || ((contains_unknown_or_top(&arg_ty_resolved)
                                                || contains_unknown_or_top(&param_ty_resolved))
                                                && Type::is_consistent(
                                                    &arg_ty_resolved,
                                                    &param_ty_resolved,
                                                ));
                                    if !sub_passes {
                                        errors.push(TypeError::type_mismatch(
                                            &param_ty_resolved,
                                            &arg_ty_resolved,
                                            arg.span.clone(),
                                        ));
                                    }
                                }
                                Err(mut errs) => {
                                    errors.append(&mut errs);
                                }
                            }
                        }
                    }
                }
                // Check variadic args: if the function is variadic, infer all args starting at
                // non_variadic_param_count and unify them against the Seq element type.
                // Use infer+unify instead of check_expr to allow literal widening (IntLiteral → Int).
                if *variadic && args.len() > non_variadic_param_count {
                    // The last param is the variadic param — extract its Seq element type
                    if let Some((_, Type::Seq(elem_ty))) = params.last() {
                        for arg in args.iter().skip(non_variadic_param_count) {
                            match infer_surface_expr(arg, env, state, type_map) {
                                Ok(arg_ty) => {
                                    // Widen literal types before unifying to allow [f 10 20 30]
                                    // where 10, 20, 30 all unify with Int element type.
                                    let widened_ty = match arg_ty {
                                        Type::IntLiteral(_) => Type::Int,
                                        Type::StringLiteral(_) => Type::Str,
                                        other => other,
                                    };
                                    let mut subst = std::mem::take(&mut state.subst);
                                    if let Err(e) = unify(
                                        &widened_ty,
                                        elem_ty,
                                        &mut subst,
                                        state,
                                        arg.span.clone(),
                                    ) {
                                        errors.push(e);
                                    }
                                    state.subst = subst;
                                }
                                Err(mut errs) => {
                                    errors.append(&mut errs);
                                }
                            }
                        }
                    }
                }
                // Check for duplicate named argument names
                let mut seen_names: HashSet<&str> = HashSet::new();
                for na in named_args {
                    if !seen_names.insert(&na.node.name) {
                        errors.push(TypeError::new(
                            format!("duplicate named argument: '{}'", na.node.name),
                            na.span.clone(),
                        ));
                    }
                }
                // Check named args by matching them to params by name
                for na in named_args {
                    let arg_name = &na.node.name;
                    // Find the param with matching name, tracking its index to detect overlap
                    let param_match = params.iter().enumerate().find_map(|(idx, (pname, pty))| {
                        if pname.as_ref() == Some(arg_name) {
                            Some((idx, pty))
                        } else {
                            None
                        }
                    });

                    match param_match {
                        Some((param_idx, param_ty)) => {
                            // C-NO-OVERLAP check: if this param was already consumed by a positional arg,
                            // emit a type warning and skip type checking (the positional check already ran).
                            if consumed_params.contains(&param_idx) {
                                errors.push(TypeError::new(
                                    format!(
                                        "named argument '{}' conflicts with positional argument at position {}",
                                        arg_name, param_idx
                                    ),
                                    na.span.clone(),
                                ));
                                continue;
                            }
                            // Mark param as consumed (Task 1: Robinson idempotency)
                            consumed_params.insert(param_idx);

                            // Infer the named arg type and unify against the param type.
                            // Boundary guard tracking: after inferring the arg type, if it is
                            // Unknown and the parameter expects a concrete type, record the span
                            // for gradual typing boundary guard insertion. This avoids a redundant
                            // pre-call infer_surface_expr that would mutate state before the actual check.
                            match infer_surface_expr(&na.node.value, env, state, type_map) {
                                Ok(arg_ty) => {
                                    // Boundary guard check (post-inference, single-pass)
                                    if is_concrete_type(param_ty) {
                                        let resolved_arg_ty = if state.subst.is_empty() {
                                            arg_ty.clone()
                                        } else {
                                            state.subst.apply(&arg_ty)
                                        };
                                        if matches!(resolved_arg_ty, Type::Unknown) {
                                            state.boundary_guards.insert(
                                                na.node.value.span.clone(),
                                                param_ty.clone(),
                                            );
                                        }
                                    }
                                    let mut subst = std::mem::take(&mut state.subst);
                                    let result = unify(
                                        &arg_ty,
                                        param_ty,
                                        &mut subst,
                                        state,
                                        na.span.clone(),
                                    );
                                    state.subst = subst;
                                    if let Err(e) = result {
                                        errors.push(TypeError::new(
                                            format!(
                                                "named argument '{}' type mismatch: {}",
                                                arg_name, e.message
                                            ),
                                            na.span.clone(),
                                        ));
                                    }
                                }
                                Err(mut errs) => {
                                    errors.append(&mut errs);
                                }
                            }
                        }
                        None => {
                            errors.push(TypeError::new(
                                format!(
                                    "unknown named argument: function has no parameter named '{}'",
                                    arg_name
                                ),
                                na.span.clone(),
                            ));
                        }
                    }
                }
                if !errors.is_empty() {
                    return Err(errors);
                }
                // Apply state.subst for defensive consistency with check_call_with_scheme.
                // The CALL-MONO guard (!func_ty.has_inference_vars()) means ret is typically fully
                // concrete, making apply() a no-op. But applying defensively guards against
                // edge cases where has_inference_vars() and the substitution domain diverge.
                return Ok(state.subst.apply(ret));
            }

            // CALL-POLY: function type has type variables
            // Instantiate the function type, then check arguments (doc/06 §[CALL-POLY])
            // Unified with CALL-MONO: both paths use check_expr, which internally dispatches
            // to unification (for TypeVars) or subsumption (for concrete types).
            let inst_ty = instantiate_at_level(&func_ty, state);

            let (inst_params, inst_ret) = match &inst_ty {
                Type::Function {
                    params,
                    ret,
                    variadic: _,
                } => (params, ret),
                _ => unreachable!("instantiate_at_level preserves Function variant"),
            };

            // Check arguments against instantiated parameter types.
            // check_expr will use unification because inst_params contain fresh TypeVars.
            let mut arg_errors: Option<Vec<TypeError>> = None;

            if !params.is_empty() {
                // Track consumed param indices to prevent named args from overlapping with positional args.
                // C-NO-OVERLAP: positional args consume params 0..args.len(). Named args searching
                // ALL params by name could accidentally match a positional-consumed param.
                let mut consumed_params = std::collections::HashSet::new();
                // Check positional args via check_expr (unified CALL-MONO/CALL-POLY path).
                // check_expr will use unification internally because inst_params contain TypeVars.
                // If the function is variadic, stop before the last param (which is the variadic param itself).
                let non_variadic_param_count = if *variadic && !inst_params.is_empty() {
                    inst_params.len() - 1
                } else {
                    inst_params.len()
                };
                for (idx, (arg, (_param_name, param_ty))) in args
                    .iter()
                    .zip(inst_params.iter().take(non_variadic_param_count))
                    .enumerate()
                {
                    consumed_params.insert(idx);
                    if let Err(mut errs) = check_surface_expr(arg, param_ty, env, state, type_map) {
                        arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                    }
                }
                // Check variadic args: if the function is variadic, infer all args starting at
                // non_variadic_param_count and unify them against the Seq element type.
                // Use infer+unify instead of check_expr to allow literal widening (IntLiteral → Int).
                if *variadic && args.len() > non_variadic_param_count {
                    // The last param is the variadic param — extract its Seq element type
                    if let Some((_, Type::Seq(elem_ty))) = inst_params.last() {
                        for arg in args.iter().skip(non_variadic_param_count) {
                            match infer_surface_expr(arg, env, state, type_map) {
                                Ok(arg_ty) => {
                                    // Widen literal types before unifying
                                    let widened_ty = match arg_ty {
                                        Type::IntLiteral(_) => Type::Int,
                                        Type::StringLiteral(_) => Type::Str,
                                        other => other,
                                    };
                                    let mut subst = std::mem::take(&mut state.subst);
                                    if let Err(e) = unify(
                                        &widened_ty,
                                        elem_ty,
                                        &mut subst,
                                        state,
                                        arg.span.clone(),
                                    ) {
                                        arg_errors.get_or_insert_with(Vec::new).push(e);
                                    }
                                    state.subst = subst;
                                }
                                Err(mut errs) => {
                                    arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                                }
                            }
                        }
                    }
                }
                // Check for duplicate named argument names
                let mut seen_names: HashSet<&str> = HashSet::new();
                for na in named_args {
                    if !seen_names.insert(&na.node.name) {
                        arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                            format!("duplicate named argument: '{}'", na.node.name),
                            na.span.clone(),
                        ));
                    }
                }
                // Check named args by matching them to params by name
                for na in named_args {
                    let arg_name = &na.node.name;
                    // Find the param with matching name, tracking its index to detect overlap
                    let param_match =
                        inst_params
                            .iter()
                            .enumerate()
                            .find_map(|(idx, (pname, pty))| {
                                if pname.as_ref() == Some(arg_name) {
                                    Some((idx, pty))
                                } else {
                                    None
                                }
                            });

                    match param_match {
                        Some((param_idx, param_ty)) => {
                            // C-NO-OVERLAP check: if this param was already consumed by a positional arg,
                            // emit a type error and skip checking (the positional check already ran).
                            if consumed_params.contains(&param_idx) {
                                arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                                    format!(
                                        "named argument '{}' conflicts with positional argument at position {}",
                                        arg_name, param_idx
                                    ),
                                    na.span.clone(),
                                ));
                                continue;
                            }
                            // Mark param as consumed
                            consumed_params.insert(param_idx);

                            // Check named arg: infer arg type once, then record boundary guard
                            // if arg is Unknown and param expects a concrete type, then unify.
                            // This avoids a redundant pre-call infer_surface_expr that would mutate
                            // state before the actual bidirectional check (the prior pattern of
                            // calling infer_surface_expr twice — once for guard, once via check_expr —
                            // left stale type vars from the first call affecting the second).
                            match infer_surface_expr(&na.node.value, env, state, type_map) {
                                Ok(arg_ty) => {
                                    // Boundary guard check (post-inference, single-pass)
                                    if is_concrete_type(param_ty) {
                                        let resolved_arg_ty = if state.subst.is_empty() {
                                            arg_ty.clone()
                                        } else {
                                            state.subst.apply(&arg_ty)
                                        };
                                        if matches!(resolved_arg_ty, Type::Unknown) {
                                            state.boundary_guards.insert(
                                                na.node.value.span.clone(),
                                                param_ty.clone(),
                                            );
                                        }
                                    }
                                    // Unify the inferred type against the expected param type
                                    let mut subst = std::mem::take(&mut state.subst);
                                    let result = unify(
                                        &arg_ty,
                                        param_ty,
                                        &mut subst,
                                        state,
                                        na.span.clone(),
                                    );
                                    state.subst = subst;
                                    if let Err(errs) = result {
                                        arg_errors.get_or_insert_with(Vec::new).push(
                                            TypeError::new(
                                                format!(
                                                    "named argument '{}' type mismatch: {}",
                                                    arg_name, errs.message
                                                ),
                                                na.span.clone(),
                                            ),
                                        );
                                    }
                                }
                                Err(mut errs) => {
                                    arg_errors.get_or_insert_with(Vec::new).append(&mut errs);
                                }
                            }
                        }
                        None => {
                            arg_errors.get_or_insert_with(Vec::new).push(TypeError::new(
                                format!(
                                    "unknown named argument: function has no parameter named '{}'",
                                    arg_name
                                ),
                                na.span.clone(),
                            ));
                        }
                    }
                }
                if let Some(errors) = arg_errors {
                    return Err(errors);
                }
                // After checking all arguments via check_expr, state.subst has been updated
                // with all unifications. Apply it to the return type to get the final result.
                Ok(state.subst.apply(inst_ret))
            } else {
                // Zero-param polymorphic function: return the instantiated return type
                // (not the original `ret` which contains the scheme-internal variable names)
                if state.subst.is_empty() {
                    Ok((**inst_ret).clone())
                } else {
                    Ok(state.subst.apply(inst_ret))
                }
            }
        }
        Type::TypeVar(_, _) => {
            // Unbound type variable (e.g. letrec forward reference to a function not yet
            // inferred). state.subst.apply already resolved bound TypeVars (line 1140-1144),
            // so reaching here means alpha is genuinely unbound. Conservative fallback:
            // infer args for side effects and return Unknown.
            // Cascade prevention: ignore arg errors (already recorded as Error in type_map).
            for arg in args {
                let _ = infer_surface_expr(arg, env, state, type_map);
            }
            // Infer named arg values exactly once here for type map population and error propagation.
            // Previously these were inferred in a pre-dispatch loop before `match &func_ty`, which
            // caused double-inference when the function type was resolved (CALL-MONO/CALL-POLY arms
            // infer named args again). Moving inference to each arm ensures single-pass Algorithm W.
            let mut named_arg_errors: Vec<TypeError> = Vec::new();
            for na in named_args {
                if let Err(mut errs) = infer_surface_expr(&na.node.value, env, state, type_map) {
                    named_arg_errors.append(&mut errs);
                }
            }
            if !named_arg_errors.is_empty() {
                return Err(named_arg_errors);
            }
            // TypeVar callee — create a fresh TypeVar for the return type to preserve inference.
            // This allows `[f x]` to unify later when `f`'s type becomes known, rather than
            // immediately going gradual with Unknown.
            let fresh_name = format!("_t{}", state.name_counter);
            state.name_counter = state.name_counter.saturating_add(1);
            state.levels.insert(fresh_name.clone(), state.level);
            let ret_var = Type::TypeVar(fresh_name, state.level);
            Ok(ret_var)
        }
        // Gradual: callee type is Unknown — infer args for LSP hover, return Unknown (check_call path)
        Type::Unknown => {
            // Infer positional args for type map population (needed for LSP hover on Unknown-typed functions).
            // This loop runs only for Unknown-typed callees — for Type::Function arms, positional args are
            // already inferred exactly once in CALL-MONO (check_expr at line 1011) or CALL-POLY (infer_surface_expr).
            // Running it here unconditionally would cause double-inference for Function calls, mutating
            // state.name_counter and state.subst a second time in violation of single-pass Algorithm W.
            // Cascade prevention: ignore arg errors (already recorded as Error in type_map).
            for arg in args {
                let _ = infer_surface_expr(arg, env, state, type_map);
            }
            // Infer named arg values exactly once here for type map population and error propagation.
            let mut named_arg_errors: Vec<TypeError> = Vec::new();
            for na in named_args {
                if let Err(mut errs) = infer_surface_expr(&na.node.value, env, state, type_map) {
                    named_arg_errors.append(&mut errs);
                }
            }
            if !named_arg_errors.is_empty() {
                return Err(named_arg_errors);
            }
            Ok(Type::Unknown)
        }
        // Unit variant constructor call: [Ok 42] wraps 42 as Ok's payload.
        // Runtime behavior (eval_call.rs:136-196): variant constructors with __variant_tag__
        // marker accept exactly 1 positional arg and wrap it as Value::Variant payload.
        // Type checking mirrors this: NominalVariant with empty fields + 1 positional arg
        // → result type is NominalVariant with inferred arg type as single-field payload.
        Type::NominalVariant { tag, fields } if fields.fields.is_empty() => {
            // Only allow exactly 1 positional arg, no named args (matches runtime validation)
            if args.len() != 1 {
                return Err(vec![TypeError::new(
                    format!(
                        "unit variant constructor takes exactly 1 argument, got {}",
                        args.len()
                    ),
                    span,
                )]);
            }
            if !named_args.is_empty() {
                return Err(vec![TypeError::new(
                    "unit variant constructor does not accept named arguments".to_string(),
                    span,
                )]);
            }

            // Infer the argument type
            let arg_ty = infer_surface_expr(&args[0], env, state, type_map)?;

            // Result type: NominalVariant with the arg type as payload.
            // Runtime stores payload as Some(payload_id), so we model it as a single-field
            // record with numeric field "0" (consistent with single-positional payload convention).
            let mut payload_fields = HashMap::new();
            payload_fields.insert("0".to_string(), arg_ty);
            Ok(Type::NominalVariant {
                tag: tag.clone(),
                fields: Row {
                    fields: payload_fields,
                },
            })
        }
        // Non-unit variant constructor: already has payload, cannot be called.
        // Example: [Ok 42] where Ok already constructed → type error.
        // Use func.span (points to the callee) rather than the whole-call span for a more informative error.
        Type::NominalVariant { .. } => {
            Err(vec![TypeError::not_a_function(&func_ty, func.span.clone())])
        }
        _ => {
            // T003: func_ty is a concrete non-callable type (e.g., Str, Int, Bool).
            //
            // B-275 FALSE POSITIVE: This arm fires incorrectly when a dict key has the same
            // name as an outer-scope prelude function (e.g., key `trim: "hello"` in a dict
            // where a sibling entry also calls `trim` as a function).
            //
            // Confirmed root cause: infer_dict processes SCCs in topological order. After
            // SCC1 ({trim: "hello"}) completes, dict_env is updated with
            // `trim → TypeScheme::mono(StringLiteral("hello"))` (concrete Str). This shadows
            // the prelude `trim: Fn[Str→Str]` for all subsequent SCCs. When SCC2 ({f: ...})
            // processes f's body, env.get("trim") finds the dict_env Str binding before the
            // prelude, so func_ty resolves to StringLiteral("hello") → T003.
            //
            // Secondary mechanism (same-SCC case): Pass 1_i in typecheck_dict.rs pre-binds
            // ALL SCC keys to TypeVar placeholders. If `trim` and `f` land in the same SCC,
            // state.subst.apply() (line ~651-655) resolves the TypeVar to Str before this
            // match, bypassing the TypeVar arm (line 1140) that would suppress the error.
            //
            // Fix direction (tracked in B-275): in check_call's VarRef dispatch (line ~1436),
            // after env.get(name) returns a monomorphic scheme with a non-function body, check
            // the env's parent chain for a function-typed binding under the same name. If
            // found AND the current binding came from a same-dict level (detectable via
            // TypeScheme level metadata or a "letrec_placeholder" flag), use the parent
            // binding instead. See typecheck_tests.rs::test_b275_letrec_typevar_does_not_shadow_prelude_function.
            Err(vec![TypeError::not_a_function(&func_ty, func.span.clone())])
        }
    }
}
