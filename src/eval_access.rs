//! Access expression evaluation: range access and proxy handler invocation.
//!
//! This module contains `eval_range_access` and `invoke_proxy_handler`, extracted
//! from `eval.rs` to keep that module focused on the core evaluation loop.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{Expr, Span, Spanned};
use crate::builtins::flatten_overlay;
use crate::error::{EvalError, EvalResult};
use crate::eval::{eval, eval_key, materialize, EvalContext};
use crate::eval_call::{invoke_function, CallContext};
use crate::value::{Environment, Key, Thunk, ThunkId, Value};

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
        } => invoke_function(&CallContext {
            params: &params,
            body: &body,
            closure_env: &closure_env,
            positional: &[key_arg],
            named: None,
            default_env: &closure_env,
            call_span: *access_span,
            depth: depth + 1,
            origin: Some(Rc::from("proxy field access")),
            ctx,
        }),
        Value::Builtin(def) => Ok(Rc::new(Thunk::new_pending_builtin(
            def,
            vec![key_arg],
            None,
            depth + 1,
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
    let target_thunk =
        eval(Rc::new(target.clone()), Rc::clone(env), ctx, depth + 1).map_err(&push_frame)?;
    let target_val =
        materialize(&target_thunk, Some(access_span), ctx, depth + 1).map_err(push_frame)?;

    let map = match target_val {
        Value::Dict(map) => map,
        Value::Overlay(l, r) => {
            flatten_overlay(&l, &r, "range access", ctx, depth + 1, *access_span)
                .map_err(push_frame)?
        }
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

    // Fast path: unbounded range [..] returns the dict unchanged
    if start_key.is_none() && end_key.is_none() {
        return Ok(Rc::clone(&target_thunk));
    }

    let mut result: IndexMap<Key, ThunkId> = IndexMap::new();
    for (k, v) in &map {
        if key_in_range(k, start_key.as_ref(), end_key.as_ref(), *access_span)? {
            result.insert(k.clone(), *v);
        }
    }

    Ok(Rc::new(Thunk::new_materialized(
        Value::Dict(result),
        *access_span,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_span;

    #[test]
    fn test_key_in_range_all_ints() {
        // Test key_in_range with all Int keys
        let span = test_span(1, 1, 1, 10);
        let k = Key::Int(5);
        let start = Key::Int(0);
        let end = Key::Int(10);

        // Key is in range [0, 10)
        let result = key_in_range(&k, Some(&start), Some(&end), span).unwrap();
        assert!(result, "5 should be in range [0, 10)");

        // Key equals start (inclusive)
        let k_at_start = Key::Int(0);
        let result = key_in_range(&k_at_start, Some(&start), Some(&end), span).unwrap();
        assert!(result, "0 should be in range [0, 10)");

        // Key equals end (exclusive)
        let k_at_end = Key::Int(10);
        let result = key_in_range(&k_at_end, Some(&start), Some(&end), span).unwrap();
        assert!(!result, "10 should NOT be in range [0, 10)");

        // Key below start
        let k_below = Key::Int(-1);
        let result = key_in_range(&k_below, Some(&start), Some(&end), span).unwrap();
        assert!(!result, "-1 should NOT be in range [0, 10)");

        // Key above end
        let k_above = Key::Int(15);
        let result = key_in_range(&k_above, Some(&start), Some(&end), span).unwrap();
        assert!(!result, "15 should NOT be in range [0, 10)");
    }

    #[test]
    fn test_key_in_range_unbounded() {
        // Test key_in_range with unbounded start/end (None)
        let span = test_span(1, 1, 1, 10);
        let k = Key::Int(100);

        // No bounds (all keys match)
        let result = key_in_range(&k, None, None, span).unwrap();
        assert!(result, "key should be in unbounded range");

        // Only start bound
        let start = Key::Int(50);
        let result = key_in_range(&k, Some(&start), None, span).unwrap();
        assert!(result, "100 should be >= 50");

        // Only end bound
        let end = Key::Int(200);
        let result = key_in_range(&k, None, Some(&end), span).unwrap();
        assert!(result, "100 should be < 200");
    }

    #[test]
    fn test_key_in_range_mixed_types_error() {
        // Test that mixing Int and String keys produces an error
        let span = test_span(1, 1, 1, 10);
        let k = Key::Int(5);
        let start = Key::String("abc".into());

        let result = key_in_range(&k, Some(&start), None, span);
        assert!(
            result.is_err(),
            "Mixing Int and String keys should produce an error"
        );
    }
}
