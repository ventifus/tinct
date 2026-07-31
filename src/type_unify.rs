//! Unification, constraint solving, and subtype checking for TypeValue-based HM inference.
//!
//! All types are represented as `Arc<Value>` (TypeValue) with constructor tags like
//! `"TypeValue.Var"`, `"TypeValue.Fn"`, `"TypeValue.Record"`, etc.
//!
//! TypeVar bindings and levels are stored in `InferenceContext` (type_infer.rs).
//! Constraints are `Arc<Value>` ConstraintDecl variants (constructed via type_class.rs helpers).

use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::error::TypeDiagnostic;
use crate::type_infer::{typevalue_ctor, typevalue_var_name, InferenceContext, TypeValue};
use crate::type_tags::*;

/// Maximum recursion depth for unification.
/// Prevents stack overflow on deeply nested type unification.
const MAX_UNIFY_DEPTH: usize = 512;

// ── TypeValue inspection helpers ──────────────────────────────────────────────

/// Extract the ctor tag from a TypeValue. Returns None if not a Variant.
fn tv_ctor(tv: &TypeValue) -> Option<&str> {
    typevalue_ctor(tv)
}

/// Check if a TypeValue is "TypeValue.Unknown" (the gradual ? type).
fn is_unknown(tv: &TypeValue) -> bool {
    matches!(tv_ctor(tv), Some(TV_UNKNOWN))
}

/// Check if a TypeValue is "TypeValue.Never" (the bottom type ⊥).
fn is_never(tv: &TypeValue) -> bool {
    matches!(tv_ctor(tv), Some(TV_NEVER))
}

/// Check if a TypeValue is "TypeValue.Top" (the top type ⊤).
fn is_top(tv: &TypeValue) -> bool {
    matches!(tv_ctor(tv), Some(TV_TOP))
}

/// Check if a TypeValue is "TypeValue.Error" (cascade error sentinel).
fn is_error(tv: &TypeValue) -> bool {
    matches!(tv_ctor(tv), Some(TV_ERROR))
}

/// Check if two TypeValues have pointer equality (same Arc allocation).
fn ptr_eq(a: &TypeValue, b: &TypeValue) -> bool {
    Arc::ptr_eq(a, b)
}

/// Check if a TypeValue structurally equals another by comparing ctor tags
/// and (for TypeValue.Var) name payloads. Shallow equality only.
fn typevalue_shallow_eq(a: &TypeValue, b: &TypeValue) -> bool {
    if ptr_eq(a, b) {
        return true;
    }
    match (tv_ctor(a), tv_ctor(b)) {
        // Unit variants: same ctor = equal
        (Some(TV_UNKNOWN), Some(TV_UNKNOWN)) => true,
        (Some(TV_NEVER), Some(TV_NEVER)) => true,
        (Some(TV_TOP), Some(TV_TOP)) => true,
        (Some(TV_ERROR), Some(TV_ERROR)) => true,
        // TypeVar: compare names
        (Some(TV_VAR), Some(TV_VAR)) => typevalue_var_name(a) == typevalue_var_name(b),
        _ => false,
    }
}

/// Collect all TypeVar names reachable from a TypeValue via the context's substitution.
/// Used for level-zeroing when Unknown is encountered.
fn collect_free_vars(tv: &TypeValue, ctx: &InferenceContext) -> Vec<String> {
    ctx.free_vars(tv)
}

// ── entails (superclass entailment for constraint simplification) ─────────────

