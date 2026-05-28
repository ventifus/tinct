//! Sequence transform builtins: `map`, `filter`, `take`, `drop`.
//!
//! These builtins apply transformations to Dict or Seq values. All follow the
//! dual-dispatch pattern: materialize the collection to dispatch on Dict vs Seq,
//! then produce lazy results (PendingCall for map on dict, PendingBuiltin for
//! filter/take/drop step functions).
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration in `standard_builtins()` remains in `builtins.rs`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::{CoreExpr, Span, Spanned};
use crate::builtins::{builtin, bytes_to_seq, flatten_overlay, ok_val, reject_named};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{BuiltinArgs, Key, Strictness, Thunk, ThunkId, Value};

/// Helper: create a synthetic CoreExpr::Call for builtin-generated calls.
/// Used when builtins construct PendingCall thunks (e.g., map, filter, until).
/// The CoreExpr is needed for DepthExceeded restore but won't be re-evaluated.
fn synthetic_call_expr(span: Span) -> Arc<Spanned<CoreExpr>> {
    Arc::new(Spanned {
        node: CoreExpr::Call {
            func: Arc::new(Spanned {
                node: CoreExpr::Int(0), // placeholder, never evaluated
                span: span.clone(),
            }),
            args: vec![],
            named_args: vec![],
            implied: false,
        },
        span,
    })
}

