//! Access expression evaluation: proxy handler invocation.
//!
//! This module contains `invoke_proxy_handler`, extracted
//! from `eval.rs` to keep that module focused on the core evaluation loop.

use std::rc::Rc;

use crate::ast::Span;
use crate::error::{EvalError, EvalResult};
use crate::eval::{materialize, EvalContext};
use crate::eval_call::{invoke_function, CallContext};
use crate::value::{Thunk, Value};

/// Invoke a proxy handler with a key value, returning the result thunk.
pub(crate) fn invoke_proxy_handler(
    handler: &Rc<Thunk>,
    key_val: Value,
    ctx: &Rc<EvalContext>,
    access_span: &Span,
) -> EvalResult<Rc<Thunk>> {
    // Performance: handler thunk is memoized by Launchbury sharing, but each
    // access clones the materialized Value. Consider eager materialization in
    // builtin_proxy for hot proxy access.
    let handler_val = materialize(handler, Some(access_span), ctx)?;
    let key_arg = Rc::new(Thunk::new_materialized(key_val, *access_span));
    match handler_val {
        Value::Function {
            params,
            body,
            env: closure_env,
            ..
        } => invoke_function(&CallContext {
            params: &params,
            body: &body,
            closure_env: &closure_env,
            closure_env_id: None,
            positional: &[key_arg],
            named: None,
            default_env: &closure_env,
            call_span: *access_span,
            origin: Some(Rc::from("proxy field access")),
            ctx,
        }),
        Value::Builtin(def) => Ok(Rc::new(Thunk::new_pending_builtin(
            def,
            vec![key_arg],
            None,
            *access_span,
            Some(Rc::from("proxy field access")),
            Rc::clone(ctx),
        ))),
        _ => Err(EvalError::type_mismatch(
            "Function or Builtin",
            handler_val.type_name(),
            *access_span,
        )
        .into()),
    }
}
