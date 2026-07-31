//! Call type checking helpers — widening and boundary guard predicates.
//!
//! `check_call_args` was deleted in the CEK machine migration (T-1644);
//! its logic is now in `typecheck_cek::apply_call_args_poly`.
//!
//! `widen_literal_types` and `is_concrete_type` are retained here because they
//! are called from both `typecheck_cek.rs` and `type_unify.rs`.

use crate::type_tags::*;
use crate::value::Value;
use std::sync::Arc;

/// Widen a literal TypeValue to its base TypeValue for argument unification.
///
/// TypeValue.IntLit → TypeValue.Repr{repr:"Value::Int"}
/// TypeValue.FloatLit → TypeValue.Repr{repr:"Value::Float"}
/// TypeValue.StrLit → TypeValue.Repr{repr:"Value::String"}
/// All other TypeValues are returned unchanged.
pub(crate) fn widen_literal_types(tv: Arc<Value>) -> Arc<Value> {
    match tv.as_ref() {
        Value::Variant { ctor, .. } => match ctor.as_ref() {
            TV_INT_LIT => crate::type_infer::make_typevalue_repr(REPR_INT),
            TV_FLOAT_LIT => crate::type_infer::make_typevalue_repr(REPR_FLOAT),
            TV_STR_LIT => crate::type_infer::make_typevalue_repr(REPR_STRING),
            _ => tv,
        },
        _ => tv,
    }
}

/// Return true when `tv` is a concrete TypeValue that should trigger a type_guard
/// boundary for gradual typing. Unknown, Top, and unresolved TypeVars are
/// not concrete — they admit any value and do not constrain the callee.
pub(crate) fn is_concrete_type(tv: &Arc<Value>) -> bool {
    match tv.as_ref() {
        Value::Variant { ctor, .. } => {
            let c = ctor.as_ref();
            c != TV_UNKNOWN && c != TV_TOP && c != TV_VAR
        }
        // Bootstrap sentinel (empty dict = Unknown) → not concrete
        Value::Dict { entries, .. } if entries.is_empty() => false,
        _ => true,
    }
}