/// `map`: Apply a function to every element of a dict or sequence.
///
/// - For Dict: applies f to each value, preserving keys. Values are lazy (PendingCall thunks).
/// - For Seq: applies f to each element, returning a lazy Seq.
///
/// Args: (f, xs)
pub(crate) fn builtin_map(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("map", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let f_thunk = Arc::clone(&args[0]);
        let xs = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        // Flatten Overlay to Dict before dispatch.
        let xs = match xs {
            Value::Overlay(l, r) => {
                Value::Dict(flatten_overlay(&l, &r, "map", &ctx, call_span.clone())?)
            }
            // Bytes: treat as Seq of Int byte values
            Value::Bytes {
                ref source,
                start,
                end,
            } => bytes_to_seq(&source[start..end], call_span.clone(), &ctx),
            other => other,
        };

        match xs {
            Value::Dict(ref map) => {
                // Dict path: create PendingCall thunks for each value
                let mut new_map = IndexMap::with_capacity(map.len());
                for (key, value_thunk_id) in map {
                    let value_thunk = ctx.get_thunk(*value_thunk_id);
                    let pending_call = Arc::new(Thunk::new_pending_call(
                        Arc::clone(&f_thunk),
                        vec![Arc::clone(&value_thunk)],
                        IndexMap::new(),
                        call_span.clone(),
                        Arc::clone(&ctx.config.stdlib_env),
                        value_thunk.span.clone(),
                        Some(Arc::from("map")),
                        Arc::clone(&ctx),
                        synthetic_call_expr(call_span.clone()),
                    ));
                    new_map.insert(key.clone(), ctx.alloc_thunk(pending_call));
                }
                ok_val(Value::Dict(new_map), call_span)
            }
            Value::Seq { head, tail } => {
                // Seq path: head = f(head), tail = map(f, tail)
                let head_thunk = ctx.get_thunk(head);
                let tail_thunk = ctx.get_thunk(tail);
                let new_head = Arc::new(Thunk::new_pending_call(
                    Arc::clone(&f_thunk),
                    vec![Arc::clone(&head_thunk)],
                    IndexMap::new(),
                    call_span.clone(),
                    Arc::clone(&ctx.config.stdlib_env),
                    head_thunk.span.clone(),
                    Some(Arc::from("map head")),
                    Arc::clone(&ctx),
                    synthetic_call_expr(call_span.clone()),
                ));
                let tail_args = vec![Arc::clone(&f_thunk), Arc::clone(&tail_thunk)];
                let new_tail = Arc::new(Thunk::new_pending_builtin(
                    // Must match the standard_builtins() registration: force_count=1 forces
                    // args[0] (the function), and Spine on args[1] forces the tail sequence.
                    // Without force_count here, builtin_map would panic at its
                    // `args[1].try_get_materialized().expect("pre-materialized by force_count/pos_strictness")`
                    // when the tail Seq is an unevaluated thunk.
                    builtin!(
                        "builtin-map",
                        builtin_map,
                        [Strictness::Id, Strictness::Spine],
                        1
                    ),
                    tail_args,
                    None,
                    call_span.clone(),
                    Some(Arc::from("call $map")),
                    Arc::clone(&ctx),
                ));
                ok_val(
                    Value::Seq {
                        head: ctx.alloc_thunk(new_head),
                        tail: ctx.alloc_thunk(new_tail),
                    },
                    call_span,
                )
            }
            other => Err(EvalError::type_mismatch_ctx(
                "map".to_string(),
                "Dict or Seq",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `filter`: Keep only elements where the predicate returns true.
///
/// - For Dict: evaluates pred for each value, returns Seq of values that pass.
/// - For Seq: evaluates pred for each element, returns lazy Seq of passing elements.
///
/// Args: (pred, xs)
pub(crate) fn builtin_filter(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("filter", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let pred_thunk = Arc::clone(&args[0]);
        let xs = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        // Flatten Overlay to Dict before dispatch.
        let xs = match xs {
            Value::Overlay(l, r) => {
                Value::Dict(flatten_overlay(&l, &r, "filter", &ctx, call_span.clone())?)
            }
            // Bytes: treat as Seq of Int byte values
            Value::Bytes {
                ref source,
                start,
                end,
            } => bytes_to_seq(&source[start..end], call_span.clone(), &ctx),
            other => other,
        };

        match xs {
            Value::Dict(_) => {
                // Dict path: iterate entries by key order, building a Seq of values
                // that pass the predicate.
                // Note: do NOT early-return for empty dicts here. Let filter_dict_step
                // handle the zero-entry case via its loop termination so that empty
                // dicts go through the same PendingBuiltin code path as non-empty dicts.
                // This keeps the return type consistent (always a PendingBuiltin that
                // eventually resolves to the nil-sentinel or a Seq).

                // args[1] is already Materialized after the materialize() call above,
                // so re-use the existing thunk directly. This avoids cloning the entire
                // IndexMap (O(n)) — each step's materialize() call is still O(1) because
                // the thunk is already in Materialized state.
                let dict_thunk = Arc::clone(&args[1]);
                let idx_thunk = ok_val(Value::Int(0), call_span.clone())?;

                let filter_args = vec![Arc::clone(&pred_thunk), dict_thunk, idx_thunk];

                let result_thunk = Arc::new(Thunk::new_pending_builtin(
                    builtin!("builtin-filter", builtin_filter_dict_step, [], 3),
                    filter_args,
                    None,
                    call_span,
                    Some(Arc::from("call $filter")),
                    Arc::clone(&ctx),
                ));
                Ok(result_thunk)
            }
            Value::Seq { head: _, tail: _ } => {
                // Seq path: lazy filter. Use `depth` (not `depth + 1`) for the
                // initial PendingBuiltin, matching the Dict path and the convention
                // for all other initial-dispatch step functions (unfold, drop, etc.).
                // Depth increments happen inside builtin_filter_seq_step on the
                // recursive tail PendingBuiltins.
                let filter_args = vec![Arc::clone(&pred_thunk), Arc::clone(&args[1])];
                let result_thunk = Arc::new(Thunk::new_pending_builtin(
                    builtin!("builtin-filter", builtin_filter_seq_step),
                    filter_args,
                    None,
                    call_span,
                    Some(Arc::from("call $filter")),
                    Arc::clone(&ctx),
                ));
                Ok(result_thunk)
            }
            other => Err(EvalError::type_mismatch_ctx(
                "filter".to_string(),
                "Dict or Seq",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// Helper for filter on Dict: iterates through dict entries, building a Seq.
///
/// Args: (pred, dict, idx)
///
/// Consecutive predicate failures are handled by an internal loop rather than
/// chaining PendingBuiltin thunks. A PendingBuiltin-per-failure would consume
/// one depth unit per rejected entry (N failures → ~2N depth units total),
/// hitting MAX_EVAL_DEPTH far earlier than expected on sparse dicts. The
/// loop short-circuits skips at zero extra depth cost, then defers lazily on
/// the first pass.
pub(crate) fn builtin_filter_dict_step(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let pred_thunk = Arc::clone(&args[0]);
        let dict_thunk = Arc::clone(&args[1]);

        let mut idx_int = match args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness")
        {
            Value::Int(i) => i,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "filter".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // dict_thunk is pre-wrapped as Materialized at the filter call site
        debug_assert!(dict_thunk.try_get_materialized().is_some());
        // Extract the IndexMap<Key, ThunkId> by consuming the Value::Dict so that
        // no !Send Value is held across .await points in the loop below.
        let dict_map: IndexMap<Key, ThunkId> = {
            let dict = dict_thunk
                .try_get_materialized()
                .expect("pre-materialized by force_count/pos_strictness");
            match dict {
                Value::Dict(m) => m,
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "filter".to_string(),
                        "Dict",
                        other.type_name(),
                        call_span,
                    )
                    .into())
                }
            }
        };

        // Loop over consecutive failing entries without consuming extra depth.
        // We only defer via PendingBuiltin when we find a passing entry (the tail
        // of the emitted Seq node) so depth counts emitted elements, not rejections.
        loop {
            // Check if we've reached the end
            let idx_usize = usize::try_from(idx_int).ok();
            if idx_usize.is_none() || idx_int >= dict_map.len() as i64 {
                return ok_val(Value::Dict(IndexMap::new()), call_span);
            }

            // Get the current entry by index (avoids secondary keys map)
            let value_thunk = match dict_map.get_index(idx_usize.unwrap()) {
                Some((_k, v)) => ctx.get_thunk(*v),
                None => {
                    return Err(EvalError::internal(
                        format!("filter: entry at index {} not found", idx_int),
                        call_span,
                    )
                    .into())
                }
            };

            // Apply predicate
            let pred_call = Arc::new(Thunk::new_pending_call(
                Arc::clone(&pred_thunk),
                vec![Arc::clone(&value_thunk)],
                IndexMap::new(),
                call_span.clone(),
                Arc::clone(&ctx.config.stdlib_env),
                value_thunk.span.clone(),
                Some(Arc::from("filter-dict pred")),
                Arc::clone(&ctx),
                synthetic_call_expr(call_span.clone()),
            ));
            let pred_result = materialize(&pred_call, None, &ctx).await?;

            let passes = match pred_result {
                Value::Bool(b) => b,
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "filter".to_string(),
                        "Bool",
                        other.type_name(),
                        call_span,
                    )
                    .into())
                }
            };

            if passes {
                // Include this value; defer the rest lazily
                let next_idx_thunk = ok_val(Value::Int(idx_int + 1), call_span.clone())?;
                let tail_args = vec![
                    Arc::clone(&pred_thunk),
                    Arc::clone(&dict_thunk),
                    next_idx_thunk,
                ];
                let tail = Arc::new(Thunk::new_pending_builtin(
                    builtin!("builtin-filter", builtin_filter_dict_step, [], 3),
                    tail_args,
                    None,
                    call_span.clone(),
                    Some(Arc::from("call $filter")),
                    Arc::clone(&ctx),
                ));

                return ok_val(
                    Value::Seq {
                        head: ctx.alloc_thunk(value_thunk),
                        tail: ctx.alloc_thunk(tail),
                    },
                    call_span,
                );
            } else {
                // Skip this entry: advance the loop without extra depth
                idx_int += 1;
            }
        }
    })
}

/// Helper for filter on Seq: lazily filters sequence elements.
///
/// Args: (pred, seq)
///
/// Consecutive predicate failures are handled by an internal loop rather than
/// chaining PendingBuiltin thunks. A PendingBuiltin-per-failure would consume
/// one depth unit per rejected element (N failures → ~2N depth units total),
/// hitting MAX_EVAL_DEPTH far earlier than expected on sparse sequences. The
/// loop short-circuits skips at zero extra depth cost, then defers lazily on
/// the first pass.
pub(crate) fn builtin_filter_seq_step(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let pred_thunk = Arc::clone(&args[0]);
        let seq_thunk = Arc::clone(&args[1]);

        // Loop over consecutive failing elements without consuming extra depth.
        // We only defer via PendingBuiltin when we find a passing element (the tail
        // of the emitted Seq node) so depth counts emitted elements, not rejections.
        let mut current = materialize(&seq_thunk, None, &ctx).await?;
        loop {
            match current {
                Value::Dict(_) => {
                    // End of sequence
                    return ok_val(Value::Dict(IndexMap::new()), call_span);
                }
                Value::Seq { head, tail } => {
                    // Apply predicate to head
                    let head_thunk = ctx.get_thunk(head);
                    let tail_thunk = ctx.get_thunk(tail);
                    let pred_call = Arc::new(Thunk::new_pending_call(
                        Arc::clone(&pred_thunk),
                        vec![Arc::clone(&head_thunk)],
                        IndexMap::new(),
                        call_span.clone(),
                        Arc::clone(&ctx.config.stdlib_env),
                        head_thunk.span.clone(),
                        Some(Arc::from("filter-seq pred")),
                        Arc::clone(&ctx),
                        synthetic_call_expr(call_span.clone()),
                    ));
                    let pred_result = materialize(&pred_call, None, &ctx).await?;

                    let passes = match pred_result {
                        Value::Bool(b) => b,
                        other => {
                            return Err(EvalError::type_mismatch_ctx(
                                "filter".to_string(),
                                "Bool",
                                other.type_name(),
                                call_span,
                            )
                            .into())
                        }
                    };

                    if passes {
                        // Include this element; defer the rest lazily
                        let tail_args = vec![Arc::clone(&pred_thunk), Arc::clone(&tail_thunk)];
                        let new_tail = Arc::new(Thunk::new_pending_builtin(
                            builtin!("builtin-filter", builtin_filter_seq_step),
                            tail_args,
                            None,
                            call_span.clone(),
                            Some(Arc::from("call $filter")),
                            Arc::clone(&ctx),
                        ));
                        return ok_val(
                            Value::Seq {
                                head,
                                tail: ctx.alloc_thunk(new_tail),
                            },
                            call_span,
                        );
                    } else {
                        // Skip this element: advance the loop without extra depth
                        current = materialize(&tail_thunk, None, &ctx).await?;
                    }
                }
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "filter".to_string(),
                        "Dict or Seq",
                        other.type_name(),
                        call_span,
                    )
                    .into())
                }
            }
        }
    })
}

