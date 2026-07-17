//! Call type checking helpers — widening and boundary guard predicates.
//!
//! `check_call_args` was deleted in the CEK machine migration (T-1644);
//! its logic is now in `typecheck_cek::apply_call_args_poly`.
//!
//! `widen_literal_types` and `is_concrete_type` are retained here because they
//! are called from both `typecheck_cek.rs` and `type_unify.rs`.

use crate::types::Type;

/// Widen a literal type to its base type for argument unification.
///
/// `IntLiteral(n)` → `Int`, `StringLiteral(s)` → `Str`. All other types
/// are returned unchanged. Used before unification so that a literal
/// argument matches a concrete-type parameter.
pub(crate) fn widen_literal_types(ty: Type) -> Type {
    match ty {
        Type::IntLiteral(_) => Type::Int,
        Type::StringLiteral(_) => Type::Str,
        other => other,
    }
}

/// Return true when `ty` is a concrete type that should trigger a type_guard
/// boundary for gradual typing. Unknown, Any, and unresolved TypeVars are
/// not concrete — they admit any value and do not constrain the callee.
pub(crate) fn is_concrete_type(ty: &Type) -> bool {
    !matches!(ty, Type::Unknown | Type::Any | Type::TypeVar(_, _))
}
