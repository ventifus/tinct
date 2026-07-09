//! Call and dot-access type checking.
#![allow(dead_code)]
//!
//! Contains:
//! - `check_dot_access` — field access type inference (string and integer keys)
//! - `check_dot_access_int` — integer dot access (`$data.0`)
//! - `is_concrete_type` — boundary guard predicate for gradual typing
//! - `check_call_with_scheme` — polymorphic call with single-shot scheme instantiation
//! - `check_call` — general function call type checking (CALL-MONO and CALL-POLY)

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use super::{check_surface_expr, contains_unknown_or_top, infer_surface_expr, TypeMap};
use crate::ast::{DotKey, Span, Spanned, SurfaceExpression, SurfaceNamedArg, SurfaceNode};
use crate::env::Env;
use crate::rust_span;
use crate::type_errors::{ArityMismatch, TypeErrorTyped, UnificationFailure};
use crate::types::{
    instantiate_at_level, instantiate_scheme, unify, Constraint, InferState, Kind, Row, Type,
    TypeError, TypeScheme,
};

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
        Type::Record(row) => {
            let widened_fields = row
                .fields
                .into_iter()
                .map(|(k, v)| (k, widen_literal_types(v)))
                .collect();
            Type::Record(Row {
                fields: widened_fields,
                tail: row.tail,
            })
        }
        other => other,
    }
}

pub(crate) async fn check_dot_access(
    target: &Arc<SurfaceNode>,
    field: &DotKey,
    env: &Arc<RwLock<Env>>,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Convert DotKey to string for field lookup
    let field_str = match field {
        DotKey::Ident(s) => s.as_str(),
        DotKey::Int(n) => {
            return check_dot_access_int(target, *n, env, span, state, constraints, type_map).await
        }
    };

    // [DOT-POLY] fast-path: if target is a VarRef and its scheme has inner_schemes,
    // instantiate the field's scheme polymorphically
    if let SurfaceExpression::VarRef { name, .. } = &target.expr {
        if let Some(scheme) = env.read().unwrap().get_scheme(name) {
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
                        &span,
                    );
                    return Ok(instantiated);
                }
            }
        }
    }

    let target_ty = infer_surface_expr(target, env, state, type_map).await?;
    // Apply the global accumulated substitution so that constraints from prior accesses
    // on the same target are visible (doc/07-type-extensions.md Part 5).
    let target_ty = state.apply(&target_ty);
    // TyCon expansion (T-1272): if the target has a named type constructor type, expand it
    // to the constructor's body for field lookup. This allows dot-access on TyCon-typed values
    // (e.g., `expr.return-ann` where `expr: Expression`, or `ann.text` where `ann: Annotation`).
    //
    // One-level expansion only. Builtin TyCons have non-Record bodies (Int→Type::Int,
    // Map→App(...), Fn→Function{...}), so they fall to the `_` arm and correctly error
    // (builtin TyCon values don't have named record fields accessible via dot-access).
    // User-defined ADTs have body = Union of NominalVariants or Record, which is then
    // handled by the Union/Record arms below for field lookup across members.
    //
    // Gradual: unknown TyCons (not in tycon_env) are kept as-is; they fall to `_` → NotARecord.
    let target_ty = {
        // TyCon expansion: look up body before consuming target_ty.
        let expanded = if let Type::TyCon(name) = &target_ty {
            let _tycon = state.tycon_env_ref();
            _tycon.get(name.as_str()).map(|def| def.body.clone())
        } else {
            None
        };
        expanded.unwrap_or(target_ty)
    };
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
            // Create fresh β for the field type — named after the field for diagnostics
            let beta = state
                .fresh_type_var_with(Some(field_str), None, Kind::Type, &span)
                .1;

            // Build the record type to unify α with (BAS: no RowVar tail)
            let mut fields = indexmap::IndexMap::new();
            fields.insert(field_str.to_string(), beta.clone());
            let record_ty = Type::Record(Row {
                fields,
                tail: crate::type_def::RowTail::Empty,
            });

            // Unify TypeVar(α) with Record({field: β})
            let alpha_ty = Type::TypeVar(alpha.clone(), alpha_level);
            let result = Box::pin(unify(&alpha_ty, &record_ty, state, constraints, span)).await;
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
        // NominalVariant: look up the field in the variant's payload fields.
        // This handles dot-access on typed variant values (e.g., ann.text where ann: Simple).
        Type::NominalVariant { ref fields, .. } => match fields.fields.get(field_str) {
            Some(ty) => Ok(ty.clone()),
            None => Ok(Type::Unknown), // field not declared on this variant — gradual
        },
        // Union: collect field types from all members that declare the field.
        // Used for dot-access on NominalVariant union types (e.g., ann.text where
        // ann: Annotation = Simple | PropertyDict | Annotated, all having text: String).
        // Returns the union of all member field types; Unknown for members lacking the field.
        Type::Union(ref members) => {
            let mut field_types: Vec<Type> = Vec::new();
            let mut all_unknown = true;
            for member in members {
                let member_field = match member {
                    Type::Record(Row { ref fields, .. }) => {
                        fields.get(field_str).cloned().unwrap_or(Type::Unknown)
                    }
                    Type::NominalVariant { ref fields, .. } => fields
                        .fields
                        .get(field_str)
                        .cloned()
                        .unwrap_or(Type::Unknown),
                    Type::Unknown => Type::Unknown,
                    _ => Type::Unknown,
                };
                if !matches!(member_field, Type::Unknown) {
                    all_unknown = false;
                }
                field_types.push(member_field);
            }
            if all_unknown {
                Ok(Type::Unknown)
            } else {
                Ok(Type::normalize_union(field_types))
            }
        }
        _ => Err(vec![TypeError::new(
            format!("expected record type, got {}", target_ty),
            span,
        )]),
    }
}