/// `take`: Take the first n elements from a dict or sequence.
///
/// - For Dict: takes first n entries by position, preserving keys. Returns Dict.
/// - For Seq: takes first n elements, returning a Seq (or terminal empty dict).
/// - If n <= 0: returns empty dict (terminal for Seq).
pub(crate) fn builtin_take(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("take", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let n = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let n_int = match n {
            Value::Int(i) => i,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "take".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        if n_int <= 0 {
            // Return empty dict (terminal for Seq, empty for Dict)
            return ok_val(Value::Dict(IndexMap::new()), call_span);
        }

        let xs = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        // Bytes: treat as Seq of Int byte values
        let xs = match xs {
            Value::Bytes {
                ref source,
                start,
                end,
            } => bytes_to_seq(&source[start..end], call_span.clone(), &ctx),
            other => other,
        };
        match xs {
            Value::Dict(ref map) => {
                // Dict: take first n entries by position
                let taken: IndexMap<Key, ThunkId> = map
                    .iter()
                    .take(n_int as usize)
                    .map(|(k, v)| (k.clone(), *v))
                    .collect();
                ok_val(Value::Dict(taken), call_span)
            }
            Value::Seq { head, tail } => {
                // Seq: head = seq head, tail = take(n-1, seq tail)
                let tail_thunk = ctx.get_thunk(tail);
                let tail_args = vec![
                    ok_val(Value::Int(n_int - 1), call_span.clone())?,
                    Arc::clone(&tail_thunk),
                ];
                let new_tail = Arc::new(Thunk::new_pending_builtin(
                    // Must match the standard_builtins() registration: force_count=2 forces
                    // args[0] (the count) and args[1] (the sequence).
                    // args[0] is already Materialized (ok_val above), but args[1] (tail_thunk)
                    // may be unevaluated. Without force_count here, builtin_take would panic at
                    // `args[1].try_get_materialized().expect("pre-materialized by force_count/pos_strictness")`.
                    builtin!(
                        "take",
                        builtin_take,
                        [Strictness::Seq, Strictness::Spine],
                        2
                    ),
                    tail_args,
                    None,
                    call_span.clone(),
                    Some(Arc::from("call $take")),
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
                "take".to_string(),
                "Dict or Seq",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `drop`: Drop the first n elements from a Dict or Seq.
///
/// - For Dict: skip first n entries by position, return Dict with remaining entries
/// - For Seq: use lazy step function to drop elements one at a time
///
/// Args: (n, xs)
pub(crate) fn builtin_drop(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("drop", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let n = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let n_int = match n {
            Value::Int(i) => i,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "drop".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        if n_int <= 0 {
            // Return xs unchanged
            return Ok(Arc::clone(&args[1]));
        }

        let xs = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        // Bytes: treat as Seq of Int byte values
        let xs = match xs {
            Value::Bytes {
                ref source,
                start,
                end,
            } => bytes_to_seq(&source[start..end], call_span.clone(), &ctx),
            other => other,
        };
        match xs {
            Value::Dict(ref map) => {
                // Dict: skip first n entries by position
                let dropped: IndexMap<Key, ThunkId> = map
                    .iter()
                    .skip(n_int as usize)
                    .map(|(k, v)| (k.clone(), *v))
                    .collect();
                ok_val(Value::Dict(dropped), call_span)
            }
            Value::Seq { head: _, tail } => {
                // Seq: use lazy step function to drop remaining elements
                let n_minus_1 = Arc::new(Thunk::new_materialized(
                    Value::Int(n_int - 1),
                    call_span.clone(),
                ));
                let tail_thunk = ctx.get_thunk(tail);
                let step_args = vec![n_minus_1, Arc::clone(&tail_thunk)];
                Ok(Arc::new(Thunk::new_pending_builtin(
                    builtin!("builtin-drop", builtin_drop_seq_step, [], 2),
                    step_args,
                    None,
                    call_span,
                    Some(Arc::from("call $drop")),
                    Arc::clone(&ctx),
                )))
            }
            other => Err(EvalError::type_mismatch_ctx(
                "drop".to_string(),
                "Dict or Seq",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// Helper for `drop` on Seq: lazily drop elements one at a time.
///
/// Args: (n_remaining, seq)
pub(crate) fn builtin_drop_seq_step(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        let n = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let n_int = match n {
            Value::Int(i) => i,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "drop".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        if n_int <= 0 {
            // Done dropping, return remaining seq
            return Ok(Arc::clone(&args[1]));
        }

        let seq = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        match seq {
            Value::Dict(_) => {
                // End of sequence before we finished dropping
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
            Value::Seq { head: _, tail } => {
                // Drop this element, continue with tail
                let n_minus_1 = Arc::new(Thunk::new_materialized(
                    Value::Int(n_int - 1),
                    call_span.clone(),
                ));
                let tail_thunk = ctx.get_thunk(tail);
                let step_args = vec![n_minus_1, Arc::clone(&tail_thunk)];
                Ok(Arc::new(Thunk::new_pending_builtin(
                    builtin!("builtin-drop", builtin_drop_seq_step, [], 2),
                    step_args,
                    None,
                    call_span,
                    Some(Arc::from("call $drop")),
                    Arc::clone(&ctx),
                )))
            }
            other => Err(EvalError::type_mismatch_ctx(
                "drop".to_string(),
                "Dict or Seq",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}
