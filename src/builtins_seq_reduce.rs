//! Sequence reduction builtins: `reduce`, `join`, `concat`.
//!
//! These builtins fold or combine Dict or Seq values. They follow the same
//! dual-dispatch pattern as the other sequence modules: materialize the
//! collection to dispatch on Dict vs Seq, then produce lazy or inherently-eager
//! results depending on the operation.
//!
//! - `reduce`: Dict path builds lazy PendingCall chain (accumulator passed as thunk); Seq path materializes each step eagerly (same rationale: O(N) Rust stack depth)
//! - `join`: inherently eager (must stringify all elements)
//! - `concat`: lazy for Seq (PendingBuiltin step chain), eager for Dict (full merge)
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration in `standard_builtins()` and `create_root_env()` remains in `builtins.rs`.

use std::sync::Arc;

use indexmap::IndexMap;

use crate::builtins::{
    builtin, bytes_to_seq, flatten_overlay, ok_val, reject_named, require_string, stringify,
    MAX_COLLECT_SIZE, MAX_STRING_SIZE,
};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{string_val, BuiltinArgs, Key, Thunk, Value};

/// `reduce`: Fold a function over a Dict or Seq.
///
/// - For Dict: build a chain of PendingCall thunks (acc stays as thunk, not materialized per step).
///   The final accumulator is returned as a thunk; the caller forces it on demand.
/// - For Seq: eagerly materialized per step (avoids O(N) Rust stack depth from lazy chain forcing).
///
/// Args: (f, init, xs)
pub(crate) fn builtin_reduce(ctx_arg: BuiltinArgs) -> EvalResult<Arc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("reduce", named, call_span)?;
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }

    let f_thunk = Arc::clone(&args[0]);
    let init_thunk = Arc::clone(&args[1]);
    let xs = args[2]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");

    // Flatten Overlay to Dict before dispatch. Bytes → Seq of Int byte values.
    let xs = match xs {
        Value::Overlay(l, r) => Value::Dict(flatten_overlay(&l, &r, "reduce", &ctx, call_span)?),
        Value::Bytes {
            ref source,
            start,
            end,
        } => bytes_to_seq(&source[start..end], call_span, &ctx),
        other => other,
    };
    match xs {
        Value::Dict(ref map) => {
            // Eagerly iterate the dict, but pass each step thunk directly as the next
            // accumulator without materializing it. The caller decides when to force
            // the final accumulator. Individual step thunks are PendingCall and will be
            // forced lazily by the materializer when the result is actually needed.
            let mut acc = init_thunk;
            for (_key, value_thunk_id) in map.iter() {
                let value_thunk = ctx.get_thunk(*value_thunk_id);
                let step_thunk = Arc::new(Thunk::new_pending_call(
                    Arc::clone(&f_thunk),
                    vec![Arc::clone(&acc), Arc::clone(&value_thunk)],
                    IndexMap::new(),
                    call_span,
                    Arc::clone(&ctx.config.stdlib_env),
                    value_thunk.span,
                    Some(Arc::from("reduce")),
                    Arc::clone(&ctx),
                ));
                acc = step_thunk;
            }
            Ok(acc)
        }
        Value::Seq { head, tail } => {
            // Eagerly iterate the Seq head/tail chain, materializing each step.
            // Same rationale as Dict: lazy accumulator thunks recurse O(N) deep.
            let mut acc = init_thunk;
            let mut current_head = ctx.get_thunk(head);
            let mut current_tail = ctx.get_thunk(tail);
            loop {
                let step_thunk = Arc::new(Thunk::new_pending_call(
                    Arc::clone(&f_thunk),
                    vec![Arc::clone(&acc), Arc::clone(&current_head)],
                    IndexMap::new(),
                    call_span,
                    Arc::clone(&ctx.config.stdlib_env),
                    current_head.span,
                    Some(Arc::from("reduce")),
                    Arc::clone(&ctx),
                ));
                let step_val = materialize(&step_thunk, Some(&call_span), &ctx)?;
                acc = Arc::new(Thunk::new_materialized(step_val, call_span));

                let tail_val = materialize(&current_tail, Some(&call_span), &ctx)?;
                match tail_val {
                    Value::Dict(_) => break, // empty dict = end of Seq
                    Value::Seq {
                        head: next_head,
                        tail: next_tail,
                    } => {
                        current_head = ctx.get_thunk(next_head);
                        current_tail = ctx.get_thunk(next_tail);
                    }
                    other => {
                        return Err(EvalError::type_mismatch_ctx(
                            "reduce".to_string(),
                            "Dict or Seq",
                            other.type_name(),
                            call_span,
                        )
                        .into());
                    }
                }
            }
            Ok(acc)
        }
        other => Err(EvalError::type_mismatch_ctx(
            "reduce".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `join`: Join elements with a separator string.
///
/// - For Dict: materialize values, stringify each, join with separator
/// - For Seq: traverse head/tail chain, stringify each element, join
///
/// Args: (sep, xs)
/// Inherently materializing: must inspect and stringify all elements to concatenate.
pub(crate) fn builtin_join(ctx_arg: BuiltinArgs) -> EvalResult<Arc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("join", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let sep = args[0]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    let sep_str = require_string("join", sep, args[0].span)?;

    let xs = args[1]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    // Flatten Overlay to Dict before dispatch.
    let xs = match xs {
        Value::Overlay(l, r) => Value::Dict(flatten_overlay(&l, &r, "join", &ctx, call_span)?),
        other => other,
    };
    match xs {
        Value::Dict(ref map) => {
            // Dict path: iterate values, materialize, stringify, join
            let mut parts = Vec::with_capacity(map.len());
            for (_key, value_thunk_id) in map.iter() {
                let value_thunk = ctx.get_thunk(*value_thunk_id);
                let val = materialize(&value_thunk, None, &ctx)?;
                parts.push(stringify(&val));
            }

            // Early return for empty collection
            if parts.is_empty() {
                return ok_val(string_val(""), call_span);
            }

            // Check output size before joining
            let total_parts_len: usize = parts.iter().map(|p| p.len()).sum();
            let sep_contribution = sep_str.len().saturating_mul(parts.len().saturating_sub(1));
            let total_output_len = total_parts_len.saturating_add(sep_contribution);

            if total_output_len > MAX_STRING_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    format!(
                        "join: output would exceed {} MB limit ({} bytes)",
                        MAX_STRING_SIZE / (1024 * 1024),
                        total_output_len
                    ),
                    call_span,
                )
                .into());
            }

            ok_val(string_val(&parts.join(&sep_str)), call_span)
        }
        Value::Seq { head, tail } => {
            // Seq path: traverse head/tail chain, collect strings
            let mut parts = Vec::new();
            let mut current_head = ctx.get_thunk(head);
            let mut current_tail = ctx.get_thunk(tail);

            loop {
                // Materialize and stringify current head
                let head_val = materialize(&current_head, None, &ctx)?;
                parts.push(stringify(&head_val));

                // Check collection size limit
                if parts.len() >= MAX_COLLECT_SIZE {
                    return Err(EvalError::resource_limit_exceeded(
                        format!("join: sequence exceeds {} elements", MAX_COLLECT_SIZE),
                        call_span,
                    )
                    .into());
                }

                // Check tail
                let tail_val = materialize(&current_tail, None, &ctx)?;
                match tail_val {
                    Value::Dict(_) => {
                        // End of sequence
                        break;
                    }
                    Value::Seq { head, tail } => {
                        current_head = ctx.get_thunk(head);
                        current_tail = ctx.get_thunk(tail);
                    }
                    other => {
                        return Err(EvalError::type_mismatch_ctx(
                            "join".to_string(),
                            "Dict or Seq",
                            other.type_name(),
                            call_span,
                        )
                        .into());
                    }
                }
            }

            // Early return for empty collection
            if parts.is_empty() {
                return ok_val(string_val(""), call_span);
            }

            // Check output size before joining
            let total_parts_len: usize = parts.iter().map(|p| p.len()).sum();
            let sep_contribution = sep_str.len().saturating_mul(parts.len().saturating_sub(1));
            let total_output_len = total_parts_len.saturating_add(sep_contribution);

            if total_output_len > MAX_STRING_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    format!(
                        "join: output would exceed {} MB limit ({} bytes)",
                        MAX_STRING_SIZE / (1024 * 1024),
                        total_output_len
                    ),
                    call_span,
                )
                .into());
            }

            ok_val(string_val(&parts.join(&sep_str)), call_span)
        }
        other => Err(EvalError::type_mismatch_ctx(
            "join".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `concat`: Concatenate two collections.
///
/// - For Seq: lazily chain xs and ys (O(1) initial, O(n) on materialization).
/// - For Dict: eagerly materialize both dicts and merge them with integer reindexing.
///
/// Args: (xs, ys)
pub(crate) fn builtin_concat(ctx_arg: BuiltinArgs) -> EvalResult<Arc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("concat", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let xs_span = args[0].span;
    let ys_span = args[1].span;
    let xs = args[0]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    let ys_thunk = Arc::clone(&args[1]);
    // Flatten Overlay to Dict before dispatch.
    let xs = match xs {
        Value::Overlay(l, r) => Value::Dict(flatten_overlay(&l, &r, "concat", &ctx, call_span)?),
        other => other,
    };

    match xs {
        Value::Seq { head, tail } => {
            // Seq path: validate ys type eagerly before building the lazy chain.
            // This catches concat(seq(1, 2, 3), 42) at call time rather than
            // deferring the error until the consumer exhausts xs — which would
            // only manifest deep in the PendingBuiltin chain at high stack depth.
            let ys_val = materialize(&ys_thunk, None, &ctx)?;
            match ys_val {
                Value::Dict(_) | Value::Seq { .. } | Value::Overlay(..) => {}
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "concat".to_string(),
                        "Dict or Seq",
                        other.type_name(),
                        ys_span,
                    )
                    .with_materialization_span(call_span)
                    .into())
                }
            }
            // ys_thunk is now Materialized (memoized). Build the lazy step chain.
            let tail_thunk = ctx.get_thunk(tail);
            let step_args = vec![Arc::clone(&tail_thunk), ys_thunk];
            let result_thunk = Arc::new(Thunk::new_pending_builtin(
                builtin!("concat", builtin_concat_seq_step),
                step_args,
                None,
                call_span,
                Some(Arc::from("call $concat")),
                Arc::clone(&ctx),
            ));
            ok_val(
                Value::Seq {
                    head,
                    tail: ctx.alloc_thunk(result_thunk),
                },
                call_span,
            )
        }
        Value::Dict(ref xs_map) => {
            // Dict path: eagerly merge both dicts with integer reindexing
            if xs_map.is_empty() {
                // Empty xs: validate ys type before returning it directly.
                // Without this check, concat([], 42) would silently succeed.
                let ys = materialize(&ys_thunk, None, &ctx)?;
                match ys {
                    Value::Dict(_) | Value::Seq { .. } | Value::Overlay(..) => {}
                    other => {
                        return Err(EvalError::type_mismatch_ctx(
                            "concat".to_string(),
                            "Dict or Seq",
                            other.type_name(),
                            ys_span,
                        )
                        .with_materialization_span(call_span)
                        .into())
                    }
                }
                return Ok(ys_thunk);
            }

            let ys = materialize(&ys_thunk, None, &ctx)?;
            // Flatten Overlay ys to Dict for the dict-concat path.
            let ys = match ys {
                Value::Overlay(l, r) => {
                    Value::Dict(flatten_overlay(&l, &r, "concat", &ctx, call_span)?)
                }
                other => other,
            };
            match ys {
                Value::Dict(ref ys_map) => {
                    let mut result = IndexMap::with_capacity(xs_map.len() + ys_map.len());
                    let mut idx = 0i64;

                    // Add all values from xs
                    for (_key, value_thunk_id) in xs_map {
                        result.insert(Key::Int(idx), *value_thunk_id);
                        idx = idx.checked_add(1).ok_or_else(|| {
                            EvalError::integer_overflow("concat".to_string(), call_span)
                        })?;
                    }

                    // Add all values from ys
                    for (_key, value_thunk_id) in ys_map {
                        result.insert(Key::Int(idx), *value_thunk_id);
                        idx = idx.checked_add(1).ok_or_else(|| {
                            EvalError::integer_overflow("concat".to_string(), call_span)
                        })?;
                    }

                    ok_val(Value::Dict(result), call_span)
                }
                other => Err(EvalError::type_mismatch_ctx(
                    "concat".to_string(),
                    "Dict",
                    other.type_name(),
                    ys_span,
                )
                .with_materialization_span(call_span)
                .into()),
            }
        }
        other => Err(EvalError::type_mismatch_ctx(
            "concat".to_string(),
            "Dict or Seq",
            other.type_name(),
            xs_span,
        )
        .with_materialization_span(call_span)
        .into()),
    }
}