/// Type check integer dot access: `$data.0`
pub(crate) async fn check_dot_access_int(
    target: &Arc<SurfaceNode>,
    index: i64,
    env: &Arc<RwLock<Env>>,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let target_ty = infer_surface_expr(target, env, state, type_map).await?;
    let target_ty = state.apply(&target_ty);

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
            // Create fresh β named after the numeric field index for diagnostics
            let beta = state
                .fresh_type_var_with(Some(field_name.as_str()), None, Kind::Type, &span)
                .1;

            let mut fields = indexmap::IndexMap::new();
            fields.insert(field_name, beta.clone());
            let record_ty = Type::Record(Row {
                fields,
                tail: crate::type_def::RowTail::Empty,
            });

            let alpha_ty = Type::TypeVar(alpha.clone(), *alpha_level);
            let result = Box::pin(unify(&alpha_ty, &record_ty, state, constraints, span)).await;
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
        _ => Err(vec![TypeError::new(
            format!("expected record type, got {}", target_ty),
            span,
        )]),
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
        Type::Record(row) => row.fields.values().all(is_concrete_type),
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

/// Check a call where the function is a TypeScheme (from a VarRef lookup).
/// This avoids double instantiation: instead of VAR-POLY instantiating the scheme
/// and then CALL-POLY instantiating the result, we instantiate once here.
#[allow(clippy::too_many_arguments)] // Signature matches check_call pattern
pub(crate) async fn check_call_with_scheme(
    scheme: &TypeScheme,
    func_span: Span,
    func_name: Option<&str>,
    args: &[Arc<SurfaceNode>],
    named_args: &[Spanned<SurfaceNamedArg>],
    env: &Arc<RwLock<Env>>,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
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
    let constraints_start = constraints.len();
    let func_ty = instantiate_scheme(
        scheme,
        state.level,
        state,
        func_name,
        Some(func_span.clone()),
        &func_span,
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
    // infer arguments for side effects and return the Error (with its payload) to cascade.
    // This prevents spurious "expected function type, got <error>" on call sites when the
    // function definition itself failed type-checking. The root cause has already been reported.
    // We return `func_ty` (not error_cascade()) so that any payload in the Error propagates
    // to containing call sites — that way `got_types` can show `<error: reason>` not bare `<error>`.
    if matches!(func_ty, Type::Error(_)) {
        // Infer positional args for type map population and error propagation.
        for arg in args {
            let _ = infer_surface_expr(arg, env, state, type_map).await;
        }
        // Infer named args for type map population and error propagation.
        for na in named_args {
            let _ = infer_surface_expr(&na.node.value, env, state, type_map).await;
        }
        return Err(vec![TypeError::not_a_function(&func_ty, func_span.clone())]);
    }

    match &func_ty {
        Type::Function {
            params,
            ret,
            variadic,
            required_count,
        } => {
            // CALL-POLY: After instantiation, the function type always has type variables.
            // This is guaranteed by the guard at line 236: check_call_with_scheme is only called
            // for polymorphic schemes (non-empty type_vars or row_vars), and instantiate_scheme
            // produces fresh TypeVars/RowVars for each quantified variable. Since generalize only
            // quantifies variables that appear in the body, the instantiated type must contain
            // those fresh variables, so has_inference_vars() is always true.
            debug_assert!(
                func_ty.has_inference_vars(),
                "check_call_with_scheme: func_ty must have inference vars after instantiation (invariant violated)"
            );
            Box::pin(check_call_args(
                params,
                ret,
                *variadic,
                *required_count,
                func_name,
                args,
                named_args,
                env,
                span,
                state,
                constraints,
                type_map,
                constraints_start,
                true, // is_poly
            ))
            .await
        }
        // Gradual: callee type is Unknown — infer args for LSP hover, return Unknown
        Type::Unknown => {
            // Infer positional args for type map population (needed for LSP hover on Unknown-typed functions).
            // This loop runs only for Unknown-typed callees — for Type::Function arms, positional args are
            // already inferred exactly once in CALL-POLY (infer_surface_expr at line 934).
            // Running it here unconditionally would cause double-inference for Function calls, mutating
            // state.subst.name_counter and state.subst a second time in violation of single-pass Algorithm W.
            // Cascade prevention: ignore arg errors (already recorded as Error in type_map).
            for arg in args {
                let _ = infer_surface_expr(arg, env, state, type_map).await;
            }
            // Infer named arg values exactly once here for type map population and error propagation.
            // Previously these were inferred in a pre-dispatch loop before `match &func_ty`, which
            // caused double-inference when the function type was resolved (CALL-POLY arm infers named
            // args again). Moving inference to each arm ensures single-pass Algorithm W.
            let mut named_arg_errors: Vec<TypeError> = Vec::new();
            for na in named_args {
                if let Err(mut errs) =
                    infer_surface_expr(&na.node.value, env, state, type_map).await
                {
                    named_arg_errors.append(&mut errs);
                }
            }
            if !named_arg_errors.is_empty() {
                return Err(named_arg_errors);
            }
            Ok(Type::Unknown)
        }
        // Unit variant constructor call: [Ok 42] wraps 42 as Ok's payload.
        // Runtime behavior: the `variant` builtin accepts 1 or 2 args — [variant "Tag"] for unit
        // and [variant "Tag" payload] for payload variants. Constructors are ordinary values or
        // functions; no special-casing in invoke_function.
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
                    "unit variant constructor does not accept named arguments",
                    span,
                )]);
            }

            // Infer the argument type
            let arg_ty = infer_surface_expr(&args[0], env, state, type_map).await?;

            // Result type: NominalVariant with the arg type as payload.
            // Runtime stores payload as Some(payload_id), so we model it as a single-field
            // record with numeric field "0" (consistent with single-positional payload convention).
            let mut payload_fields = indexmap::IndexMap::new();
            payload_fields.insert("0".to_string(), arg_ty);
            Ok(Type::NominalVariant {
                tag: tag.clone(),
                fields: Row {
                    fields: payload_fields,
                    tail: crate::type_def::RowTail::Empty,
                },
            })
        }
        // Non-unit variant constructor: already has payload, cannot be called.
        // Example: [Ok 42] where Ok already constructed → type error.
        Type::NominalVariant { .. } => Err(vec![TypeError::new(
            format!("expected function type, got {}", func_ty),
            func_span,
        )]),
        _ => Err(vec![TypeError::new(
            format!("expected function type, got {}", func_ty),
            func_span,
        )]),
    }
}

