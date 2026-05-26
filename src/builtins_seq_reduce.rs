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

use std::future::Future;
use std::pin::Pin;
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
/// Uses PendingBuiltin step chains to avoid O(N) Rust stack depth for large inputs.
/// Both Dict and Seq paths delegate to helper builtins that process one element
/// at a time, creating lazy PendingBuiltin chains that are forced iteratively
/// by the CEK machine.
///
/// Args: (f, init, xs)
pub(crate) fn builtin_reduce(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("reduce", named.as_ref(), call_span)?;
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
            Value::Overlay(l, r) => {
                Value::Dict(flatten_overlay(&l, &r, "reduce", &ctx, call_span)?)
            }
            Value::Bytes {
                ref source,
                start,
                end,
            } => bytes_to_seq(&source[start..end], call_span, &ctx),
            other => other,
        };
        match xs {
            Value::Dict(ref map) => {
                if map.is_empty() {
                    // Empty dict: return init directly
                    return Ok(init_thunk);
                }

                // Create a Dict value to pass to the step builtin
                // The step builtin will iterate through it
                let xs_thunk = Arc::new(Thunk::new_materialized(xs.clone(), call_span));

                // Create a PendingBuiltin thunk for the first step
                // Args: (f, init, xs_dict, idx_int)
                let idx_thunk = Arc::new(Thunk::new_materialized(Value::Int(0), call_span));
                let step_thunk = Arc::new(Thunk::new_pending_builtin(
                    builtin!("reduce_dict_step", builtin_reduce_dict_step),
                    vec![f_thunk, init_thunk, xs_thunk, idx_thunk],
                    None,
                    call_span,
                    Some(Arc::from("reduce")),
                    Arc::clone(&ctx),
                ));

                Ok(step_thunk)
            }
            Value::Seq {
                head: _head,
                tail: _tail,
            } => {
                // Seq path: delegate to step builtin
                // Args: (f, init, seq)
                let seq_thunk = Arc::new(Thunk::new_materialized(xs.clone(), call_span));
                let step_thunk = Arc::new(Thunk::new_pending_builtin(
                    builtin!("reduce_seq_step", builtin_reduce_seq_step),
                    vec![f_thunk, init_thunk, seq_thunk],
                    None,
                    call_span,
                    Some(Arc::from("reduce")),
                    Arc::clone(&ctx),
                ));

                Ok(step_thunk)
            }
            other => Err(EvalError::type_mismatch_ctx(
                "reduce".to_string(),
                "Dict or Seq",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// Helper for reduce on Dict: processes ALL entries in a single async invocation.
///
/// Args: (f, acc, xs_dict, idx)
///
/// Processes all remaining entries in a loop to avoid creating a chain of
/// N nested thunk materializations that would exhaust the continuation stack (E040).
pub(crate) fn builtin_reduce_dict_step(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        let f_thunk = Arc::clone(&args[0]);
        let mut acc_thunk = Arc::clone(&args[1]);
        let xs = args[2]
            .try_get_materialized()
            .expect("xs should be materialized");
        let idx_val = args[3]
            .try_get_materialized()
            .expect("idx should be materialized");

        let start_idx = match idx_val {
            Value::Int(i) => i as usize,
            _ => {
                return Err(EvalError::internal(
                    "reduce_dict_step: idx must be Int".to_string(),
                    call_span,
                )
                .into())
            }
        };

        match xs {
            Value::Dict(ref map) => {
                // Process all remaining entries in a loop — no recursive thunk chain.
                for (_, value_thunk_id) in map.iter().skip(start_idx) {
                    let value_thunk = ctx.get_thunk(*value_thunk_id);
                    let call_thunk = Arc::new(Thunk::new_pending_call(
                        Arc::clone(&f_thunk),
                        vec![Arc::clone(&acc_thunk), Arc::clone(&value_thunk)],
                        IndexMap::new(),
                        call_span,
                        Arc::clone(&ctx.config.stdlib_env),
                        value_thunk.span,
                        Some(Arc::from("reduce")),
                        Arc::clone(&ctx),
                    ));
                    let new_acc_val =
                        materialize(&call_thunk, None, &ctx)
                            .await
                            .map_err(|mut e| {
                                e.push_frame("in reduce".to_string(), call_span);
                                e
                            })?;
                    acc_thunk = Arc::new(Thunk::new_materialized(new_acc_val, call_span));
                }
                Ok(acc_thunk)
            }
            _ => Err(EvalError::internal(
                "reduce_dict_step: xs must be Dict".to_string(),
                call_span,
            )
            .into()),
        }
    })
}

/// Helper for reduce on Seq: processes ALL head/tail chain entries in a single invocation.
///
/// Args: (f, acc, seq)
///
/// `args[2]` is pre-materialized by `pos_strictness[2]=Spine` via the CEK machine.
/// Processes all elements in a loop to avoid O(N) continuation depth.
pub(crate) fn builtin_reduce_seq_step(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        let f_thunk = Arc::clone(&args[0]);
        let mut acc_thunk = Arc::clone(&args[1]);
        let mut seq_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[2]=Spine");

        // Loop over all elements — no recursive thunk chain.
        loop {
            match seq_val {
                Value::Dict(_) => {
                    // End of sequence — return accumulator
                    return Ok(acc_thunk);
                }
                Value::Seq { head, tail } => {
                    let head_thunk = ctx.get_thunk(head);
                    let tail_thunk = ctx.get_thunk(tail);

                    let call_thunk = Arc::new(Thunk::new_pending_call(
                        Arc::clone(&f_thunk),
                        vec![Arc::clone(&acc_thunk), Arc::clone(&head_thunk)],
                        IndexMap::new(),
                        call_span,
                        Arc::clone(&ctx.config.stdlib_env),
                        head_thunk.span,
                        Some(Arc::from("reduce")),
                        Arc::clone(&ctx),
                    ));
                    let new_acc_val =
                        materialize(&call_thunk, None, &ctx)
                            .await
                            .map_err(|mut e| {
                                e.push_frame("in reduce".to_string(), call_span);
                                e
                            })?;
                    acc_thunk = Arc::new(Thunk::new_materialized(new_acc_val, call_span));

                    // Advance to the tail
                    seq_val = materialize(&tail_thunk, None, &ctx).await?;
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
    })
}

/// `join`: Join elements with a separator string.
///
/// - For Dict: materialize values, stringify each, join with separator
/// - For Seq: traverse head/tail chain, stringify each element, join
///
/// Args: (sep, xs)
/// Inherently materializing: must inspect and stringify all elements to concatenate.
pub(crate) fn builtin_join(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("join", named.as_ref(), call_span)?;
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
                    let val = materialize(&value_thunk, None, &ctx).await?;
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
                    let head_val = materialize(&current_head, None, &ctx).await?;
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
                    let tail_val = materialize(&current_tail, None, &ctx).await?;
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
    })
}

