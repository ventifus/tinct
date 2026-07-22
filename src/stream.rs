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
use crate::eval_materialize::force_dict_tree;
use crate::value::{string_val, BuiltinArgs, Thunk};

/// `builtin-to-tinct`: serialize a value to its SCN tinct source representation.
///
/// Takes one positional argument (WHNF-materialized via `force_count = 1`).
/// Returns a `String` containing the canonical tinct source text for the value.
///
/// Deep-forces all nested structures (dict entries, collection elements, variant payloads)
/// before serialization to ensure `to_tinct` can access all materialized values.
///
/// Errors if the value has no tinct representation (capabilities, tasks, channels, etc.).
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
            ..
        } = ctx_arg;
        let val = expect_one_arg(
            "builtin-to-tinct",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        // Deep-force all nested structures before serialization.
        // The WHNF-materialized value may contain unevaluated thunks in dict entries,
        // collection elements, or variant payloads. force_dict_tree recursively materializes
        // all nested values so that to_tinct's try_get_value calls succeed.
        let deep_val = force_dict_tree(&val, &ctx).await?;

        let tinct_str = deep_val
            .to_tinct(Some(&ctx))
            .map_err(|e| EvalError::user_error(format!("to-tinct: {}", e), call_span.clone()))?;
        ok_val(string_val(&tinct_str), call_span)
    })
}