/// Helper for concat on Seq: lazily chains xs tail with ys.
///
/// Args: (xs_tail, ys)
pub(crate) fn builtin_concat_seq_step(ctx_arg: BuiltinArgs) -> EvalResult<Arc<Thunk>> {
    let BuiltinArgs {
        args,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    let xs_tail_thunk = Arc::clone(&args[0]);
    let ys_thunk = Arc::clone(&args[1]);
    let xs_tail = materialize(&xs_tail_thunk, None, &ctx)?;

    match xs_tail {
        Value::Dict(_) => {
            // End of xs sequence: return ys.
            // Type validation for ys happens eagerly in builtin_concat at call time
            // (for the Seq xs path), so we know ys is a Dict or Seq by this point.
            Ok(ys_thunk)
        }
        Value::Seq { head, tail } => {
            // Continue chaining: head from xs, tail is concat(tail, ys)
            let tail_thunk = ctx.get_thunk(tail);
            let step_args = vec![Arc::clone(&tail_thunk), ys_thunk];
            let new_tail = Arc::new(Thunk::new_pending_builtin(
                builtin!("concat", builtin_concat_seq_step),
                step_args,
                None,
                call_span,
                Some(Arc::from("call $concat")),
                Arc::clone(&ctx),
            ));
            ok_val(
                Value::Seq {
                    head,
                    tail: ctx.alloc_thunk(new_tail),
                },
                call_span,
            )
        }
        other => Err(EvalError::type_mismatch_ctx(
            "concat".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}