/// `concat`: Concatenate two collections.
///
/// - For Seq: lazily chain xs and ys (O(1) initial, O(n) on materialization).
/// - For Dict: eagerly materialize both dicts and merge them with integer reindexing.
///
/// Args: (xs, ys)
pub(crate) fn builtin_concat(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("concat", named.as_ref(), call_span)?;
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
            Value::Overlay(l, r) => {
                Value::Dict(flatten_overlay(&l, &r, "concat", &ctx, call_span)?)
            }
            other => other,
        };

        match xs {
            Value::Seq { head, tail } => {
                // Seq path: validate ys type eagerly before building the lazy chain.
                // This catches concat(seq(1, 2, 3), 42) at call time rather than
                // deferring the error until the consumer exhausts xs — which would
                // only manifest deep in the PendingBuiltin chain at high stack depth.
                let ys_val = materialize(&ys_thunk, None, &ctx).await?;
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
                    let ys = materialize(&ys_thunk, None, &ctx).await?;
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

                let ys = materialize(&ys_thunk, None, &ctx).await?;
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
    })
}

/// Helper for concat on Seq: lazily chains xs tail with ys.
///
/// Args: (xs_tail, ys)
pub(crate) fn builtin_concat_seq_step(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let xs_tail_thunk = Arc::clone(&args[0]);
        let ys_thunk = Arc::clone(&args[1]);
        let xs_tail = materialize(&xs_tail_thunk, None, &ctx).await?;

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
    })
}