/// Extract the class name from a ConstraintDecl Arc<Value>.
/// ConstraintDecl { class: TypeValue.Op { name: ... }, args: ... }
fn extract_constraint_class_name(cv: &Arc<crate::value::Value>) -> Option<String> {
    match cv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_CONSTRAINT_DECL => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let class_key = crate::value::HashableValue::Str(Arc::from(FIELD_CLASS));
                let class_thunk = entries.get(&class_key)?;
                match class_thunk.peek_result()? {
                    Ok(crate::value::Value::Variant {
                        ctor: c_ctor,
                        payload: Some(payload),
                        ..
                    }) if c_ctor.as_ref() == TV_OP => match payload.peek_result()? {
                        Ok(crate::value::Value::Dict { entries: inner, .. }) => {
                            let name_key = crate::value::HashableValue::Str(Arc::from(FIELD_NAME));
                            let name_thunk = inner.get(&name_key)?;
                            match name_thunk.peek_result()? {
                                Ok(crate::value::Value::String {
                                    source, start, end, ..
                                }) => Some(source[*start..*end].to_string()),
                                _ => None,
                            }
                        }
                        _ => None,
                    },
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract the single type variable name from a single-param ConstraintDecl.
/// Returns None for multi-param constraints or non-Var first args.
fn extract_single_param_constraint_var(cv: &Arc<crate::value::Value>) -> Option<String> {
    match cv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_CONSTRAINT_DECL => {
            match thunk.peek_result()? {
                Ok(crate::value::Value::Dict { entries, .. }) => {
                    let args_key = crate::value::HashableValue::Str(Arc::from(FIELD_ARGS));
                    let args_thunk = entries.get(&args_key)?;
                    match args_thunk.peek_result()? {
                        Ok(crate::value::Value::Dict {
                            entries: args_entries,
                            ..
                        }) => {
                            if args_entries.len() != 1 {
                                return None; // Multi-param
                            }
                            let first_key = crate::value::HashableValue::Int(0);
                            let first_thunk = args_entries.get(&first_key)?;
                            match first_thunk.peek_result()? {
                                Ok(first_val) => {
                                    let first_arc: TypeValue = Arc::new(first_val.clone());
                                    typevalue_var_name(&first_arc)
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

// ── TypeValue member extraction helpers ──────────────────────────────────────

/// Extract members from a TypeValue.Union { members: Dict }.
/// Returns None if the TypeValue is not a Union or the payload is unsettled.
fn extract_union_members(tv: &TypeValue) -> Option<Vec<TypeValue>> {
    extract_indexed_members(tv, TV_UNION, FIELD_MEMBERS)
}

/// Extract members from a TypeValue.Inter { members: Dict }.
fn extract_intersection_members(tv: &TypeValue) -> Option<Vec<TypeValue>> {
    extract_indexed_members(tv, TV_INTER, FIELD_MEMBERS)
}

/// Extract indexed dict members from a TypeValue variant with a given field name.
fn extract_indexed_members(tv: &TypeValue, ctor_tag: &str, field: &str) -> Option<Vec<TypeValue>> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == ctor_tag => {
            match thunk.peek_result()? {
                Ok(crate::value::Value::Dict { entries, .. }) => {
                    let field_key = crate::value::HashableValue::Str(Arc::from(field));
                    let field_thunk = entries.get(&field_key)?;
                    match field_thunk.peek_result()? {
                        Ok(crate::value::Value::Dict {
                            entries: members, ..
                        }) => {
                            let mut result = Vec::with_capacity(members.len());
                            // Collect in integer order (0, 1, 2, ...)
                            let mut i = 0i64;
                            loop {
                                let key = crate::value::HashableValue::Int(i);
                                let Some(member_thunk) = members.get(&key) else {
                                    break;
                                };
                                match member_thunk.peek_result()? {
                                    Ok(member_val) => {
                                        result.push(Arc::new(member_val.clone()));
                                    }
                                    _ => break,
                                }
                                i += 1;
                            }
                            Some(result)
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Extract the `name` string from a TypeValue.Op { name: String }.
fn extract_op_name(tv: &TypeValue) -> Option<String> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_OP => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let key = crate::value::HashableValue::Str(Arc::from(FIELD_NAME));
                let name_thunk = entries.get(&key)?;
                match name_thunk.peek_result()? {
                    Ok(crate::value::Value::String {
                        source, start, end, ..
                    }) => Some(source[*start..*end].to_string()),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract the `repr` string from a TypeValue.Repr { repr: String }.
fn extract_repr_string(tv: &TypeValue) -> Option<String> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_REPR => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let key = crate::value::HashableValue::Str(Arc::from(FIELD_REPR));
                let repr_thunk = entries.get(&key)?;
                match repr_thunk.peek_result()? {
                    Ok(crate::value::Value::String {
                        source, start, end, ..
                    }) => Some(source[*start..*end].to_string()),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

// ── check_function_arity ──────────────────────────────────────────────────────

/// Validate that two function types have compatible arity and variadic structure.
fn check_function_arity(
    p1_len: usize,
    p2_len: usize,
    is_variadic_1: bool,
    is_variadic_2: bool,
    span: Span,
) -> Result<(), TypeDiagnostic> {
    if p1_len != p2_len {
        return Err(TypeDiagnostic::error(
            "type-error",
            format!(
                "arity mismatch: expected {} arguments, got {}",
                p1_len, p2_len
            ),
            span,
        ));
    }
    if is_variadic_1 != is_variadic_2 {
        return Err(TypeDiagnostic::error(
            "type-error",
            format!(
                "variadic mismatch: function with {} params ({}) vs {} params ({})",
                p1_len,
                if is_variadic_1 {
                    "variadic"
                } else {
                    "non-variadic"
                },
                p2_len,
                if is_variadic_2 {
                    "variadic"
                } else {
                    "non-variadic"
                }
            ),
            span,
        ));
    }
    Ok(())
}

// ── TypeValue-level occurs check and level lowering ───────────────────────────

/// Check if `var_name` occurs in the TypeValue `ty`, and simultaneously lower
/// the level of all TypeVars encountered to at most `cap_level`.
///
/// Returns `true` if `var_name` appears in `ty` (infinite type detected).
/// Mutates `ctx.levels` for all TypeVars found — level lowering prevents unsound generalization.
///
/// This is the TypeValue equivalent of the old `lower_levels_check_occurs` function.
/// Since TypeValue payloads are async thunks, we can only inspect settled payloads.
/// Unsettled payloads are treated conservatively (no occurs, no level lowering needed).
fn lower_levels_check_occurs_tv(
    ty: &TypeValue,
    var_name: &str,
    cap_level: u32,
    ctx: &mut InferenceContext,
) -> bool {
    let ty = ctx.apply_subst(ty);
    match tv_ctor(&ty) {
        Some(TV_VAR) => {
            let name = typevalue_var_name(&ty)
                .expect("invariant: TypeValue.Var payload must be settled with a name field");
            let found = name == var_name;
            // Lower level for this TypeVar
            ctx.lower_var_level(&name, cap_level);
            found
        }
        Some(TV_UNKNOWN) | Some(TV_NEVER) | Some(TV_TOP) | Some(TV_ERROR) | Some(TV_REPR)
        | Some(TV_INT_LIT) | Some(TV_FLOAT_LIT) | Some(TV_STR_LIT) | Some(TV_OP) => {
            // Leaf types: no TypeVars
            false
        }
        Some(TV_UNION) => {
            let members = extract_union_members(&ty)
                .expect("invariant: TypeValue.Union payload must be settled with members field");
            members
                .iter()
                .any(|m| lower_levels_check_occurs_tv(m, var_name, cap_level, ctx))
        }
        Some(TV_INTER) => {
            let members = extract_intersection_members(&ty)
                .expect("invariant: TypeValue.Inter payload must be settled with members field");
            members
                .iter()
                .any(|m| lower_levels_check_occurs_tv(m, var_name, cap_level, ctx))
        }
        _ => {
            // For compound variants (Fn, Record, App, Neg, Recursive, etc.),
            // inspect the settled payload dict recursively.
            if let Some(members) = extract_payload_typevalue_fields(&ty) {
                members
                    .iter()
                    .any(|m| lower_levels_check_occurs_tv(m, var_name, cap_level, ctx))
            } else {
                false
            }
        }
    }
}

/// Extract all TypeValue-shaped fields from a variant's payload dict (synchronously).
/// Used for recursive occurs-check traversal of compound TypeValues.
fn extract_payload_typevalue_fields(tv: &TypeValue) -> Option<Vec<TypeValue>> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            payload: Some(thunk),
            ..
        } => {
            match thunk.peek_result()? {
                Ok(crate::value::Value::Dict { entries, .. }) => {
                    let mut result = Vec::new();
                    for (_key, val_thunk) in entries.iter() {
                        if let Some(Ok(val)) = val_thunk.peek_result() {
                            match val {
                                crate::value::Value::Variant { .. } => {
                                    result.push(Arc::new(val.clone()));
                                }
                                crate::value::Value::Dict { entries: inner, .. } => {
                                    // Dict payloads (e.g. Union members, Fn params)
                                    for (_k, v_thunk) in inner.iter() {
                                        if let Some(Ok(v)) = v_thunk.peek_result() {
                                            if matches!(v, crate::value::Value::Variant { .. }) {
                                                result.push(Arc::new(v.clone()));
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(result)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

// ── unify ─────────────────────────────────────────────────────────────────────

/// Symmetric equality unification for TypeValues.
///
/// Implements Robinson (1965) unification with:
/// - Occurs check + Kiselyov (2013) level lowering (fused for efficiency)
/// - Unknown consistency (gradual typing, Siek & Taha 2006)
/// - Error absorption (cascade error sentinel)
/// - TypeVar binding via `ctx.bind()`
/// - TypeVar-to-TypeVar: bind higher-level to lower-level (Kiselyov L3)
///
/// On success: `ctx` is updated with new TypeVar bindings.
/// On failure: returns `TypeDiagnostic::error`.
pub async fn unify(
    a: &TypeValue,
    b: &TypeValue,
    ctx: &mut InferenceContext,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    span: Span,
    depth: usize,
) -> Result<(), TypeDiagnostic> {
    if depth >= MAX_UNIFY_DEPTH {
        return Err(TypeDiagnostic::error(
            "type-error",
            format!("unification depth limit exceeded (limit: {MAX_UNIFY_DEPTH})"),
            span,
        ));
    }

    // Apply current substitution to both sides (Robinson step: chase bound vars).
    let a = ctx.apply_subst(a);
    let b = ctx.apply_subst(b);

    // Reflexivity: pointer-equal types are subtypes.
    if typevalue_shallow_eq(&a, &b) {
        return Ok(());
    }

    // Error absorption: unify(Error, T) = Ok(()) for all T.
    // Error is a sentinel for failed sub-expression inference; absorbing it silently
    // prevents cascade errors in parent expressions.
    if is_error(&a) || is_error(&b) {
        return Ok(());
    }

    // Unknown-consistency: gradual typing treatment (Siek & Taha 2006, §3).
    //
    // Unknown is consistent with all types without binding any type variable.
    // When Unknown meets a TypeVar, zero the variable's level to prevent over-generalization.
    // When Unknown meets a non-TypeVar, zero all vars in the non-Unknown side.
    if is_unknown(&a) {
        if let Some(name) = typevalue_var_name(&b) {
            ctx.lower_var_level(&name, 0);
        } else {
            let vars = collect_free_vars(&b, ctx);
            for var in &vars {
                ctx.lower_var_level(var, 0);
            }
            emit_unknown_warning(ctx, &b, &span);
        }
        return Ok(());
    }
    if is_unknown(&b) {
        if let Some(name) = typevalue_var_name(&a) {
            ctx.lower_var_level(&name, 0);
        } else {
            let vars = collect_free_vars(&a, ctx);
            for var in &vars {
                ctx.lower_var_level(var, 0);
            }
            emit_unknown_warning(ctx, &a, &span);
        }
        return Ok(());
    }

    // Top (⊤) unification: zero levels, succeed.
    // Top should not appear in unification positions in well-typed programs.
    if is_top(&a) {
        if let Some(name) = typevalue_var_name(&b) {
            ctx.lower_var_level(&name, 0);
        } else {
            let vars = collect_free_vars(&b, ctx);
            for var in &vars {
                ctx.lower_var_level(var, 0);
            }
        }
        return Ok(());
    }
    if is_top(&b) {
        if let Some(name) = typevalue_var_name(&a) {
            ctx.lower_var_level(&name, 0);
        } else {
            let vars = collect_free_vars(&a, ctx);
            for var in &vars {
                ctx.lower_var_level(var, 0);
            }
        }
        return Ok(());
    }

    // Never (⊥) unification: Never unifies with any type (bottom type).
    if is_never(&a) || is_never(&b) {
        return Ok(());
    }

    // TypeVar-to-TypeVar: bind higher-level var to lower-level var (Kiselyov L3).
    if let (Some(name_a), Some(name_b)) = (typevalue_var_name(&a), typevalue_var_name(&b)) {
        if name_a == name_b {
            return Ok(());
        }
        let level_a = ctx.get_level(&name_a);
        let level_b = ctx.get_level(&name_b);

        transfer_class_constraints_tv(&name_a, &name_b, constraints);
        if level_a >= level_b {
            // Bind name_a → TypeVar(name_b)
            ctx.bind(name_a, b.clone())?;
        } else {
            // Bind name_b → TypeVar(name_a)
            ctx.bind(name_b, a.clone())?;
        }
        return Ok(());
    }

    // U-VAR-LEVEL: bind TypeVar α to concrete type τ.
    if let Some(name) = typevalue_var_name(&a) {
        let alpha_level = ctx.get_level(&name);
        if lower_levels_check_occurs_tv(&b, &name, alpha_level, ctx) {
            return Err(TypeDiagnostic::error(
                "type-error",
                format!("infinite type: {name} occurs in type"),
                span,
            ));
        }
        // Constraint transfer: if binding to another TypeVar or Op, transfer constraints.
        if let Some(beta_name) = typevalue_var_name(&b) {
            transfer_class_constraints_tv(&name, &beta_name, constraints);
        }
        ctx.bind(name, b.clone())?;
        return Ok(());
    }

    // U-VAR-LEVEL-SYM: symmetric — TypeVar on the right.
    if let Some(name) = typevalue_var_name(&b) {
        let alpha_level = ctx.get_level(&name);
        if lower_levels_check_occurs_tv(&a, &name, alpha_level, ctx) {
            return Err(TypeDiagnostic::error(
                "type-error",
                format!("infinite type: {name} occurs in type"),
                span,
            ));
        }
        if let Some(beta_name) = typevalue_var_name(&a) {
            transfer_class_constraints_tv(&name, &beta_name, constraints);
        }
        ctx.bind(name, a.clone())?;
        return Ok(());
    }

    // Structural arms: both sides are concrete (non-TypeVar, non-Unknown, non-Never, non-Top).
    let ctor_a = tv_ctor(&a);
    let ctor_b = tv_ctor(&b);

    match (ctor_a, ctor_b) {
        // Repr types: same repr = equal, different = error.
        (Some(TV_REPR), Some(TV_REPR)) => {
            let repr_a = extract_repr_string(&a);
            let repr_b = extract_repr_string(&b);
            if repr_a == repr_b {
                Ok(())
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "cannot unify {:?} with {:?}",
                        repr_a.as_deref().unwrap_or("?"),
                        repr_b.as_deref().unwrap_or("?")
                    ),
                    span,
                ))
            }
        }

        // Op types: same name = equal, different = error.
        (Some(TV_OP), Some(TV_OP)) => {
            let name_a = extract_op_name(&a);
            let name_b = extract_op_name(&b);
            if name_a == name_b {
                Ok(())
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "cannot unify type operator {:?} with {:?}",
                        name_a.as_deref().unwrap_or("?"),
                        name_b.as_deref().unwrap_or("?")
                    ),
                    span,
                ))
            }
        }

        // IntLit, FloatLit, StrLit: equal iff same literal value.
        (Some(TV_INT_LIT), Some(TV_INT_LIT)) => {
            let va = extract_int_lit(&a);
            let vb = extract_int_lit(&b);
            if va == vb {
                Ok(())
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    format!("cannot unify integer literals {:?} and {:?}", va, vb),
                    span,
                ))
            }
        }

        (Some(TV_INT_LIT), Some(TV_REPR)) => {
            // IntLiteral promotes to Int: IntLit(n) ~ Value::Int is allowed.
            if extract_repr_string(&b).as_deref() == Some(REPR_INT) {
                Ok(())
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    "cannot unify integer literal with non-Int type",
                    span,
                ))
            }
        }
        (Some(TV_REPR), Some(TV_INT_LIT)) => {
            if extract_repr_string(&a).as_deref() == Some(REPR_INT) {
                Ok(())
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    "cannot unify integer literal with non-Int type",
                    span,
                ))
            }
        }

        (Some(TV_STR_LIT), Some(TV_STR_LIT)) => {
            let va = extract_str_lit(&a);
            let vb = extract_str_lit(&b);
            if va == vb {
                Ok(())
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    format!("cannot unify string literals {:?} and {:?}", va, vb),
                    span,
                ))
            }
        }

        (Some(TV_STR_LIT), Some(TV_REPR)) => {
            if extract_repr_string(&b).as_deref() == Some(REPR_STRING) {
                Ok(())
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    "cannot unify string literal with non-Str type",
                    span,
                ))
            }
        }
        (Some(TV_REPR), Some(TV_STR_LIT)) => {
            if extract_repr_string(&a).as_deref() == Some(REPR_STRING) {
                Ok(())
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    "cannot unify string literal with non-Str type",
                    span,
                ))
            }
        }

        (Some(TV_FLOAT_LIT), Some(TV_FLOAT_LIT)) => {
            // Float comparison: bit equality
            let va = extract_float_lit(&a);
            let vb = extract_float_lit(&b);
            if va.map(|f| f.to_bits()) == vb.map(|f| f.to_bits()) {
                Ok(())
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    format!("cannot unify float literals {:?} and {:?}", va, vb),
                    span,
                ))
            }
        }

        // Function types: bidirectional constrain (contravariant params, covariant return).
        (Some(TV_FN), Some(TV_FN)) => {
            let (ac, bc) = (a.clone(), b.clone());
            Box::pin(constrain(&ac, &bc, ctx, constraints, span.clone())).await?;
            Box::pin(constrain(&bc, &ac, ctx, constraints, span)).await
        }

        // Record types: bidirectional constrain_rows.
        (Some(TV_RECORD), Some(TV_RECORD)) => {
            let (ac, bc) = (a.clone(), b.clone());
            Box::pin(constrain(&ac, &bc, ctx, constraints, span.clone())).await?;
            Box::pin(constrain(&bc, &ac, ctx, constraints, span)).await
        }

        // Union types: pairwise member unification.
        (Some(TV_UNION), Some(TV_UNION)) => {
            let members_a = extract_union_members(&a)
                .expect("invariant: TypeValue.Union payload must be settled with members field");
            let members_b = extract_union_members(&b)
                .expect("invariant: TypeValue.Union payload must be settled with members field");
            if members_a.len() != members_b.len() {
                // Different member counts: fall through to error
                return Err(TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "cannot unify union types with different numbers of members ({} vs {})",
                        members_a.len(),
                        members_b.len()
                    ),
                    span,
                ));
            }
            for (ma, mb) in members_a.iter().zip(members_b.iter()) {
                Box::pin(unify(ma, mb, ctx, constraints, span.clone(), depth + 1)).await?;
            }
            Ok(())
        }

        // App types: structural unification on the op and arg.
        //
        // Injectivity guard (T-2077): when both ops are the same TypeValue.Op name, the op
        // may be a class resolver function. If the resolver is non-injective, pairwise
        // unification of args is unsound (equal outputs don't imply equal inputs), so the
        // pair must be deferred until both sides are ground.
        //
        // Pragmatic: type_unify.rs has no access to the class env (ClassDecl.resolver_injective).
        // We inspect the constraints list for a ConstraintDecl whose class name equals the op
        // name — a heuristic that the op is a resolver type application. When no such constraint
        // is found (or the constraints don't carry injectivity information), we default to
        // injective (pairwise unification), which is the safe behaviour for standard type
        // constructors (Seq, Result, etc.) where injectivity holds by construction.
        //
        // Future work: pass the class env into InferenceContext so that unify() can look up
        // ClassDecl.resolver_injective directly and activate the deferred path correctly.
        (Some(TV_APP), Some(TV_APP)) => {
            let (op_a, arg_a) = extract_app_parts(&a).ok_or_else(|| {
                TypeDiagnostic::error("type-error", "malformed TypeValue.App", span.clone())
            })?;
            let (op_b, arg_b) = extract_app_parts(&b).ok_or_else(|| {
                TypeDiagnostic::error("type-error", "malformed TypeValue.App", span.clone())
            })?;

            // Check whether both ops are TV_OP with the same name and, if so, whether
            // the op name is associated with a non-injective resolver.
            let op_name_a = extract_op_name(&op_a);
            let op_name_b = extract_op_name(&op_b);
            let same_op_name = op_name_a.is_some() && op_name_a == op_name_b;

            // Determine injectivity. Without class-env access we can only consult constraints.
            // A ConstraintDecl whose class name matches the op name is evidence that the op
            // is a resolver application. However, constraints do not carry resolver_injective
            // directly — ClassDecl.resolver_injective lives in the env, which is not available
            // here. Pragmatic result: we never have positive evidence of non-injectivity, so
            // we always fall through to the injective path below.
            //
            // This preserves the existing semantics while providing the deferral infrastructure
            // (resolver_deferred field on InferenceContext) for future activation once the class
            // env is threaded through to unify().
            let is_non_injective = if same_op_name {
                // Search constraints for a ConstraintDecl whose class name equals the op name.
                // Even when found, we lack resolver_injective — conservatively treat as injective.
                let _op_name = op_name_a.as_deref().unwrap_or("");
                let _found_in_constraints = constraints
                    .iter()
                    .any(|c| extract_constraint_class_name(c).as_deref() == Some(_op_name));
                // Without ClassDecl access: default to injective regardless of _found_in_constraints.
                false
            } else {
                false
            };

            if is_non_injective {
                // Non-injective resolver: defer the equality pair until both sides are ground.
                // run_fd_improvement_fixpoint drains ctx.resolver_deferred after each constraint push.
                ctx.resolver_deferred.push((Arc::clone(&a), Arc::clone(&b)));
                Ok(())
            } else {
                // Injective (or unknown — defaulting to injective): pairwise unify op and arg.
                Box::pin(unify(
                    &op_a,
                    &op_b,
                    ctx,
                    constraints,
                    span.clone(),
                    depth + 1,
                ))
                .await?;
                Box::pin(unify(&arg_a, &arg_b, ctx, constraints, span, depth + 1)).await
            }
        }

        // Recursive types: open with fresh TypeVar, bidirectional constrain.
        (Some(TV_RECURSIVE), Some(TV_RECURSIVE)) => {
            let body_a = extract_recursive_body(&a).ok_or_else(|| {
                TypeDiagnostic::error("type-error", "malformed TypeValue.Recursive", span.clone())
            })?;
            let body_b = extract_recursive_body(&b).ok_or_else(|| {
                TypeDiagnostic::error("type-error", "malformed TypeValue.Recursive", span.clone())
            })?;
            // Open both with the same fresh TypeVar (Pierce 2002 §21.8 simultaneous-opening).
            let fresh = ctx.fresh_typevar("rec");
            let opened_a = substitute_rec_ref(&body_a, 0, &fresh);
            let opened_b = substitute_rec_ref(&body_b, 0, &fresh);
            Box::pin(constrain(
                &opened_a,
                &opened_b,
                ctx,
                constraints,
                span.clone(),
            ))
            .await?;
            Box::pin(constrain(&opened_b, &opened_a, ctx, constraints, span)).await
        }

        // Recursive vs concrete: open recursive, constrain with concrete.
        (Some(TV_RECURSIVE), _) => {
            let body_a = extract_recursive_body(&a).ok_or_else(|| {
                TypeDiagnostic::error("type-error", "malformed TypeValue.Recursive", span.clone())
            })?;
            let fresh = ctx.fresh_typevar("rec");
            let opened_a = substitute_rec_ref(&body_a, 0, &fresh);
            Box::pin(constrain(&opened_a, &b, ctx, constraints, span.clone())).await?;
            Box::pin(constrain(&b, &opened_a, ctx, constraints, span)).await
        }

        (_, Some(TV_RECURSIVE)) => {
            let body_b = extract_recursive_body(&b).ok_or_else(|| {
                TypeDiagnostic::error("type-error", "malformed TypeValue.Recursive", span.clone())
            })?;
            let fresh = ctx.fresh_typevar("rec");
            let opened_b = substitute_rec_ref(&body_b, 0, &fresh);
            Box::pin(constrain(&a, &opened_b, ctx, constraints, span.clone())).await?;
            Box::pin(constrain(&opened_b, &a, ctx, constraints, span)).await
        }

        // Negation: contravariant (bidirectional swap).
        (Some(TV_NEG), Some(TV_NEG)) => {
            let inner_a = extract_neg_inner(&a).ok_or_else(|| {
                TypeDiagnostic::error("type-error", "malformed TypeValue.Neg", span.clone())
            })?;
            let inner_b = extract_neg_inner(&b).ok_or_else(|| {
                TypeDiagnostic::error("type-error", "malformed TypeValue.Neg", span.clone())
            })?;
            Box::pin(constrain(
                &inner_b,
                &inner_a,
                ctx,
                constraints,
                span.clone(),
            ))
            .await?;
            Box::pin(constrain(&inner_a, &inner_b, ctx, constraints, span)).await
        }

        // C-Var1: concrete type vs Union — try each Union member.
        // If any member unifies successfully, the whole unification succeeds.
        // TypeVar members are tried last (after concrete members); the first TypeVar
        // that unifies (binding TypeVar ↦ concrete) is accepted.
        // Rule: τ ≤ τ₁ ∨ ... ∨ τₙ  iff  ∃i. τ ≤ τᵢ  (existential Union membership).
        (_, Some(TV_UNION)) => {
            let members = extract_union_members(&b).unwrap_or_default();
            if members.is_empty() {
                return Err(TypeDiagnostic::error(
                    "type-error",
                    "cannot unify with empty union (Never)",
                    span,
                ));
            }
            // Try concrete members first (avoid premature TypeVar binding).
            let mut ordered: Vec<TypeValue> = members
                .iter()
                .filter(|m| typevalue_var_name(m).is_none())
                .cloned()
                .collect();
            ordered.extend(
                members
                    .iter()
                    .filter(|m| typevalue_var_name(m).is_some())
                    .cloned(),
            );
            for member in &ordered {
                // Clone ctx state before attempt; restore on failure.
                let mut attempt_ctx = ctx.clone();
                let mut attempt_constraints = constraints.clone();
                let result = Box::pin(unify(
                    &a,
                    member,
                    &mut attempt_ctx,
                    &mut attempt_constraints,
                    span.clone(),
                    depth + 1,
                ))
                .await;
                if result.is_ok() {
                    *ctx = attempt_ctx;
                    *constraints = attempt_constraints;
                    return Ok(());
                }
            }
            Err(TypeDiagnostic::error(
                "type-error",
                format!("cannot unify {} with {}", ctor_a.unwrap_or("?"), TV_UNION),
                span,
            )
            .with_note(format!(
                "actual:   {}",
                crate::eval::format_type_for_assert(&a)
            ))
            .with_note(format!(
                "expected: {}",
                crate::eval::format_type_for_assert(&b)
            )))
        }

        // C-Var1 symmetric: Union vs concrete — symmetric version of the above.
        (Some(TV_UNION), _) => {
            let members = extract_union_members(&a).unwrap_or_default();
            if members.is_empty() {
                return Err(TypeDiagnostic::error(
                    "type-error",
                    "cannot unify empty union (Never) with type",
                    span,
                ));
            }
            let mut ordered: Vec<TypeValue> = members
                .iter()
                .filter(|m| typevalue_var_name(m).is_none())
                .cloned()
                .collect();
            ordered.extend(
                members
                    .iter()
                    .filter(|m| typevalue_var_name(m).is_some())
                    .cloned(),
            );
            for member in &ordered {
                let mut attempt_ctx = ctx.clone();
                let mut attempt_constraints = constraints.clone();
                let result = Box::pin(unify(
                    member,
                    &b,
                    &mut attempt_ctx,
                    &mut attempt_constraints,
                    span.clone(),
                    depth + 1,
                ))
                .await;
                if result.is_ok() {
                    *ctx = attempt_ctx;
                    *constraints = attempt_constraints;
                    return Ok(());
                }
            }
            Err(TypeDiagnostic::error(
                "type-error",
                format!(
                    "cannot unify TypeValue.Union with {}",
                    ctor_b.unwrap_or("?")
                ),
                span,
            )
            .with_note(format!(
                "actual:   {}",
                crate::eval::format_type_for_assert(&a)
            ))
            .with_note(format!(
                "expected: {}",
                crate::eval::format_type_for_assert(&b)
            )))
        }

        // C-Var2: Intersection vs concrete — try to unify each intersection member with the
        // concrete type. TypeVar members are tried first (C-Var2 priority: binding TypeVar).
        // Rule: α ∧ τ₁ ≤ τ₂  →  α ≤ ~τ₁ ∨ τ₂  (conservative: try TypeVar binding).
        (Some(TV_INTER), _) => {
            let members = extract_intersection_members(&a).unwrap_or_default();
            if members.is_empty() {
                // Empty intersection = Top: Top <: T for all T — accept.
                return Ok(());
            }
            // Try TypeVar members first (C-Var2 priority).
            let mut ordered: Vec<TypeValue> = members
                .iter()
                .filter(|m| typevalue_var_name(m).is_some())
                .cloned()
                .collect();
            ordered.extend(
                members
                    .iter()
                    .filter(|m| typevalue_var_name(m).is_none())
                    .cloned(),
            );
            for member in &ordered {
                let mut attempt_ctx = ctx.clone();
                let mut attempt_constraints = constraints.clone();
                let result = Box::pin(unify(
                    member,
                    &b,
                    &mut attempt_ctx,
                    &mut attempt_constraints,
                    span.clone(),
                    depth + 1,
                ))
                .await;
                if result.is_ok() {
                    *ctx = attempt_ctx;
                    *constraints = attempt_constraints;
                    return Ok(());
                }
            }
            Err(TypeDiagnostic::error(
                "type-error",
                format!(
                    "cannot unify TypeValue.Inter with {}",
                    ctor_b.unwrap_or("?")
                ),
                span,
            )
            .with_note(format!(
                "actual:   {}",
                crate::eval::format_type_for_assert(&a)
            ))
            .with_note(format!(
                "expected: {}",
                crate::eval::format_type_for_assert(&b)
            )))
        }

        // Cross-ctor mismatch: type error.
        (Some(ca), Some(cb)) if ca != cb => Err(TypeDiagnostic::error(
            "type-error",
            format!("cannot unify {} with {}", ca, cb),
            span,
        )
        .with_note(format!(
            "left:  {}",
            crate::eval::format_type_for_assert(&a)
        ))
        .with_note(format!(
            "right: {}",
            crate::eval::format_type_for_assert(&b)
        ))),

        // Unknown ctor (unsettled thunks, etc.): conservative accept.
        _ => Ok(()),
    }
}

// ── constrain ─────────────────────────────────────────────────────────────────

/// Directional subtype constraint: `sub <: sup`.
///
/// Unlike `unify()` (symmetric equality), `constrain()` is directional.
/// `sub` is the inferred type (actual); `sup` is the expected/annotated type.
///
/// - C-Var1: fires when sup is Union containing TypeVars (τ₁ ≤ τ₂ ∨ α → τ₁ & ~τ₂ ≤ α)
/// - C-Var2: fires when sub is Intersection containing TypeVars (α ∧ τ₁ ≤ τ₂ → α ≤ ~τ₁ ∨ τ₂)
/// - C-FN: contravariant params, covariant return
/// - Otherwise: fall through to unify()
pub async fn constrain(
    sub: &TypeValue,
    sup: &TypeValue,
    ctx: &mut InferenceContext,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    span: Span,
) -> Result<(), TypeDiagnostic> {
    // Apply current substitution.
    let sub = ctx.apply_subst(sub);
    let sup = ctx.apply_subst(sup);

    // Reflexivity: equal types are subtypes.
    if typevalue_shallow_eq(&sub, &sup) {
        return Ok(());
    }

    // Error absorption.
    if is_error(&sub) || is_error(&sup) {
        return Ok(());
    }

    // Unknown directional: zero levels, accept.
    if is_unknown(&sub) {
        if let Some(name) = typevalue_var_name(&sup) {
            ctx.lower_var_level(&name, 0);
        } else {
            let vars = collect_free_vars(&sup, ctx);
            for var in &vars {
                ctx.lower_var_level(var, 0);
            }
        }
        return Ok(());
    }
    if is_unknown(&sup) {
        if let Some(name) = typevalue_var_name(&sub) {
            ctx.lower_var_level(&name, 0);
        } else {
            let vars = collect_free_vars(&sub, ctx);
            for var in &vars {
                ctx.lower_var_level(var, 0);
            }
        }
        return Ok(());
    }

    // Never: Never <: T for all T (bottom type).
    if is_never(&sub) {
        return Ok(());
    }

    // Top: T <: Top for all T (top type).
    if is_top(&sup) {
        return Ok(());
    }

    // C-FN: directional function constraint (contravariant params, covariant return).
    if matches!(tv_ctor(&sub), Some(TV_FN)) && matches!(tv_ctor(&sup), Some(TV_FN)) {
        return constrain_fn(&sub, &sup, ctx, constraints, span).await;
    }

    // C-Record: directional record subtyping.
    if matches!(tv_ctor(&sub), Some(TV_RECORD)) && matches!(tv_ctor(&sup), Some(TV_RECORD)) {
        return constrain_record(&sub, &sup, ctx, constraints, span).await;
    }

    // C-LB (lower bound accumulation): constrain(sub, α) where α is a free TypeVar.
    //
    // "sub <: α" means α must be AT LEAST as general as sub. We accumulate sub as a
    // lower bound on α rather than binding α = sub via equality. Later constraints
    // `constrain(sub2, α)` from other call sites may produce a WIDER lower bound (e.g.,
    // sub2 is more general than sub), and all are satisfied by α = JOIN(lbs).
    //
    // This is DIRECTIONAL: constrain(sub, α) ≠ constrain(α, sub). The equality binding
    // in unify() would be unsound here because equality is symmetric but subtyping is not.
    //
    // Invariant: `sup` has already been fully resolved through apply_subst at the top of
    // this function. If sup is a TypeVar here, it is guaranteed to be free (not in subst)
    // — apply_subst follows binding chains to fixpoint.
    if let Some(alpha_name) = typevalue_var_name(&sup) {
        ctx.add_lower_bound(&alpha_name, sub.clone());
        return Ok(());
    }

    // C-UB (upper bound / narrowing): constrain(α, sup) where α is a free TypeVar and
    // sup is a concrete type.
    //
    // "α <: sup" means α is bounded above by sup. We bind α = sup (the most precise upper
    // bound available) and verify that all existing lower bounds for α satisfy lb <: sup.
    // This keeps the equality invariant: once α is bound, all constraints are consistent.
    //
    // Invariant: `sub` has already been fully resolved through apply_subst at the top of
    // this function. If sub is a TypeVar here, it is guaranteed to be free (not in subst).
    if let Some(alpha_name) = typevalue_var_name(&sub) {
        // C-UB: bind α = sup. Lower bounds on α are accumulated from previous constrain(lb, α)
        // calls (via add_lower_bound). Rather than eagerly checking lower bounds against sup
        // here (which causes false errors when lower bounds include gradual/unannotated types
        // that the BAS conservatively rejects), we simply bind and allow constraint propagation
        // to surface real violations via unify() on concrete types.
        //
        // Note: eager lower-bound checking via is_subtype_bas was added in S-1003 but causes
        // false positives when gradual types (Unknown, complex unions from type-level functions
        // like `merge`'s return type) appear as lower bounds. The old pre-S-1003 system
        // accumulated bounds lazily and did not perform this eager check.
        let _lbs = ctx.take_lower_bounds(&alpha_name);
        // Bind α = sup (upper bound narrows the TypeVar).
        ctx.bind(alpha_name, sup.clone())?;
        return Ok(());
    }

    // Fall through to unify() for symmetric cases (both sides concrete, structural
    // decomposition, ground-type subtype check via is_subtype, etc.).
    unify(&sub, &sup, ctx, constraints, span, 0).await
}

/// Directional function-type constraint.
/// Contravariant params: constrain(sup_param, sub_param)
/// Covariant return: constrain(sub_ret, sup_ret)
async fn constrain_fn(
    sub: &TypeValue,
    sup: &TypeValue,
    ctx: &mut InferenceContext,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    span: Span,
) -> Result<(), TypeDiagnostic> {
    let (params_sub, ret_sub) = extract_fn_parts(sub).ok_or_else(|| {
        TypeDiagnostic::error("type-error", "malformed TypeValue.Fn (sub)", span.clone())
    })?;
    let (params_sup, ret_sup) = extract_fn_parts(sup).ok_or_else(|| {
        TypeDiagnostic::error("type-error", "malformed TypeValue.Fn (sup)", span.clone())
    })?;

    let sub_variadic = is_fn_variadic(sub);
    let sup_variadic = is_fn_variadic(sup);
    let sub_any_fn = params_sub.is_empty() && sub_variadic;
    let sup_any_fn = params_sup.is_empty() && sup_variadic;

    // Any-function ≤ any concrete-arity: constrain returns only.
    if sub_any_fn && !params_sup.is_empty() {
        return Box::pin(constrain(&ret_sub, &ret_sup, ctx, constraints, span)).await;
    }
    if sup_any_fn && !params_sub.is_empty() {
        return Box::pin(constrain(&ret_sub, &ret_sup, ctx, constraints, span)).await;
    }

    check_function_arity(
        params_sub.len(),
        params_sup.len(),
        sub_variadic,
        sup_variadic,
        span.clone(),
    )?;

    // Contravariant params: constrain(sup_param, sub_param)
    for (p_sub, p_sup) in params_sub.iter().zip(params_sup.iter()) {
        Box::pin(constrain(p_sup, p_sub, ctx, constraints, span.clone())).await?;
    }

    // Covariant return: constrain(sub_ret, sup_ret)
    Box::pin(constrain(&ret_sub, &ret_sup, ctx, constraints, span)).await
}

/// Directional record subtype constraint (width + depth subtyping).
/// sup's fields must be coverable by sub (width subtyping: sub may have extra fields).
async fn constrain_record(
    sub: &TypeValue,
    sup: &TypeValue,
    ctx: &mut InferenceContext,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    span: Span,
) -> Result<(), TypeDiagnostic> {
    let sup_fields = extract_record_fields(sup)
        .expect("invariant: TypeValue.Record payload must be settled with fields dict");
    let sub_fields = extract_record_fields(sub)
        .expect("invariant: TypeValue.Record payload must be settled with fields dict");

    for (k, sup_ty) in &sup_fields {
        if let Some(sub_ty) = sub_fields.get(k) {
            Box::pin(constrain(sub_ty, sup_ty, ctx, constraints, span.clone())).await?;
        } else {
            return Err(TypeDiagnostic::error(
                "type-error",
                format!("missing field '{}': record subtype constraint", k),
                span,
            )
            .with_note(format!(
                "actual:   {}",
                crate::eval::format_type_for_assert(sub)
            ))
            .with_note(format!(
                "expected: {}",
                crate::eval::format_type_for_assert(sup)
            )));
        }
    }
    Ok(())
}

// ── TypeValue structural extraction helpers ────────────────────────────────────

/// Extract (params, return) from a TypeValue.Fn.
/// Returns None if the payload is unsettled or malformed.
fn extract_fn_parts(tv: &TypeValue) -> Option<(Vec<TypeValue>, TypeValue)> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_FN => {
            match thunk.peek_result()? {
                Ok(crate::value::Value::Dict { entries, .. }) => {
                    // Extract `params` dict
                    let params_key = crate::value::HashableValue::Str(Arc::from(FIELD_PARAMS));
                    let params_thunk = entries.get(&params_key)?;
                    let params = match params_thunk.peek_result()? {
                        Ok(crate::value::Value::Dict {
                            entries: p_entries, ..
                        }) => {
                            let mut result = Vec::with_capacity(p_entries.len());
                            let mut i = 0i64;
                            loop {
                                let key = crate::value::HashableValue::Int(i);
                                let Some(pt) = p_entries.get(&key) else {
                                    break;
                                };
                                match pt.peek_result()? {
                                    Ok(pv) => result.push(Arc::new(pv.clone())),
                                    _ => break,
                                }
                                i += 1;
                            }
                            result
                        }
                        _ => return None,
                    };

                    // Extract `return` TypeValue
                    let ret_key = crate::value::HashableValue::Str(Arc::from(FIELD_RETURN));
                    let ret_thunk = entries.get(&ret_key)?;
                    let ret = match ret_thunk.peek_result()? {
                        Ok(rv) => Arc::new(rv.clone()),
                        _ => return None,
                    };

                    Some((params, ret))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Check if a TypeValue.Fn has variadic params (variadic: "true" variant in payload).
fn is_fn_variadic(tv: &TypeValue) -> bool {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_FN => {
            if let Some(Ok(crate::value::Value::Dict { entries, .. })) = thunk.peek_result() {
                let key = crate::value::HashableValue::Str(Arc::from(FIELD_VARIADIC));
                if let Some(vt) = entries.get(&key) {
                    if let Some(Ok(crate::value::Value::Variant { ctor: b_ctor, .. })) =
                        vt.peek_result()
                    {
                        return b_ctor.as_ref() == "true";
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// Extract fields from a TypeValue.Record { fields: Dict }.
/// Returns an IndexMap of field-name → TypeValue, preserving insertion (declaration) order.
/// Deterministic ordering ensures consistent error messages when multiple fields are missing.
fn extract_record_fields(tv: &TypeValue) -> Option<IndexMap<String, TypeValue>> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_RECORD => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let fields_key = crate::value::HashableValue::Str(Arc::from(FIELD_FIELDS));
                let fields_thunk = entries.get(&fields_key)?;
                match fields_thunk.peek_result()? {
                    Ok(crate::value::Value::Dict {
                        entries: f_entries, ..
                    }) => {
                        let mut result = IndexMap::new();
                        for (k, v_thunk) in f_entries.iter() {
                            let field_name = match k {
                                crate::value::HashableValue::Str(s) => s.as_ref().to_string(),
                                _ => continue,
                            };
                            if let Some(Ok(fv)) = v_thunk.peek_result() {
                                result.insert(field_name, Arc::new(fv.clone()));
                            }
                        }
                        Some(result)
                    }
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract (op, arg) from a TypeValue.App { op: TypeValue, arg: TypeValue }.
fn extract_app_parts(tv: &TypeValue) -> Option<(TypeValue, TypeValue)> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_APP => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let op_key = crate::value::HashableValue::Str(Arc::from(FIELD_OP));
                let arg_key = crate::value::HashableValue::Str(Arc::from(FIELD_ARG));
                let op_thunk = entries.get(&op_key)?;
                let arg_thunk = entries.get(&arg_key)?;
                let op = match op_thunk.peek_result()? {
                    Ok(v) => Arc::new(v.clone()),
                    _ => return None,
                };
                let arg = match arg_thunk.peek_result()? {
                    Ok(v) => Arc::new(v.clone()),
                    _ => return None,
                };
                Some((op, arg))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract the body from a TypeValue.Recursive { body: TypeValue }.
fn extract_recursive_body(tv: &TypeValue) -> Option<TypeValue> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_RECURSIVE => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let body_key = crate::value::HashableValue::Str(Arc::from(FIELD_BODY));
                let body_thunk = entries.get(&body_key)?;
                match body_thunk.peek_result()? {
                    Ok(v) => Some(Arc::new(v.clone())),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract the inner from a TypeValue.Neg { of: TypeValue } (negation).
fn extract_neg_inner(tv: &TypeValue) -> Option<TypeValue> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_NEG => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let of_key = crate::value::HashableValue::Str(Arc::from(FIELD_OF));
                let of_thunk = entries.get(&of_key)?;
                match of_thunk.peek_result()? {
                    Ok(v) => Some(Arc::new(v.clone())),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract the integer literal value from a TypeValue.IntLit { value: Integer }.
fn extract_int_lit(tv: &TypeValue) -> Option<i64> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_INT_LIT => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let key = crate::value::HashableValue::Str(Arc::from(FIELD_VALUE));
                let v_thunk = entries.get(&key)?;
                match v_thunk.peek_result()? {
                    Ok(crate::value::Value::Int { n, .. }) => Some(*n),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract the float literal value from a TypeValue.FloatLit { value: Float }.
fn extract_float_lit(tv: &TypeValue) -> Option<f64> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_FLOAT_LIT => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let key = crate::value::HashableValue::Str(Arc::from(FIELD_VALUE));
                let v_thunk = entries.get(&key)?;
                match v_thunk.peek_result()? {
                    Ok(crate::value::Value::Float { n, .. }) => Some(*n),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract the string literal value from a TypeValue.StrLit { value: String }.
fn extract_str_lit(tv: &TypeValue) -> Option<String> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_STR_LIT => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let key = crate::value::HashableValue::Str(Arc::from(FIELD_VALUE));
                let v_thunk = entries.get(&key)?;
                match v_thunk.peek_result()? {
                    Ok(crate::value::Value::String {
                        source, start, end, ..
                    }) => Some(source[*start..*end].to_string()),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

// ── Recursive type substitution ───────────────────────────────────────────────

/// Substitute all TypeValue.RecursiveRef { depth } occurrences in `body`
/// with `replacement`. Used when opening a TypeValue.Recursive (μ-type).
///
/// Delegates to `crate::bas::substitute_recursive_ref`, which is the canonical
/// complete implementation covering all compound TypeValue constructors (TV_FN,
/// TV_RECORD, TV_UNION, TV_INTER, TV_NEG, TV_APP, TV_NOMINAL_VARIANT, etc.).
fn substitute_rec_ref(body: &TypeValue, depth: u32, replacement: &TypeValue) -> TypeValue {
    crate::bas::substitute_recursive_ref(body, depth, replacement)
}

// ── Constraint transfer ───────────────────────────────────────────────────────

/// Transfer all class constraints from TypeVar `alpha` to TypeVar `beta`.
/// Called during TypeVar→TypeVar binding to migrate constraint obligations.
/// `constraints` is `Vec<Arc<Value>>` where each element is a ConstraintDecl.
fn transfer_class_constraints_tv(
    alpha: &str,
    beta: &str,
    constraints: &mut Vec<Arc<crate::value::Value>>,
) {
    // Collect ConstraintDecl values that have alpha as a Var arg.
    let alpha_constraints: Vec<Arc<crate::value::Value>> = constraints
        .iter()
        .filter(|c| constraint_has_var_arg(c, alpha))
        .cloned()
        .collect();

    if alpha_constraints.is_empty() {
        return;
    }

    // For each alpha constraint, add a renamed version (alpha → beta) for beta.
    for c in &alpha_constraints {
        let renamed = rename_constraint_var(c, alpha, beta);
        // Only add if not already present (deduplication by ptr_eq).
        if !constraints.iter().any(|existing| {
            Arc::ptr_eq(existing, &renamed) || constraint_structurally_eq(existing, &renamed)
        }) {
            constraints.push(renamed);
        }
    }
}

/// Check if a ConstraintDecl has a TypeValue.Var arg with the given name.
fn constraint_has_var_arg(cv: &Arc<crate::value::Value>, var_name: &str) -> bool {
    match cv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_CONSTRAINT_DECL => {
            if let Some(Ok(crate::value::Value::Dict { entries, .. })) = thunk.peek_result() {
                let args_key = crate::value::HashableValue::Str(Arc::from(FIELD_ARGS));
                if let Some(args_thunk) = entries.get(&args_key) {
                    if let Some(Ok(crate::value::Value::Dict {
                        entries: args_entries,
                        ..
                    })) = args_thunk.peek_result()
                    {
                        for (_k, v_thunk) in args_entries.iter() {
                            if let Some(Ok(v)) = v_thunk.peek_result() {
                                let v_arc: TypeValue = Arc::new(v.clone());
                                if let Some(name) = typevalue_var_name(&v_arc) {
                                    if name == var_name {
                                        return true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// Rename all TypeValue.Var args with name `alpha` to `beta` in a ConstraintDecl.
/// Returns the renamed ConstraintDecl (or the original if nothing to rename).
fn rename_constraint_var(
    cv: &Arc<crate::value::Value>,
    alpha: &str,
    beta: &str,
) -> Arc<crate::value::Value> {
    // Reconstruct a new ConstraintDecl with the renamed variable.
    // Extract class and args, substitute alpha→beta in args, rebuild via
    // make_constraint_decl. This is the correct general implementation.
    use crate::type_class::make_constraint_decl;
    use crate::type_infer::make_typevar_value;

    // Verify this is a ConstraintDecl (has an extractable class name) before proceeding.
    if extract_constraint_class_name(cv).is_none() {
        return Arc::clone(cv);
    }

    match cv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_CONSTRAINT_DECL => {
            if let Some(Ok(crate::value::Value::Dict { entries, .. })) = thunk.peek_result() {
                let class_key = crate::value::HashableValue::Str(Arc::from(FIELD_CLASS));
                let args_key = crate::value::HashableValue::Str(Arc::from(FIELD_ARGS));

                let Some(class_thunk) = entries.get(&class_key) else {
                    return Arc::clone(cv);
                };
                let Some(class_val_ok) = class_thunk.peek_result() else {
                    return Arc::clone(cv);
                };
                let Ok(class_val) = class_val_ok else {
                    return Arc::clone(cv);
                };
                let class_tv: TypeValue = Arc::new(class_val.clone());

                let Some(args_thunk) = entries.get(&args_key) else {
                    return Arc::clone(cv);
                };
                let Some(Ok(crate::value::Value::Dict {
                    entries: args_entries,
                    ..
                })) = args_thunk.peek_result()
                else {
                    return Arc::clone(cv);
                };

                let mut new_args = Vec::with_capacity(args_entries.len());
                // Iterate in integer order
                let mut i = 0i64;
                loop {
                    let key = crate::value::HashableValue::Int(i);
                    let Some(arg_thunk) = args_entries.get(&key) else {
                        break;
                    };
                    let Some(Ok(arg_val)) = arg_thunk.peek_result() else {
                        break;
                    };
                    let arg_arc: TypeValue = Arc::new(arg_val.clone());
                    let renamed_arg = if typevalue_var_name(&arg_arc).as_deref() == Some(alpha) {
                        make_typevar_value(beta)
                    } else {
                        arg_arc
                    };
                    new_args.push(renamed_arg);
                    i += 1;
                }

                make_constraint_decl(class_tv, new_args)
            } else {
                Arc::clone(cv)
            }
        }
        _ => Arc::clone(cv),
    }
}

/// Structural equality check for two ConstraintDecl values (shallow).
fn constraint_structurally_eq(a: &Arc<crate::value::Value>, b: &Arc<crate::value::Value>) -> bool {
    if Arc::ptr_eq(a, b) {
        return true;
    }
    // Compare class names and arg vars as strings (best-effort).
    let class_a = extract_constraint_class_name(a);
    let class_b = extract_constraint_class_name(b);
    if class_a != class_b {
        return false;
    }
    // For single-param constraints, compare the var name.
    let var_a = extract_single_param_constraint_var(a);
    let var_b = extract_single_param_constraint_var(b);
    var_a == var_b
}

// ── Unknown warning emission ─────────────────────────────────────────────────

/// Emit a warning when Unknown meets a concrete non-TypeVar type in unification.
/// Warnings are collected in ctx's diagnostics list.
fn emit_unknown_warning(_ctx: &mut InferenceContext, _concrete: &TypeValue, _span: &Span) {
    // InferenceContext does not have a diagnostics field.
    // Warnings for Unknown-consistency are collected by callers.
    // This function is a placeholder for future diagnostic integration.
}

// ── process_deferred_equalities ───────────────────────────────────────────────

/// Process deferred equality constraints.
///
/// Deferred equalities arise when TypeStageApp applications cannot be reduced
/// immediately (e.g., because type arguments are not yet ground). This function
/// retries them until a fixed point is reached.
pub async fn process_deferred_equalities(
    deferred: &mut Vec<(TypeValue, TypeValue)>,
    ctx: &mut InferenceContext,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    span: Span,
) -> Result<(), TypeDiagnostic> {
    let max_iterations = 100;
    let mut iteration = 0;
    let mut progress = true;
    while progress && iteration < max_iterations {
        iteration += 1;
        progress = false;
        let current = std::mem::take(deferred);
        if current.is_empty() {
            break;
        }
        for (a, b) in current {
            let a_applied = ctx.apply_subst(&a);
            let b_applied = ctx.apply_subst(&b);
            // Attempt unification. If it fails, keep deferred for next iteration.
            let result = Box::pin(unify(
                &a_applied,
                &b_applied,
                ctx,
                constraints,
                span.clone(),
                0,
            ))
            .await;
            match result {
                Ok(()) => {
                    progress = true;
                }
                Err(_) => {
                    // Keep for next iteration — may become resolvable after other pairs unify.
                    deferred.push((a_applied, b_applied));
                }
            }
        }
    }
    // If any constraints remain after the fixpoint, they are permanently unresolvable.
    // Propagate the first error so type errors are visible to callers.
    if !deferred.is_empty() {
        let (a, b) = &deferred[0];
        let a_applied = ctx.apply_subst(a);
        let b_applied = ctx.apply_subst(b);
        return Box::pin(unify(&a_applied, &b_applied, ctx, constraints, span, 0)).await;
    }
    Ok(())
}

#[cfg(test)]
mod type_unify_tests;

#[cfg(test)]
mod type_unify_tests_new {
    use super::*;
    use crate::type_infer::{make_typevalue_repr, make_typevalue_unknown, InferenceContext};

    fn make_ctx() -> InferenceContext {
        InferenceContext::new()
    }

    fn make_span() -> Span {
        Span {
            file: std::sync::Arc::from("test"),
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 0,
            name: None,
        }
    }

    #[tokio::test]
    async fn test_unify_same_repr() {
        let mut ctx = make_ctx();
        let mut constraints = Vec::new();
        let int_tv = make_typevalue_repr(REPR_INT);
        let result = unify(&int_tv, &int_tv, &mut ctx, &mut constraints, make_span(), 0).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_unify_typevar_to_repr() {
        let mut ctx = make_ctx();
        let mut constraints = Vec::new();
        let var = ctx.fresh_typevar("t");
        let int_tv = make_typevalue_repr(REPR_INT);
        let span = make_span();
        let result = unify(&var, &int_tv, &mut ctx, &mut constraints, span, 0).await;
        assert!(result.is_ok());
        // The TypeVar should now be bound to Int
        if let Some(name) = typevalue_var_name(&var) {
            let bound = ctx.lookup(&name);
            assert!(bound.is_some());
        }
    }

    #[tokio::test]
    async fn test_unify_unknown_with_anything() {
        let mut ctx = make_ctx();
        let mut constraints = Vec::new();
        let unknown = make_typevalue_unknown();
        let int_tv = make_typevalue_repr(REPR_INT);
        let span = make_span();
        let result = unify(&unknown, &int_tv, &mut ctx, &mut constraints, span, 0).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_unify_different_reprs_fails() {
        let mut ctx = make_ctx();
        let mut constraints = Vec::new();
        let int_tv = make_typevalue_repr(REPR_INT);
        let str_tv = make_typevalue_repr(REPR_STRING);
        let result = unify(&int_tv, &str_tv, &mut ctx, &mut constraints, make_span(), 0).await;
        assert!(result.is_err());
    }
}
