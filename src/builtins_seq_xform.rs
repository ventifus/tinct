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

use std::rc::Rc;

use indexmap::IndexMap;

use crate::builtins::{builtin, bytes_to_seq, flatten_overlay, ok_val, reject_named};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{BuiltinArgs, Key, Thunk, ThunkId, ThunkState, Value};

/// `map`: Apply a function to every element of a dict or sequence.
///
/// - For Dict: applies f to each value, preserving keys. Values are lazy (PendingCall thunks).
/// - For Seq: applies f to each element, returning a lazy Seq.
///
/// Args: (f, xs)
pub(crate) fn builtin_map(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("map", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let f_thunk = Rc::clone(&args[0]);
    let xs = materialize(&args[1], None, &ctx)?;
    // Flatten Overlay to Dict before dispatch.
    let xs = match xs {
        Value::Overlay(l, r) => Value::Dict(flatten_overlay(&l, &r, "map", &ctx, call_span)?),
        // Bytes: treat as Seq of Int byte values
        Value::Bytes {
            ref source,
            start,
            end,
        } => bytes_to_seq(&source[start..end], call_span, &ctx),
        other => other,
    };

    match xs {
        Value::Dict(ref map) => {
            // Dict path: create PendingCall thunks for each value
            let mut new_map = IndexMap::with_capacity(map.len());
            for (key, value_thunk_id) in map {
                let value_thunk = ctx.get_thunk(*value_thunk_id);
                let pending_call = Rc::new(Thunk::new_pending_call(
                    Rc::clone(&f_thunk),
                    vec![Rc::clone(&value_thunk)],
                    IndexMap::new(),
                    call_span,
                    Rc::clone(&ctx.config.stdlib_env),
                    value_thunk.span,
                    Some(Rc::from("map")),
                    Rc::clone(&ctx),
                ));
                new_map.insert(key.clone(), ctx.alloc_thunk(pending_call));
            }
            ok_val(Value::Dict(new_map), call_span)
        }
        Value::Seq { head, tail } => {
            // Seq path: head = f(head), tail = map(f, tail)
            let head_thunk = ctx.get_thunk(head);
            let tail_thunk = ctx.get_thunk(tail);
            let new_head = Rc::new(Thunk::new_pending_call(
                Rc::clone(&f_thunk),
                vec![Rc::clone(&head_thunk)],
                IndexMap::new(),
                call_span,
                Rc::clone(&ctx.config.stdlib_env),
                head_thunk.span,
                Some(Rc::from("map head")),
                Rc::clone(&ctx),
            ));
            let tail_args = vec![Rc::clone(&f_thunk), Rc::clone(&tail_thunk)];
            let new_tail = Rc::new(Thunk::new_pending_builtin(
                builtin!("map", builtin_map),
                tail_args,
                None,
                call_span,
                Some(Rc::from("call $map")),
                Rc::clone(&ctx),
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
}

/// `filter`: Keep only elements where the predicate returns true.
///
/// - For Dict: evaluates pred for each value, returns Seq of values that pass.
/// - For Seq: evaluates pred for each element, returns lazy Seq of passing elements.
///
/// Args: (pred, xs)
pub(crate) fn builtin_filter(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("filter", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let pred_thunk = Rc::clone(&args[0]);
    let xs = materialize(&args[1], None, &ctx)?;
    // Flatten Overlay to Dict before dispatch.
    let xs = match xs {
        Value::Overlay(l, r) => Value::Dict(flatten_overlay(&l, &r, "filter", &ctx, call_span)?),
        // Bytes: treat as Seq of Int byte values
        Value::Bytes {
            ref source,
            start,
            end,
        } => bytes_to_seq(&source[start..end], call_span, &ctx),
        other => other,
    };

    match xs {
        Value::Dict(ref map) => {
            // Dict path: iterate entries by key order, building a Seq of values
            // that pass the predicate
            if map.is_empty() {
                return ok_val(Value::Dict(IndexMap::new()), call_span);
            }

            // args[1] is already Materialized after the materialize() call above,
            // so re-use the existing thunk directly. This avoids cloning the entire
            // IndexMap (O(n)) — each step's materialize() call is still O(1) because
            // the thunk is already in Materialized state.
            let dict_thunk = Rc::clone(&args[1]);
            let idx_thunk = ok_val(Value::Int(0), call_span)?;

            let filter_args = vec![Rc::clone(&pred_thunk), dict_thunk, idx_thunk];

            let result_thunk = Rc::new(Thunk::new_pending_builtin(
                builtin!("filter", builtin_filter_dict_step),
                filter_args,
                None,
                call_span,
                Some(Rc::from("call $filter")),
                Rc::clone(&ctx),
            ));
            Ok(result_thunk)
        }
        Value::Seq { head: _, tail: _ } => {
            // Seq path: lazy filter. Use `depth` (not `depth + 1`) for the
            // initial PendingBuiltin, matching the Dict path and the convention
            // for all other initial-dispatch step functions (unfold, drop, etc.).
            // Depth increments happen inside builtin_filter_seq_step on the
            // recursive tail PendingBuiltins.
            let filter_args = vec![Rc::clone(&pred_thunk), Rc::clone(&args[1])];
            let result_thunk = Rc::new(Thunk::new_pending_builtin(
                builtin!("filter", builtin_filter_seq_step),
                filter_args,
                None,
                call_span,
                Some(Rc::from("call $filter")),
                Rc::clone(&ctx),
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
pub(crate) fn builtin_filter_dict_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    let pred_thunk = Rc::clone(&args[0]);
    let dict_thunk = Rc::clone(&args[1]);

    let mut idx_int = match materialize(&args[2], None, &ctx)? {
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
    debug_assert!(matches!(&*dict_thunk.state(), ThunkState::Materialized(_)));
    let dict = materialize(&dict_thunk, None, &ctx)?;
    let dict_map = match dict {
        Value::Dict(ref m) => m,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "filter".to_string(),
                "Dict",
                other.type_name(),
                call_span,
            )
            .into())
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
        let pred_call = Rc::new(Thunk::new_pending_call(
            Rc::clone(&pred_thunk),
            vec![Rc::clone(&value_thunk)],
            IndexMap::new(),
            call_span,
            Rc::clone(&ctx.config.stdlib_env),
            value_thunk.span,
            Some(Rc::from("filter-dict pred")),
            Rc::clone(&ctx),
        ));
        let pred_result = materialize(&pred_call, None, &ctx)?;

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
            let next_idx_thunk = ok_val(Value::Int(idx_int + 1), call_span)?;
            let tail_args = vec![
                Rc::clone(&pred_thunk),
                Rc::clone(&dict_thunk),
                next_idx_thunk,
            ];
            let tail = Rc::new(Thunk::new_pending_builtin(
                builtin!("filter", builtin_filter_dict_step),
                tail_args,
                None,
                call_span,
                Some(Rc::from("call $filter")),
                Rc::clone(&ctx),
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
pub(crate) fn builtin_filter_seq_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    let pred_thunk = Rc::clone(&args[0]);
    let seq_thunk = Rc::clone(&args[1]);

    // Loop over consecutive failing elements without consuming extra depth.
    // We only defer via PendingBuiltin when we find a passing element (the tail
    // of the emitted Seq node) so depth counts emitted elements, not rejections.
    let mut current = materialize(&seq_thunk, None, &ctx)?;
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
                let pred_call = Rc::new(Thunk::new_pending_call(
                    Rc::clone(&pred_thunk),
                    vec![Rc::clone(&head_thunk)],
                    IndexMap::new(),
                    call_span,
                    Rc::clone(&ctx.config.stdlib_env),
                    head_thunk.span,
                    Some(Rc::from("filter-seq pred")),
                    Rc::clone(&ctx),
                ));
                let pred_result = materialize(&pred_call, None, &ctx)?;

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
                    let tail_args = vec![Rc::clone(&pred_thunk), Rc::clone(&tail_thunk)];
                    let new_tail = Rc::new(Thunk::new_pending_builtin(
                        builtin!("filter", builtin_filter_seq_step),
                        tail_args,
                        None,
                        call_span,
                        Some(Rc::from("call $filter")),
                        Rc::clone(&ctx),
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
                    current = materialize(&tail_thunk, None, &ctx)?;
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
}

/// `take`: Take the first n elements from a dict or sequence.
///
/// - For Dict: takes first n entries by position, preserving keys. Returns Dict.
/// - For Seq: takes first n elements, returning a Seq (or terminal empty dict).
/// - If n <= 0: returns empty dict (terminal for Seq).
pub(crate) fn builtin_take(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("take", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let n = materialize(&args[0], None, &ctx)?;
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

    let xs = materialize(&args[1], None, &ctx)?;
    // Bytes: treat as Seq of Int byte values
    let xs = match xs {
        Value::Bytes {
            ref source,
            start,
            end,
        } => bytes_to_seq(&source[start..end], call_span, &ctx),
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
                ok_val(Value::Int(n_int - 1), call_span)?,
                Rc::clone(&tail_thunk),
            ];
            let new_tail = Rc::new(Thunk::new_pending_builtin(
                builtin!("take", builtin_take),
                tail_args,
                None,
                call_span,
                Some(Rc::from("call $take")),
                Rc::clone(&ctx),
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
}

/// `drop`: Drop the first n elements from a Dict or Seq.
///
/// - For Dict: skip first n entries by position, return Dict with remaining entries
/// - For Seq: use lazy step function to drop elements one at a time
///
/// Args: (n, xs)
pub(crate) fn builtin_drop(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("drop", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let n = materialize(&args[0], None, &ctx)?;
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
        return Ok(Rc::clone(&args[1]));
    }

    let xs = materialize(&args[1], None, &ctx)?;
    // Bytes: treat as Seq of Int byte values
    let xs = match xs {
        Value::Bytes {
            ref source,
            start,
            end,
        } => bytes_to_seq(&source[start..end], call_span, &ctx),
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
            let n_minus_1 = Rc::new(Thunk::new_materialized(Value::Int(n_int - 1), call_span));
            let tail_thunk = ctx.get_thunk(tail);
            let step_args = vec![n_minus_1, Rc::clone(&tail_thunk)];
            Ok(Rc::new(Thunk::new_pending_builtin(
                builtin!("drop", builtin_drop_seq_step),
                step_args,
                None,
                call_span,
                Some(Rc::from("call $drop")),
                Rc::clone(&ctx),
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
}

/// Helper for `drop` on Seq: lazily drop elements one at a time.
///
/// Args: (n_remaining, seq)
pub(crate) fn builtin_drop_seq_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        call_span,
        ctx,
        ..
    } = ctx_arg;

    let n = materialize(&args[0], None, &ctx)?;
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
        return Ok(Rc::clone(&args[1]));
    }

    let seq = materialize(&args[1], None, &ctx)?;
    match seq {
        Value::Dict(_) => {
            // End of sequence before we finished dropping
            ok_val(Value::Dict(IndexMap::new()), call_span)
        }
        Value::Seq { head: _, tail } => {
            // Drop this element, continue with tail
            let n_minus_1 = Rc::new(Thunk::new_materialized(Value::Int(n_int - 1), call_span));
            let tail_thunk = ctx.get_thunk(tail);
            let step_args = vec![n_minus_1, Rc::clone(&tail_thunk)];
            Ok(Rc::new(Thunk::new_pending_builtin(
                builtin!("drop", builtin_drop_seq_step),
                step_args,
                None,
                call_span,
                Some(Rc::from("call $drop")),
                Rc::clone(&ctx),
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
}
