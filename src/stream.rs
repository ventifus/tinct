//! Stream output builtins — tinct SCN serialization for the streaming pipeline.
//!
//! `builtin_to_tinct`: serialize any materialized value to its Self-Contained Normal
//! Form (SCN) tinct source representation, suitable for `---` stream boundaries and
//! codec round-trips.
//!
//! The implementation delegates to `Value::to_tinct` (defined in `src/surface_fmt.rs`),
//! which formats each value variant to canonical tinct syntax.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::builtins::{expect_one_arg, ok_val};
use crate::error::{EvalError, EvalResult};
use crate::value::{string_val, BuiltinArgs, Thunk};

/// `builtin-to-tinct`: serialize a value to its SCN tinct source representation.
///
/// Takes one positional argument (pre-materialized via `force_count = 1`).
/// Returns a `String` containing the canonical tinct source text for the value.
///
/// Errors if the value has no tinct representation (capabilities, tasks, channels, etc.)
/// or if a required structure is not fully materialized (e.g., dict values not forced).
///
/// See `Value::to_tinct` in `src/surface_fmt.rs` for the full serialization logic.
pub(crate) fn builtin_to_tinct(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = expect_one_arg(
            "builtin-to-tinct",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let tinct_str = val
            .to_tinct(Some(&ctx))
            .map_err(|e| EvalError::user_error(format!("to-tinct: {}", e), call_span.clone()))?;
        ok_val(string_val(&tinct_str), call_span)
    })
}
