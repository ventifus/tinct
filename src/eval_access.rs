//! Access expression evaluation: proxy handler invocation.
//!
//! This module contains `invoke_proxy_handler`, extracted
//! from `eval.rs` to keep that module focused on the core evaluation loop.

use std::sync::Arc;

use crate::ast::Span;
use crate::error::{EvalError, EvalResult};
use crate::eval::{materialize, EvalContext};
use crate::eval_call::{invoke_function, CallContext};
use crate::value::{Thunk, Value};

/// Invoke a proxy handler with a key value, returning the result thunk.
pub(crate) async fn invoke_proxy_handler(
    handler: &Arc<Thunk>,
    key_val: Value,
    ctx: &Arc<EvalContext>,
    access_span: &Span,
) -> EvalResult<Arc<Thunk>> {
    // Performance: handler thunk is memoized by Launchbury sharing, but each
    // access clones the materialized Value. Consider eager materialization in
    // builtin_proxy for hot proxy access.
    let handler_val = materialize(handler, Some(access_span), ctx).await?;
    let key_arg = Arc::new(Thunk::new_materialized(key_val, access_span.clone()));
    let key_arg_id = ctx.alloc_thunk(key_arg);
    match handler_val {
        Value::Function {
            params,
            body,
            closure_env_id,
            ..
        } => {
            invoke_function(&CallContext {
                params: &params,
                body: &body,
                closure_env_id,
                positional: &[key_arg_id],
                named: None,
                default_env_id: closure_env_id,
                call_span: access_span.clone(),
                origin: Some(Arc::from("proxy field access")),
                ctx,
            })
            .await
        }
        Value::Builtin(def) => Ok(Arc::new(Thunk::new_pending_builtin(
            def,
            vec![key_arg_id],
            None,
            access_span.clone(),
            Some(Arc::from("proxy field access")),
            ctx.current_env_id, // T-1558: caller_env_id
            Arc::clone(ctx),
        ))),
        _ => Err(EvalError::type_mismatch(
            "Function or Builtin",
            handler_val.type_name(),
            access_span.clone(),
        )
        .into()),
    }
}
