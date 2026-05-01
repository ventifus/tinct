//! Access expression evaluation: range access and proxy handler invocation.
//!
//! This module contains `eval_range_access` and `invoke_proxy_handler`, extracted
//! from `eval.rs` to keep that module focused on the core evaluation loop.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{Expr, Span, Spanned};
use crate::error::{EvalError, EvalResult};
use crate::eval::{eval, eval_key, materialize, EvalContext};
use crate::eval_call::{invoke_function, CallContext};
use crate::value::{Environment, Key, Thunk, Value};

thread_local! {
    /// Shared empty IndexMap for named arguments when none are provided.
    /// Avoids per-call allocation in invoke_proxy_handler.
    static EMPTY_NAMED_ARGS: IndexMap<String, Rc<Thunk>> = IndexMap::new();
}

/// Check whether `k` falls in the half-open range `[start, end)`.
/// `None` bounds are treated as unbounded (i.e. negative/positive infinity).
/// Returns an error when `k` is not comparable with the bound (mixed key types).
pub(crate) fn key_in_range(
    k: &Key,
    start: Option<&Key>,
    end: Option<&Key>,
    span: Span,
) -> EvalResult<bool> {
    let after_start = match start {
        Some(s) => {
            let ord = k.partial_cmp(s).ok_or_else(|| {
                EvalError::type_mismatch_ctx(
                    "range access".to_string(),
                    "comparable key types (both Int or both String)",
                    "mixed Int and String keys",
                    span,
                )
            })?;
            ord != std::cmp::Ordering::Less
        }
        None => true,
    };
    let before_end = match end {
        Some(e) => {
            let ord = k.partial_cmp(e).ok_or_else(|| {
                EvalError::type_mismatch_ctx(
                    "range access".to_string(),
                    "comparable key types (both Int or both String)",
                    "mixed Int and String keys",
                    span,
                )
            })?;
            ord == std::cmp::Ordering::Less
        }
        None => true,
    };
    Ok(after_start && before_end)
}

/// Invoke a proxy handler with a key value, returning the result thunk.
///
/// Proxy-handler-returns-Proxy chains are bounded by MAX_EVAL_DEPTH.
/// Each invoke_proxy_handler call costs 1 depth level via materialize().
pub(crate) fn invoke_proxy_handler(
    handler: &Rc<Thunk>,
    key_val: Value,
    ctx: &Rc<EvalContext>,
    access_span: &Span,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    // Performance: handler thunk is memoized by Launchbury sharing, but each
    // access clones the materialized Value. Consider eager materialization in
    // builtin_proxy for hot proxy access.
    let handler_val = materialize(handler, Some(access_span), ctx, depth + 1)?;
    let key_arg = Rc::new(Thunk::new_materialized(key_val, *access_span));
    match handler_val {
        Value::Function {
            params,
            body,
            env: closure_env,
        } => EMPTY_NAMED_ARGS.with(|empty| {
            invoke_function(&CallContext {
                params: &params,
                body: &body,
                closure_env: &closure_env,
                positional: &[key_arg],
                named: empty,
                default_env: &closure_env,
                call_span: *access_span,
                depth: depth + 1,
                origin: Cow::Borrowed("proxy field access"),
                ctx,
            })
        }),
        Value::Builtin { name, func } => {
            // Create a fresh empty IndexMap for named args (0 capacity, no allocation)
            Ok(Rc::new(Thunk::new_pending_builtin(
                name,
                func,
                vec![key_arg],
                IndexMap::new(),
                depth + 1,
                *access_span,
                Cow::Borrowed("proxy field access"),
                Rc::clone(ctx),
            )))
        }
        _ => Err(EvalError::type_mismatch(
            "Function or Builtin",
            handler_val.type_name(),
            *access_span,
        )
        .into()),
    }
}

/// RangeAccess: materialize target, filter dict entries by key range.
/// Range is [start, end) -- start inclusive, end exclusive.
/// Mixed-type keys (some Int, some String) produce an error.
pub(crate) fn eval_range_access(
    target: &Spanned<Expr>,
    start: Option<&Spanned<Expr>>,
    end: Option<&Spanned<Expr>>,
    env: &Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    access_span: &Span,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    let push_frame = |mut e: Box<EvalError>| -> Box<EvalError> {
        e.push_frame("accessing [..:..]".to_string(), *access_span);
        e
    };
    let target_thunk = eval(target, Rc::clone(env), ctx, depth + 1).map_err(&push_frame)?;
    let target_val =
        materialize(&target_thunk, Some(access_span), ctx, depth + 1).map_err(push_frame)?;

    let map = match target_val {
        Value::Dict(map) => map,
        Value::Proxy { .. } => {
            return Err(push_frame(
                EvalError::type_mismatch_ctx(
                    "range access".to_string(),
                    "Dict",
                    "Proxy",
                    target_thunk.span,
                )
                .with_materialization_span(*access_span)
                .into(),
            ));
        }
        _ => {
            return Err(push_frame(
                EvalError::type_mismatch_ctx(
                    "range access".to_string(),
                    "Dict",
                    target_val.type_name(),
                    target_thunk.span,
                )
                .with_materialization_span(*access_span)
                .into(),
            ));
        }
    };

    let start_key = start.map(|e| eval_key(e, env, ctx, depth)).transpose()?;
    let end_key = end.map(|e| eval_key(e, env, ctx, depth)).transpose()?;

    let mut result: IndexMap<Key, Rc<Thunk>> = IndexMap::new();
    for (k, v) in &map {
        if key_in_range(k, start_key.as_ref(), end_key.as_ref(), *access_span)? {
            result.insert(k.clone(), Rc::clone(v));
        }
    }

    Ok(Rc::new(Thunk::new_materialized(
        Value::Dict(result),
        *access_span,
    )))
}
