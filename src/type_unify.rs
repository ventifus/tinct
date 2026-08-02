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
use crate::error::Diagnostic;
use crate::type_infer::{
    extract_rowtail_var_name, typevalue_ctor, typevalue_var_name, InferenceContext, TypeValue,
};
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
) -> Result<(), Diagnostic> {
    if p1_len != p2_len {
        return Err(Diagnostic::error(
            "type-error",
            format!(
                "arity mismatch: expected {} arguments, got {}",
                p1_len, p2_len
            ),
            span,
        ));
    }
    if is_variadic_1 != is_variadic_2 {
        return Err(Diagnostic::error(
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
pub(crate) fn lower_levels_check_occurs_tv(
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
        Some(TV_RECORD) => {
            // Check field types AND the tail RowVar for occurs check + level lowering.
            let fields = extract_record_fields(&ty)
                .expect("invariant: TypeValue.Record payload must be settled with fields dict");
            let tail = extract_record_tail(&ty)
                .expect("invariant: TypeValue.Record payload must be settled with tail field");

            // Check occurs in field types (standard traversal).
            let occurs_in_fields = fields
                .values()
                .any(|field_ty| lower_levels_check_occurs_tv(field_ty, var_name, cap_level, ctx));

            if occurs_in_fields {
                return true;
            }

            // Check the tail RowVar.
            if let Some(row_var_name) = extract_rowtail_var_name(&tail) {
                // RowVar occurs check: if the tail is the var we're checking, that's a cycle.
                let found = row_var_name == var_name;
                // Level lowering for the RowVar.
                ctx.lower_var_level(&row_var_name, cap_level);
                found
            } else if tv_ctor(&tail).as_deref() == Some(RT_UNIFORM) {
                // Tail is a Uniform tail — recursively check its value-type AND key-type for
                // TypeVar occurrences and perform level lowering on any TypeVars found inside.
                let value_occurs = if let Some(value_type) = extract_uniform_value_type(&tail) {
                    lower_levels_check_occurs_tv(&value_type, var_name, cap_level, ctx)
                } else {
                    false
                };

                if value_occurs {
                    return true;
                }

                // Also traverse key-type (Uniform tails can have key-type constraints).
                if let Some(key_type) = ctx.extract_uniform_key_type(&tail) {
                    lower_levels_check_occurs_tv(&key_type, var_name, cap_level, ctx)
                } else {
                    false
                }
            } else {
                // Tail is Closed (Empty dict) or unknown — no TypeVars present.
                false
            }
        }
        _ => {
            // For compound variants (Fn, App, Neg, Recursive, etc.),
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
/// On failure: returns `Diagnostic::error`.
pub async fn unify(
    a: &TypeValue,
    b: &TypeValue,
    ctx: &mut InferenceContext,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    span: Span,
    depth: usize,
) -> Result<(), Diagnostic> {
    if depth >= MAX_UNIFY_DEPTH {
        return Err(Diagnostic::error(
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
            return Err(Diagnostic::error(
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
            return Err(Diagnostic::error(
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
                Err(Diagnostic::error(
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
                Err(Diagnostic::error(
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
                Err(Diagnostic::error(
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
                Err(Diagnostic::error(
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
                Err(Diagnostic::error(
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
                Err(Diagnostic::error(
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
                Err(Diagnostic::error(
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
                Err(Diagnostic::error(
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
                Err(Diagnostic::error(
                    "type-error",
                    format!("cannot unify float literals {:?} and {:?}", va, vb),
                    span,
                ))
            }
        }

        // FloatLiteral promotes to Float (mirrors IntLiteral promotion).
        (Some(TV_FLOAT_LIT), Some(TV_REPR)) => {
            if extract_repr_string(&b).as_deref() == Some(REPR_FLOAT) {
                Ok(())
            } else {
                Err(Diagnostic::error(
                    "type-error",
                    "cannot unify FloatLiteral with non-Float repr type",
                    span,
                ))
            }
        }
        (Some(TV_REPR), Some(TV_FLOAT_LIT)) => {
            if extract_repr_string(&a).as_deref() == Some(REPR_FLOAT) {
                Ok(())
            } else {
                Err(Diagnostic::error(
                    "type-error",
                    "cannot unify non-Float repr type with FloatLiteral",
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

        // Record types: unidirectional constrain (a <: b) + tail unification.
        //
        // Structural record subtyping is NOT symmetric: a record with extra fields is a
        // subtype of one without them (width subtyping), but not vice versa. Calling
        // constrain(b, a) after constrain(a, b) produces false "missing field" errors
        // when a has more named fields than b — b cannot satisfy a's named-field
        // requirement even though b is structurally compatible as a supertype.
        //
        // The original bidirectional call
        //   constrain(a, b) + constrain(b, a)
        // fired "missing field 'ts'" for reduce calls in test-loader.llt where one union
        // branch returns an empty record and another returns {ts: Dict, rt: Dict}.
        // The reverse call constrain(empty, {ts, rt}) correctly rejects {} as a subtype
        // of {ts, rt}, but that check is wrong in unification — we are looking for a
        // common supertype (join/LUB), not enforcing equality.
        //
        // One direction (a <: b) is sufficient: all of b's named fields must appear in a
        // with unifiable types, and extra fields in a are allowed by width subtyping.
        // The tail is then unified symmetrically (unify_rowtails).
        (Some(TV_RECORD), Some(TV_RECORD)) => {
            let (ac, bc) = (a.clone(), b.clone());
            // Step 1: unidirectional field constraint — require all of b's named fields in a.
            Box::pin(constrain(&ac, &bc, ctx, constraints, span.clone())).await?;

            // Step 2: unify the row tails (RowTail.Uniform, RowTail.Var, RowTail.Closed).
            let a_tail = extract_record_tail(&ac)
                .expect("invariant: TypeValue.Record payload must be settled with tail field");
            let b_tail = extract_record_tail(&bc)
                .expect("invariant: TypeValue.Record payload must be settled with tail field");
            Box::pin(unify_rowtails(&a_tail, &b_tail, ctx, constraints, span)).await
        }

        // Union types: order-insensitive bipartite matching with backtracking (B-690).
        // Union([A, B]) ~ Union([B, A]) should succeed (order-insensitive).
        //
        // BACKTRACKING: Greedy first-match can reject valid unifications. Example:
        // Union([α, Int]) ~ Union([Int, Str]) where α is free. Greedy matches α~Int first
        // (binding α=Int), then Int~Str fails. But α=Str, Int~Int is valid.
        //
        // Fix: recursive backtracking search. For each unmatched member of members_a, try
        // all unmatched members_b. If a probe succeeds, recurse on remaining members. If
        // recursion fails, restore state and try the next b candidate.
        (Some(TV_UNION), Some(TV_UNION)) => {
            let members_a = extract_union_members(&a)
                .expect("invariant: TypeValue.Union payload must be settled with members field");
            let members_b = extract_union_members(&b)
                .expect("invariant: TypeValue.Union payload must be settled with members field");
            if members_a.len() != members_b.len() {
                // Different member counts: cannot unify
                return Err(Diagnostic::error(
                    "type-error",
                    format!(
                        "cannot unify union types with different numbers of members ({} vs {})",
                        members_a.len(),
                        members_b.len()
                    ),
                    span,
                ));
            }

            // Recursive backtracking helper: try to match members_a[a_start..] to unmatched members_b.
            // Returns Ok(()) if a valid assignment exists, Err otherwise.
            // On success, ctx and constraints are updated with the bindings.
            // On failure, ctx and constraints are restored to their state at entry.
            // Uses Box::pin for async recursion (codebase does not depend on async_recursion crate).
            fn try_match_from<'a>(
                a_start: usize,
                members_a: &'a [TypeValue],
                members_b: &'a [TypeValue],
                matched_b: &'a mut Vec<bool>,
                ctx: &'a mut InferenceContext,
                constraints: &'a mut Vec<Arc<crate::value::Value>>,
                span: Span,
                depth: usize,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), Diagnostic>> + Send + 'a>,
            > {
                Box::pin(async move {
                    // Base case: all members_a have been matched.
                    if a_start >= members_a.len() {
                        return Ok(());
                    }

                    let ma = &members_a[a_start];
                    // Try each unmatched member of members_b.
                    for b_idx in 0..members_b.len() {
                        if matched_b[b_idx] {
                            continue; // Already matched to a previous member_a
                        }

                        // Save state before probe.
                        let saved_ctx = ctx.clone();
                        let saved_constraints = constraints.clone();

                        // Probe: try to unify ma ~ members_b[b_idx].
                        let probe_result = Box::pin(unify(
                            ma,
                            &members_b[b_idx],
                            ctx,
                            constraints,
                            span.clone(),
                            depth + 1,
                        ))
                        .await;

                        if probe_result.is_ok() {
                            // Probe succeeded: mark b_idx as matched and recurse on remaining members_a.
                            matched_b[b_idx] = true;
                            let recurse_result = try_match_from(
                                a_start + 1,
                                members_a,
                                members_b,
                                matched_b,
                                ctx,
                                constraints,
                                span.clone(),
                                depth,
                            )
                            .await;

                            if recurse_result.is_ok() {
                                // Recursion succeeded: valid assignment found. Keep bindings and return.
                                return Ok(());
                            }

                            // Recursion failed: backtrack. Restore state and unmark b_idx.
                            *ctx = saved_ctx;
                            *constraints = saved_constraints;
                            matched_b[b_idx] = false;
                        } else {
                            // Probe failed: restore state and try next b_idx.
                            *ctx = saved_ctx;
                            *constraints = saved_constraints;
                        }
                    }

                    // No valid b_idx for members_a[a_start] — backtrack to caller.
                    Err(Diagnostic::error(
                        "type-error",
                        format!(
                            "cannot unify union types: no matching member found for type {}",
                            crate::eval::format_type_for_assert(ma)
                        ),
                        span,
                    ))
                })
            }

            let mut matched_b_indices = vec![false; members_b.len()];
            try_match_from(
                0,
                &members_a,
                &members_b,
                &mut matched_b_indices,
                ctx,
                constraints,
                span,
                depth,
            )
            .await
        }

        // App types: structural unification on the op and arg.
        //
        // TypeValue.App ~ TypeValue.App: behavior splits on injectivity of the op.
        //
        // Standard type constructors (Seq, Result, etc.) are injective by construction:
        // equal results imply equal arguments, so pairwise unification of op and arg is sound.
        //
        // Resolver-based type constructors (e.g. AddResult for Addable) may be non-injective:
        // AddResult(Int, Float) = Float = AddResult(Float, Float). Pairwise unification of args
        // would falsely conclude Int ~ Float. For non-injective ops, argument pairs are pushed to
        // ctx.resolver_deferred and retried once both sides reduce to concrete types.
        //
        // Injectivity is tracked in ctx.non_injective_resolvers, populated by
        // infer_class_decl_from_surface (typecheck.rs) when a ClassDecl with
        // resolver_injective = false is registered.
        (Some(TV_APP), Some(TV_APP)) => {
            let (op_a, arg_a) = extract_app_parts(&a).ok_or_else(|| {
                Diagnostic::error("type-error", "malformed TypeValue.App", span.clone())
            })?;
            let (op_b, arg_b) = extract_app_parts(&b).ok_or_else(|| {
                Diagnostic::error("type-error", "malformed TypeValue.App", span.clone())
            })?;

            // Check whether both ops are TV_OP with the same name and, if so, whether
            // the op name is associated with a non-injective resolver.
            let op_name_a = extract_op_name(&op_a);
            let op_name_b = extract_op_name(&op_b);
            let same_op_name = op_name_a.is_some() && op_name_a == op_name_b;

            if same_op_name {
                // Same op name: check injectivity before deciding how to unify args.
                let is_non_injective = op_name_a
                    .as_deref()
                    .map(|name| ctx.non_injective_resolvers.contains(name))
                    .expect("invariant: same_op_name guard ensures op_name_a is Some");

                if is_non_injective {
                    // Non-injective resolver: defer arg equality check until both sides are
                    // concrete (ground). Pairwise unification of args is unsound here because
                    // F(a) = F(b) does not imply a = b. The pair is retried by
                    // run_fd_improvement_fixpoint once both args have no free TypeVars.
                    ctx.resolver_deferred.push((arg_a, arg_b));
                    Ok(())
                } else {
                    // Injective op: pairwise structural unification — unify op and arg.
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
            } else {
                // UNIFY-TYCON-EXPAND (T-1112): different op names — try expanding both via tycon_env.
                // When both ops are registered type constructors with a single-parameter body,
                // substitute the argument into the body and unify the expanded types.
                // This allows structural aliases to unify: if Alias[T] = Seq[T], then
                // App(Alias, Int) ~ App(Seq, Int) succeeds by expanding both to their bodies.
                let expanded_a = expand_tycon_app(&op_name_a, &arg_a, ctx);
                let expanded_b = expand_tycon_app(&op_name_b, &arg_b, ctx);
                match (expanded_a, expanded_b) {
                    (Some(exp_a), Some(exp_b)) => {
                        // Both expanded: unify the expanded bodies.
                        Box::pin(unify(&exp_a, &exp_b, ctx, constraints, span, depth + 1)).await
                    }
                    _ => {
                        // Cannot expand one or both: fall through to structural unification,
                        // which will fail at the op level (different names).
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
            }
        }

        // Recursive types: open with fresh TypeVar, bidirectional constrain.
        (Some(TV_RECURSIVE), Some(TV_RECURSIVE)) => {
            let body_a = extract_recursive_body(&a).ok_or_else(|| {
                Diagnostic::error("type-error", "malformed TypeValue.Recursive", span.clone())
            })?;
            let body_b = extract_recursive_body(&b).ok_or_else(|| {
                Diagnostic::error("type-error", "malformed TypeValue.Recursive", span.clone())
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
                Diagnostic::error("type-error", "malformed TypeValue.Recursive", span.clone())
            })?;
            let fresh = ctx.fresh_typevar("rec");
            let opened_a = substitute_rec_ref(&body_a, 0, &fresh);
            Box::pin(constrain(&opened_a, &b, ctx, constraints, span.clone())).await?;
            Box::pin(constrain(&b, &opened_a, ctx, constraints, span)).await
        }

        (_, Some(TV_RECURSIVE)) => {
            let body_b = extract_recursive_body(&b).ok_or_else(|| {
                Diagnostic::error("type-error", "malformed TypeValue.Recursive", span.clone())
            })?;
            let fresh = ctx.fresh_typevar("rec");
            let opened_b = substitute_rec_ref(&body_b, 0, &fresh);
            Box::pin(constrain(&a, &opened_b, ctx, constraints, span.clone())).await?;
            Box::pin(constrain(&opened_b, &a, ctx, constraints, span)).await
        }

        // Negation: contravariant (bidirectional swap).
        (Some(TV_NEG), Some(TV_NEG)) => {
            let inner_a = extract_neg_inner(&a).ok_or_else(|| {
                Diagnostic::error("type-error", "malformed TypeValue.Neg", span.clone())
            })?;
            let inner_b = extract_neg_inner(&b).ok_or_else(|| {
                Diagnostic::error("type-error", "malformed TypeValue.Neg", span.clone())
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
            let members = extract_union_members(&b).ok_or_else(|| {
                Diagnostic::error(
                    "type-error",
                    "TypeValue.Union payload is unsettled or malformed",
                    span.clone(),
                )
            })?;
            if members.is_empty() {
                return Err(Diagnostic::error(
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
            Err(Diagnostic::error(
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
            let members = extract_union_members(&a).ok_or_else(|| {
                Diagnostic::error(
                    "type-error",
                    "TypeValue.Union payload is unsettled or malformed",
                    span.clone(),
                )
            })?;
            if members.is_empty() {
                return Err(Diagnostic::error(
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
            Err(Diagnostic::error(
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
            let members = extract_intersection_members(&a).ok_or_else(|| {
                Diagnostic::error(
                    "type-error",
                    "TypeValue.Intersect payload is unsettled or malformed",
                    span.clone(),
                )
            })?;
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
            Err(Diagnostic::error(
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
        (Some(ca), Some(cb)) if ca != cb => Err(Diagnostic::error(
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

// ── unify_rowtails ────────────────────────────────────────────────────────────

/// Occurs check for RowVar binding: returns true if the RowVar named `name` occurs
/// free in `tail`. Prevents constructing infinite row types.
///
/// Direct self-reference: ρ occurs in RowTail.Var("ρ").
///
/// For RowTail.Uniform, the value-type is itself a TypeValue that may contain Records
/// with RowVar tails — so we recursively check into it. This covers the case where
/// binding ρ = RowTail.Uniform { value-type: Record { tail: ρ } } would construct
/// an infinite row type via nesting rather than direct self-reference.
pub(crate) fn rowvar_occurs_in_tail(name: &str, tail: &TypeValue) -> bool {
    // Direct self-reference: ρ appears as RowTail.Var("ρ").
    if extract_rowtail_var_name(tail).as_deref() == Some(name) {
        return true;
    }
    // Indirect reference: ρ occurs inside a Uniform value-type or key-type's nested Record tail.
    if tv_ctor(tail).as_deref() == Some(RT_UNIFORM) {
        // Check value-type field.
        if let Some(value_type) = extract_uniform_value_type(tail) {
            if tv_ctor(&value_type).as_deref() == Some(TV_RECORD) {
                if let Some(nested_tail) = extract_record_tail(&value_type) {
                    if rowvar_occurs_in_tail(name, &nested_tail) {
                        return true;
                    }
                }
            }
        }
        // Also check key-type field (key-type could be a Record with RowVar tail).
        if let Some(key_type) = extract_uniform_key_type(tail) {
            if tv_ctor(&key_type).as_deref() == Some(TV_RECORD) {
                if let Some(nested_tail) = extract_record_tail(&key_type) {
                    if rowvar_occurs_in_tail(name, &nested_tail) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Unify two RowTail values.
///
/// - RowTail.Closed ~ RowTail.Closed: succeed
/// - RowTail.Uniform ~ RowTail.Uniform: unify their value-types and key-types (when both are present)
/// - RowTail.Var ~ concrete tail: bind the RowVar to the concrete tail
/// - RowTail.Closed ~ RowTail.Uniform: fail (closed ≠ open)
pub(crate) async fn unify_rowtails(
    a: &TypeValue,
    b: &TypeValue,
    ctx: &mut InferenceContext,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    span: Span,
) -> Result<(), Diagnostic> {
    // Apply substitution to both tails.
    let a = ctx.apply_subst(a);
    let b = ctx.apply_subst(b);

    let a_ctor = tv_ctor(&a);
    let b_ctor = tv_ctor(&b);

    match (a_ctor, b_ctor) {
        // Closed ~ Closed: succeed.
        (Some(RT_CLOSED), Some(RT_CLOSED)) => Ok(()),

        // Empty dict [] ~ Empty dict []: succeed (both closed).
        (None, None) => {
            // Both are empty dicts (treated as closed tails).
            Ok(())
        }

        // Closed ~ Empty dict: succeed (both closed).
        (Some(RT_CLOSED), None) | (None, Some(RT_CLOSED)) => Ok(()),

        // Uniform ~ Uniform: unify their value-types AND key-types.
        (Some(RT_UNIFORM), Some(RT_UNIFORM)) => {
            let a_value_type = extract_uniform_value_type(&a)
                .expect("invariant: RowTail.Uniform payload must have value-type field");
            let b_value_type = extract_uniform_value_type(&b)
                .expect("invariant: RowTail.Uniform payload must have value-type field");

            // Unify value-types.
            Box::pin(unify(
                &a_value_type,
                &b_value_type,
                ctx,
                constraints,
                span.clone(),
                0,
            ))
            .await?;

            // Also unify key-types if both are present.
            let a_key = extract_uniform_key_type(&a);
            let b_key = extract_uniform_key_type(&b);
            match (a_key, b_key) {
                (Some(ak), Some(bk)) => {
                    Box::pin(unify(&ak, &bk, ctx, constraints, span, 0)).await?;
                }
                // B-711: One or neither has key-type — the unconstrained side accepts any
                // key-type, so no unification needed. The unified tail preserves whichever
                // key-type exists (if any). This is correct: an unconstrained key domain
                // is compatible with any specific key domain.
                _ => {}
            }
            Ok(())
        }

        // RowVar ~ RowVar: bind the higher-level RowVar to the lower-level one.
        // Analogous to U-VAR-VAR for TypeVars (level comparison, keep the lower-level one).
        (Some(RT_VAR), Some(RT_VAR)) => {
            let name_a = extract_rowtail_var_name(&a);
            let name_b = extract_rowtail_var_name(&b);
            match (name_a, name_b) {
                (Some(na), Some(nb)) => {
                    if na == nb {
                        return Ok(()); // Same RowVar — nothing to do.
                    }
                    let level_a = ctx.get_level(&na);
                    let level_b = ctx.get_level(&nb);
                    // Bind the higher-level RowVar to the lower-level one.
                    // When levels are equal, bind a → b (arbitrary but consistent).
                    if level_a >= level_b {
                        ctx.lower_var_level(&nb, level_a);
                        ctx.bind(na, b.clone())?;
                    } else {
                        ctx.lower_var_level(&na, level_b);
                        ctx.bind(nb, a.clone())?;
                    }
                    Ok(())
                }
                // Malformed RowTail.Var — conservative accept.
                _ => Ok(()),
            }
        }

        // RowVar ~ concrete tail: bind the RowVar to the concrete tail.
        // Analogous to U-VAR-LEVEL for TypeVars (bind variable to concrete type).
        // Occurs check: binding ρ = tail is rejected if ρ appears free in tail (infinite row type).
        (Some(RT_VAR), _) => {
            match extract_rowtail_var_name(&a) {
                Some(name) => {
                    if rowvar_occurs_in_tail(&name, &b) {
                        return Err(Diagnostic::error(
                            "type-error",
                            format!(
                                "infinite row type: row variable '{}' occurs in its own binding",
                                name
                            ),
                            span,
                        ));
                    }
                    ctx.bind(name, b.clone())?;
                    Ok(())
                }
                // Malformed RowTail.Var — conservative accept.
                None => Ok(()),
            }
        }
        (_, Some(RT_VAR)) => {
            match extract_rowtail_var_name(&b) {
                Some(name) => {
                    if rowvar_occurs_in_tail(&name, &a) {
                        return Err(Diagnostic::error(
                            "type-error",
                            format!(
                                "infinite row type: row variable '{}' occurs in its own binding",
                                name
                            ),
                            span,
                        ));
                    }
                    ctx.bind(name, a.clone())?;
                    Ok(())
                }
                // Malformed RowTail.Var — conservative accept.
                None => Ok(()),
            }
        }

        // Closed ~ Uniform: fail (incompatible tails).
        (Some(RT_CLOSED), Some(RT_UNIFORM)) | (Some(RT_UNIFORM), Some(RT_CLOSED)) => {
            Err(Diagnostic::error(
                "type-error",
                "cannot unify closed record with open uniform record",
                span,
            ))
        }

        // Empty dict ~ Uniform: fail (closed ≠ open).
        (None, Some(RT_UNIFORM)) | (Some(RT_UNIFORM), None) => Err(Diagnostic::error(
            "type-error",
            "cannot unify closed record (empty tail) with open uniform record",
            span,
        )),

        // Unknown ctor combinations: conservative accept.
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
) -> Result<(), Diagnostic> {
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

    // C-LB-Union (B-686): constrain(sub, Union([..., TypeVar(α), ...])) accumulates lower
    // bounds instead of equality binding.
    //
    // When sup is a Union containing TypeVars, the constraint "sub <: Union([..., α, ...])"
    // is satisfied if sub matches ANY member. Correct semantics:
    // 1. Try constrain(sub, each non-TypeVar member) — if any succeeds, constraint satisfied
    // 2. If all fail, accumulate sub as a lower bound for the first TypeVar member
    //
    // This prevents falling through to unify(), which would bind α=sub via U-VAR-LEVEL
    // (equality), violating directionality. The Union is satisfied if sub matches any member;
    // TypeVars in the union are flexible — they accumulate bounds, not equality bindings.
    if matches!(tv_ctor(&sup), Some(TV_UNION)) {
        let members = extract_union_members(&sup).ok_or_else(|| {
            Diagnostic::error(
                "type-error",
                "TypeValue.Union payload is unsettled or malformed",
                span.clone(),
            )
        })?;

        // Partition members: non-TypeVars first, TypeVars last.
        let (concrete_members, typevar_members): (Vec<_>, Vec<_>) = members
            .iter()
            .partition(|m| typevalue_var_name(m).is_none());

        // Try concrete members first via constrain (non-destructive probes).
        for member in &concrete_members {
            let mut probe_ctx = ctx.clone();
            let mut probe_constraints = constraints.clone();
            let result = Box::pin(constrain(
                &sub,
                member,
                &mut probe_ctx,
                &mut probe_constraints,
                span.clone(),
            ))
            .await;
            if result.is_ok() {
                // Constraint satisfied by this concrete member — no TypeVar binding needed.
                *ctx = probe_ctx;
                *constraints = probe_constraints;
                return Ok(());
            }
        }

        // All concrete members failed. If there are TypeVars in the union, accumulate
        // sub as a lower bound for the first TypeVar member.
        if let Some(first_typevar) = typevar_members.first() {
            if let Some(alpha_name) = typevalue_var_name(first_typevar) {
                ctx.add_lower_bound(&alpha_name, sub.clone());
                return Ok(());
            }
        }

        // No concrete member matched and no TypeVars available — constraint fails.
        // Fall through to unify for the error message.
    }

    // C-UB (upper bound / narrowing): constrain(α, sup) where α is a free TypeVar and
    // sup is a concrete type.
    //
    // Instead of eagerly binding α = sup, accumulate sup as an upper bound. This
    // allows multiple upper bounds to be recorded before resolution. When exactly one upper
    // bound exists, binds α eagerly. When multiple upper bounds exist, they are accumulated;
    // the first upper bound triggers binding with lb verification (B-705).
    //
    // Invariant: `sub` has already been fully resolved through apply_subst at the top of
    // this function. If sub is a TypeVar here, it is guaranteed to be free (not in subst).
    if let Some(alpha_name) = typevalue_var_name(&sub) {
        // Check if α was bound since the top-level apply_subst by re-resolving it.
        let alpha_tv = crate::type_infer::make_typevar_value(&alpha_name);
        let resolved = ctx.apply_subst(&alpha_tv);
        if !typevalue_shallow_eq(&alpha_tv, &resolved) {
            // α is already bound — re-check the constraint with the bound value.
            return Box::pin(constrain(&resolved, &sup, ctx, constraints, span)).await;
        }

        // Add sup as an upper bound (T-2096). If this is the first and only upper bound,
        // bind eagerly after verifying all accumulated lower bounds. When multiple upper
        // bounds exist, they accumulate in ctx.upper_bounds for resolution at the binding site.
        ctx.add_upper_bound(&alpha_name, sup.clone());
        let all_upper_bounds = ctx
            .upper_bounds
            .get(&alpha_name)
            .map(|v| v.len())
            .expect("invariant: add_upper_bound just inserted alpha_name; entry must be Some");

        if all_upper_bounds == 1 {
            // Verify all accumulated lower bounds are subtypes of the upper bound
            // before binding. This ensures that binding α = sup doesn't violate earlier
            // constraints like constrain(String, α).
            let lower_bounds = ctx.take_lower_bounds(&alpha_name);
            for lb in &lower_bounds {
                // Verify lb <: sup holds.
                if !crate::bas::is_subtype_bas(lb, &sup, ctx) {
                    return Err(Diagnostic::error(
                        "type-error",
                        format!(
                            "accumulated lower bound is not a subtype of upper bound (incompatible constraints on type variable)"
                        ),
                        span.clone(),
                    ));
                }
            }

            // Single upper bound — bind eagerly (same as old behavior). Discard upper bounds
            // to prevent stale data accumulation (they become dead once α is bound).
            drop(ctx.take_upper_bounds(&alpha_name));
            ctx.bind(alpha_name, sup.clone())?;
        }
        // When `all_upper_bounds > 1`, the TypeVar was already bound by the first upper bound
        // call above. This branch is effectively unreachable: apply_subst at the top of
        // constrain() resolves bound TypeVars before reaching C-UB.

        return Ok(());
    }

    // Reject Top in sub position after TypeVar cases are handled above.
    // constrain(Top, TypeVar) is handled by C-LB above (TypeVar in sup).
    // constrain(Top, concrete) is rejected here — Top is not a subtype of any concrete type.
    if is_top(&sub) {
        return Err(Diagnostic::error(
            "type-error",
            format!("Top (⊤) is not a subtype of a specific type"),
            span.clone(),
        ));
    }

    // Reject Never in sup position after TypeVar cases are handled above.
    // constrain(TypeVar, Never) is handled by C-UB above (TypeVar in sub).
    // constrain(concrete, Never) is rejected here — no specific type is a subtype of Never.
    if is_never(&sup) {
        return Err(Diagnostic::error(
            "type-error",
            format!("a specific type is not a subtype of Never (⊥)"),
            span.clone(),
        ));
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
) -> Result<(), Diagnostic> {
    let (params_sub, ret_sub) = extract_fn_parts(sub).ok_or_else(|| {
        Diagnostic::error("type-error", "malformed TypeValue.Fn (sub)", span.clone())
    })?;
    let (params_sup, ret_sup) = extract_fn_parts(sup).ok_or_else(|| {
        Diagnostic::error("type-error", "malformed TypeValue.Fn (sup)", span.clone())
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
/// Handles RowTail.Uniform: when sup has a uniform tail, extra fields in sub must be subtypes of the uniform type.
async fn constrain_record(
    sub: &TypeValue,
    sup: &TypeValue,
    ctx: &mut InferenceContext,
    constraints: &mut Vec<Arc<crate::value::Value>>,
    span: Span,
) -> Result<(), Diagnostic> {
    let sup_fields = extract_record_fields(sup)
        .expect("invariant: TypeValue.Record payload must be settled with fields dict");
    let sub_fields = extract_record_fields(sub)
        .expect("invariant: TypeValue.Record payload must be settled with fields dict");
    let sup_tail = extract_record_tail(sup)
        .expect("invariant: TypeValue.Record payload must be settled with tail field");
    let sub_tail = extract_record_tail(sub)
        .expect("invariant: TypeValue.Record payload must be settled with tail field");

    // Step 1: constrain all named fields (depth subtyping).
    for (k, sup_ty) in &sup_fields {
        if let Some(sub_ty) = sub_fields.get(k) {
            Box::pin(constrain(sub_ty, sup_ty, ctx, constraints, span.clone())).await?;
        } else {
            return Err(Diagnostic::error(
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

    // Step 2: handle row tail constraints (width subtyping).
    let sup_tail_ctor = tv_ctor(&sup_tail);
    match sup_tail_ctor {
        Some(RT_CLOSED) => {
            // sup tail is closed: structural width subtyping — sub may have extra fields.
            // All sup fields were checked in Step 1 (depth subtyping). Extra fields in sub
            // are allowed (a record with more fields is MORE specific, thus a subtype).
            Ok(())
        }
        Some(RT_UNIFORM) => {
            // sup tail is uniform: extra fields in sub must be subtypes of the uniform value-type.
            let sup_value_type = extract_uniform_value_type(&sup_tail)
                .expect("invariant: RowTail.Uniform payload must have value-type field");

            // Collect extra fields from sub (not in sup's named fields).
            let extra_fields: Vec<(&String, &TypeValue)> = sub_fields
                .iter()
                .filter(|(k, _v)| !sup_fields.contains_key(*k))
                .collect();

            // Constrain each extra field to be a subtype of sup_value_type.
            for (field_name, sub_field_ty) in extra_fields {
                Box::pin(constrain(
                    sub_field_ty,
                    &sup_value_type,
                    ctx,
                    constraints,
                    span.clone(),
                ))
                .await
                .map_err(|e| {
                    e.with_note(format!(
                        "field '{}' does not satisfy uniform tail constraint",
                        field_name
                    ))
                })?;
            }

            // If sub also has a uniform tail, constrain sub_value_type <: sup_value_type.
            if matches!(tv_ctor(&sub_tail), Some(RT_UNIFORM)) {
                let sub_value_type = extract_uniform_value_type(&sub_tail)
                    .expect("invariant: RowTail.Uniform payload must have value-type field");
                Box::pin(constrain(
                    &sub_value_type,
                    &sup_value_type,
                    ctx,
                    constraints,
                    span.clone(),
                ))
                .await?;

                // B-710: Unify key-types when both tails are Uniform.
                // Key-types are invariant: a dict with Int keys cannot be used where String keys
                // are expected, nor vice versa. Using constrain (covariant) here would allow
                // IntKeyMap <: StrKeyMap which is incorrect — key-types must be equal.
                let sub_key = extract_uniform_key_type(&sub_tail);
                let sup_key = extract_uniform_key_type(&sup_tail);
                match (sub_key, sup_key) {
                    (Some(sk), Some(spk)) => {
                        Box::pin(unify(&sk, &spk, ctx, constraints, span.clone(), 0)).await?;
                    }
                    _ => {} // One or neither has key-type — compatible
                }
            }

            Ok(())
        }
        Some(RT_VAR) => {
            // sup tail is a RowVar ρ. Accumulate sub_tail as a lower bound for ρ.
            // This preserves directionality: "sub_row <: {... ρ}" means ρ must accommodate
            // at least the sub tail. When exactly one lower bound exists, binds ρ eagerly.
            // When multiple lower bounds exist, they accumulate in ctx.row_lower_bounds.
            match extract_rowtail_var_name(&sup_tail) {
                Some(name) => {
                    ctx.add_row_lower_bound(&name, sub_tail.clone());
                    let all_row_bounds = ctx.row_lower_bounds.get(&name).map(|v| v.len()).expect(
                        "invariant: add_row_lower_bound just inserted name; entry must be Some",
                    );

                    if all_row_bounds == 1 {
                        // Single lower bound — bind eagerly.
                        drop(ctx.take_row_lower_bounds(&name));
                        ctx.bind(name, sub_tail.clone())?;
                    }
                    // Multiple lower bounds: accumulated in ctx.row_lower_bounds.

                    Ok(())
                }
                None => panic!(
                    "invariant violation: RowTail.Var payload has no name field — \
                     all RowVar values must be constructed via make_rowtail_var"
                ),
            }
        }
        _ => {
            // sup tail is closed (empty dict [] or RT_CLOSED or unknown variant).
            // Structural width subtyping: sub may have extra fields — a record with more
            // fields is MORE specific, thus a subtype. All sup fields were already checked
            // in Step 1 (depth subtyping). Extra fields in sub are allowed.
            Ok(())
        }
    }
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
                        return b_ctor.as_ref() == BOOL_TRUE;
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

/// Extract the tail from a TypeValue.Record { tail: RowTail }.
/// Returns the tail as a TypeValue (RowTail.Closed, RowTail.Uniform, RowTail.Var).
/// Returns None if the payload is unsettled or malformed.
fn extract_record_tail(tv: &TypeValue) -> Option<TypeValue> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_RECORD => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let tail_key = crate::value::HashableValue::Str(Arc::from(FIELD_TAIL));
                let tail_thunk = entries.get(&tail_key)?;
                match tail_thunk.peek_result()? {
                    Ok(tv) => Some(Arc::new(tv.clone())),
                    Err(e) => panic!(
                        "invariant violation: TypeValue.Record tail thunk is in error state: {e:?}"
                    ),
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract the value-type from a RowTail.Uniform { value-type: TypeValue }.
/// Returns None if the tail is not a RowTail.Uniform or the payload is unsettled.
fn extract_uniform_value_type(tail: &TypeValue) -> Option<TypeValue> {
    match tail.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == RT_UNIFORM => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let value_type_key =
                    crate::value::HashableValue::Str(Arc::from(RT_FIELD_VALUE_TYPE));
                let value_type_thunk = entries.get(&value_type_key)?;
                match value_type_thunk.peek_result()? {
                    Ok(vt) => Some(Arc::new(vt.clone())),
                    Err(e) => panic!(
                        "invariant violation: TypeValue.Uniform value-type thunk is in error state: {e:?}"
                    ),
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract the key-type from a RowTail.Uniform { key-type: TypeValue? }.
/// Returns None if the tail is not a RowTail.Uniform, the payload is unsettled, or key-type is absent.
fn extract_uniform_key_type(tail: &TypeValue) -> Option<TypeValue> {
    match tail.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == RT_UNIFORM => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let key_type_key = crate::value::HashableValue::Str(Arc::from(RT_FIELD_KEY_TYPE));
                let key_type_thunk = entries.get(&key_type_key)?;
                match key_type_thunk.peek_result()? {
                    Ok(kt) => Some(Arc::new(kt.clone())),
                    Err(e) => panic!(
                        "invariant violation: TypeValue.Uniform key-type thunk is in error state: {e:?}"
                    ),
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

// ── UNIFY-TYCON-EXPAND: type constructor body expansion ───────────────────────

/// UNIFY-TYCON-EXPAND (T-1112): attempt to expand a type constructor application.
///
/// Given `App(Op(name), arg)`, look up `name` in `ctx.tycon_env`. If found and the
/// TyConDef has exactly one parameter, create a temporary substitution of the parameter
/// TypeVar → arg and apply it to the TyConDef body. Returns the expanded body TypeValue,
/// or None if expansion is not possible (op not in tycon_env, wrong arity, malformed body).
///
/// This enables structural alias unification: if `Alias[T] = Seq[T]` is in tycon_env,
/// then `App(Alias, Int) ~ App(Seq, Int)` can succeed by expanding both to their bodies
/// and unifying `Int` (the Seq body parametrically) with the alias expansion.
///
/// Limitation: only single-parameter type constructors are expanded. Multi-parameter
/// constructors require multi-argument App chains (App(App(F, a), b)) — those are
/// handled by the outer structural arms after partial expansion.
fn expand_tycon_app(
    op_name: &Option<String>,
    arg: &TypeValue,
    ctx: &InferenceContext,
) -> Option<TypeValue> {
    let name = op_name.as_deref()?;
    let tycon_def = ctx.tycon_env.get(name)?;
    // Only expand single-parameter type constructors.
    if tycon_def.params.len() != 1 {
        return None;
    }
    let param_name = &tycon_def.params[0];
    let body: TypeValue = Arc::new(tycon_def.body.as_ref().clone());
    // Build a renaming map: param_name → arg.
    // Apply it to the body via apply_typevalue_renaming, which walks compound structures.
    let mut renaming = std::collections::HashMap::new();
    renaming.insert(param_name.clone(), Arc::clone(arg));
    Some(crate::types::type_env::apply_typevalue_renaming(
        &body, &renaming,
    ))
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
) -> Result<(), Diagnostic> {
    let max_iterations = 100;
    let mut iteration = 0;
    let mut progress = true;
    // Track the most recent error for deferred pairs, so it can be propagated
    // if the pair remains permanently unresolvable after the fixpoint.
    let mut last_error: Option<Diagnostic> = None;
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
                    last_error = None;
                }
                Err(e) => {
                    // Temporarily unresolvable — may become solvable once other pairs unify.
                    // Record the error so it can be propagated if this pair never resolves.
                    last_error = Some(e);
                    deferred.push((a_applied, b_applied));
                }
            }
        }
    }
    // If any constraints remain after the fixpoint, they are permanently unresolvable.
    // Propagate the accumulated error (or re-derive it) so type errors are visible to callers.
    if !deferred.is_empty() {
        if let Some(e) = last_error {
            return Err(e);
        }
        // Fallback: last_error is None but deferred is non-empty.
        // Re-derive a concrete error from the first remaining pair, then unconditionally
        // return Err — even if that pair now unifies (it may have become satisfiable while
        // other deferred pairs remain stuck). This ensures deferred is never silently dropped.
        let (a, b) = &deferred[0];
        let a_applied = ctx.apply_subst(a);
        let b_applied = ctx.apply_subst(b);
        return Box::pin(unify(
            &a_applied,
            &b_applied,
            ctx,
            constraints,
            span.clone(),
            0,
        ))
        .await
        .and_then(|()| {
            // First pair became satisfiable, but others remain — report generic error.
            Err(Diagnostic::error(
                "type-error",
                "deferred type constraints could not be fully resolved",
                span,
            ))
        });
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

    // B-710: Key-types in Uniform tails are invariant — constraining sub <: sup must fail
    // in BOTH directions when key-types differ. A covariant check would only fail one way;
    // this test pins the invariant (unify) behavior.
    #[tokio::test]
    async fn test_b710_key_type_invariant_both_directions_fail() {
        use crate::type_infer::{
            make_rowtail_uniform_with_key_type, make_typevalue_record, make_typevalue_top,
        };

        // Build Record { ...Int-keyed String-valued } (Uniform tail: key=Int, value=String)
        let int_tv = make_typevalue_repr(REPR_INT);
        let str_tv = make_typevalue_repr(REPR_STRING);
        let top_tv = make_typevalue_top();

        let int_key_tail = make_rowtail_uniform_with_key_type(top_tv.clone(), Some(int_tv.clone()));
        let str_key_tail = make_rowtail_uniform_with_key_type(top_tv.clone(), Some(str_tv.clone()));

        let int_key_map =
            make_typevalue_record(indexmap::IndexMap::new(), Some(int_key_tail.clone()));
        let str_key_map =
            make_typevalue_record(indexmap::IndexMap::new(), Some(str_key_tail.clone()));

        // Direction 1: int-key-map <: str-key-map must fail (Int ≠ String key-type)
        {
            let mut ctx = make_ctx();
            let mut constraints = Vec::new();
            let result = constrain(
                &int_key_map,
                &str_key_map,
                &mut ctx,
                &mut constraints,
                make_span(),
            )
            .await;
            assert!(
                result.is_err(),
                "B-710: constrain(int-key-map, str-key-map) must fail — key-types are invariant"
            );
        }

        // Direction 2: str-key-map <: int-key-map must also fail
        {
            let mut ctx = make_ctx();
            let mut constraints = Vec::new();
            let result = constrain(
                &str_key_map,
                &int_key_map,
                &mut ctx,
                &mut constraints,
                make_span(),
            )
            .await;
            assert!(
                result.is_err(),
                "B-710: constrain(str-key-map, int-key-map) must also fail — key-types are invariant, not covariant"
            );
        }
    }
}