/// Shared argument checker for both `check_call_with_scheme` (poly path) and `check_call`
/// (mono and poly paths).
///
/// Takes already-unwrapped function type fields (`params`, `ret`, `variadic`,
/// `required_count`) and the `is_poly` flag to select the argument checking strategy:
///
/// - `is_poly: true` — CALL-POLY path: infer each arg with `infer_surface_expr` then
///   unify against the param type.  Used by `check_call_with_scheme` and by
///   `check_call`'s CALL-POLY branch.
/// - `is_poly: false` — CALL-MONO path: use `check_surface_expr` for lambda args and
///   the subsumption-based path for all other args.  Used by `check_call`'s CALL-MONO
///   branch.
///
/// **Deferred arity check:** all supplied args are inferred before the arity check fires,
/// so `got_types` in the `ArityMismatch` error is naturally populated with the actual
/// argument types instead of being left empty.
#[allow(clippy::too_many_arguments)]
async fn check_call_args(
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
                let elem_ty: Option<Type> = if let Type::Record(row) = variadic_param_ty {
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
                if let Type::Record(row) = t {
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn check_call(
    func: &Arc<SurfaceNode>,
    args: &[Arc<SurfaceNode>],
    named_args: &[Spanned<SurfaceNamedArg>],
    env: &Arc<RwLock<Env>>,
    span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Derive callee name from the function expression for error messages.
    let func_callee: Option<String> = match &func.expr {
        crate::ast::SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
        crate::ast::SurfaceExpression::Field {
            field: crate::ast::DotKey::Ident(name),
            ..
        } => Some(name.clone()),
        _ => None,
    };

    let func_ty = infer_surface_expr(func, env, state, type_map).await?;
    // Apply state.subst to resolve any TypeVars bound during infer_expr (e.g., from infer_fn
    // with polymorphic return annotations). Without this, has_inference_vars() incorrectly returns
    // true for already-bound TypeVars, causing CALL-POLY to fire and double-instantiate.
    let func_ty = if state.subst_is_empty() {
        func_ty
    } else {
        state.apply(&func_ty)
    };

    // Error cascade suppression: if the function type is Error (e.g., `include` failed prelude
    // type-checking and was recorded as Type::Error in TypeEnv), infer arguments for side effects
    // and return the Error (with its payload) rather than a bare cascade sentinel.
    // This prevents spurious T003 errors on every [include %libdir "..."] call when the prelude's
    // self-type-check encounters errors. The underlying cause has already been reported.
    // We return `func_ty` (not error_cascade()) so that any payload in the Error propagates
    // to containing call sites — that way `got_types` can show `<error: reason>` not bare `<error>`.
    if matches!(func_ty, Type::Error(_)) {
        // Infer positional args for type map population and error propagation.
        for arg in args {
            let _ = infer_surface_expr(arg, env, state, type_map).await;
        }
        // Infer named args for type map population and error propagation.
        for na in named_args {
            let _ = infer_surface_expr(&na.node.value, env, state, type_map).await;
        }
        return Err(vec![TypeError::not_a_function(&func_ty, func.span.clone())]);
    }

    match &func_ty {
        Type::Function {
            params,
            ret,
            variadic,
            required_count,
        } => {
            // CALL-MONO: function type is fully concrete (no type variables).
            // Use bidirectional checking for arguments via [SUB] rule (doc/06 §[CALL-MONO]).
            if !func_ty.has_inference_vars() {
                return Box::pin(check_call_args(
                    params,
                    ret,
                    *variadic,
                    *required_count,
                    func_callee.as_deref(),
                    args,
                    named_args,
                    env,
                    span,
                    state,
                    constraints,
                    type_map,
                    0,     // constraints_start unused on CALL-MONO path (no TypeVar constraints)
                    false, // is_poly = false → CALL-MONO subsumption path
                ))
                .await;
            }

            // CALL-POLY: function type has type variables.
            // Instantiate the function type at the current level, then delegate to check_call_args.
            let inst_ty = instantiate_at_level(&func_ty, state, &rust_span!());
            let constraints_start = constraints.len();

            let (inst_params, inst_ret, inst_variadic, inst_required_count) = match &inst_ty {
                Type::Function {
                    params,
                    ret,
                    variadic,
                    required_count,
                } => (params, ret, *variadic, *required_count),
                _ => unreachable!("instantiate_at_level preserves Function variant"),
            };

            Box::pin(check_call_args(
                inst_params,
                inst_ret,
                inst_variadic,
                inst_required_count,
                func_callee.as_deref(),
                args,
                named_args,
                env,
                span,
                state,
                constraints,
                type_map,
                constraints_start,
                true, // is_poly
            ))
            .await
        }
        Type::TypeVar(_, _) => {
            // Unbound type variable (e.g. letrec forward reference to a function not yet
            // inferred). state.subst.apply already resolved bound TypeVars (line 1140-1144),
            // so reaching here means alpha is genuinely unbound. Conservative fallback:
            // infer args for side effects and return Unknown.
            // Cascade prevention: ignore arg errors (already recorded as Error in type_map).
            for arg in args {
                let _ = infer_surface_expr(arg, env, state, type_map).await;
            }
            // Infer named arg values exactly once here for type map population and error propagation.
            // Previously these were inferred in a pre-dispatch loop before `match &func_ty`, which
            // caused double-inference when the function type was resolved (CALL-MONO/CALL-POLY arms
            // infer named args again). Moving inference to each arm ensures single-pass Algorithm W.
            let mut named_arg_errors: Vec<TypeError> = Vec::new();
            for na in named_args {
                if let Err(mut errs) =
                    infer_surface_expr(&na.node.value, env, state, type_map).await
                {
                    named_arg_errors.append(&mut errs);
                }
            }
            if !named_arg_errors.is_empty() {
                return Err(named_arg_errors);
            }
            // TypeVar callee — create a fresh TypeVar for the return type to preserve inference.
            // This allows `[f x]` to unify later when `f`'s type becomes known, rather than
            // immediately going gradual with Unknown.
            let ret_var = state
                .fresh_type_var_with(Some("ret"), None, Kind::Type, &span)
                .1;
            Ok(ret_var)
        }
        // Gradual: callee type is Unknown — infer args for LSP hover, return Unknown (check_call path)
        Type::Unknown => {
            // Infer positional args for type map population (needed for LSP hover on Unknown-typed functions).
            // This loop runs only for Unknown-typed callees — for Type::Function arms, positional args are
            // already inferred exactly once in CALL-MONO (check_expr at line 1011) or CALL-POLY (infer_surface_expr).
            // Running it here unconditionally would cause double-inference for Function calls, mutating
            // state.subst.name_counter and state.subst a second time in violation of single-pass Algorithm W.
            // Cascade prevention: ignore arg errors (already recorded as Error in type_map).
            for arg in args {
                let _ = infer_surface_expr(arg, env, state, type_map).await;
            }
            // Infer named arg values exactly once here for type map population and error propagation.
            let mut named_arg_errors: Vec<TypeError> = Vec::new();
            for na in named_args {
                if let Err(mut errs) =
                    infer_surface_expr(&na.node.value, env, state, type_map).await
                {
                    named_arg_errors.append(&mut errs);
                }
            }
            if !named_arg_errors.is_empty() {
                return Err(named_arg_errors);
            }
            Ok(Type::Unknown)
        }
        // Unit variant constructor call: [Ok 42] wraps 42 as Ok's payload.
        // Runtime behavior: the `variant` builtin accepts 1 or 2 args — [variant "Tag"] for unit
        // and [variant "Tag" payload] for payload variants. Constructors are ordinary values or
        // functions; no special-casing in invoke_function.
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
                    "unit variant constructor does not accept named arguments",
                    span,
                )]);
            }

            // Infer the argument type
            let arg_ty = infer_surface_expr(&args[0], env, state, type_map).await?;

            // Result type: NominalVariant with the arg type as payload.
            // Runtime stores payload as Some(payload_id), so we model it as a single-field
            // record with numeric field "0" (consistent with single-positional payload convention).
            let mut payload_fields = indexmap::IndexMap::new();
            payload_fields.insert("0".to_string(), arg_ty);
            Ok(Type::NominalVariant {
                tag: tag.clone(),
                fields: Row {
                    fields: payload_fields,
                    tail: crate::type_def::RowTail::Empty,
                },
            })
        }
        // Non-unit variant constructor: already has payload, cannot be called.
        // Example: [Ok 42] where Ok already constructed → type error.
        // Use func.span (points to the callee) rather than the whole-call span for a more informative error.
        Type::NominalVariant { .. } => Err(vec![TypeError::new(
            format!("expected function type, got {}", func_ty),
            func.span.clone(),
        )]),
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
            // state.apply() (line ~651-655) resolves the TypeVar to Str before this
            // match, bypassing the TypeVar arm (line 1140) that would suppress the error.
            //
            // Fix direction (tracked in B-275): in check_call's VarRef dispatch (line ~1436),
            // after env.get(name) returns a monomorphic scheme with a non-function body, check
            // the env's parent chain for a function-typed binding under the same name. If
            // found AND the current binding came from a same-dict level (detectable via
            // TypeScheme level metadata or a "letrec_placeholder" flag), use the parent
            // binding instead. See typecheck_tests.rs::test_b275_letrec_typevar_does_not_shadow_prelude_function.
            Err(vec![TypeError::new(
                format!("expected function type, got {}", func_ty),
                func.span.clone(),
            )])
        }
    }
}
