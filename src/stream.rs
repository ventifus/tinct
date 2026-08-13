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
use crate::error::Diagnostic;
use crate::eval_materialize::force_dict_tree;
use crate::value::{string_val, BuiltinArgs, Thunk};

const KIND: &str = "eval/stream";

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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = {
            let (result, diags) = expect_one_arg(
                "builtin-to-tinct",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // Deep-force all nested structures before serialization.
        // The WHNF-materialized value may contain unevaluated thunks in dict entries,
        // collection elements, or variant payloads. force_dict_tree recursively materializes
        // all nested values so that to_tinct's peek_result calls succeed.
        let deep_val = {
            let (result, diags) = force_dict_tree(&val, &ctx).await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let tinct_str = match deep_val.to_tinct(Some(&ctx)).map_err(|e| {
            Diagnostic::error(KIND, format!("to-tinct: {}", e), call_span.clone())
        }) {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };
        let (result, diags) = ok_val(string_val(&tinct_str), call_span).into_parts();
        _dr_diags.extend(diags);
        match result {
            Ok(v) => crate::error::DiagnosticResult::ok(v).with_diagnostics(_dr_diags),
            Err(e) => crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        }
    })
}
