//! Sequence reduction builtins: `reduce`, `join`, `concat`.
//!
//! These builtins fold or combine Dict or Seq values. They follow the same
//! dual-dispatch pattern as the other sequence modules: materialize the
//! collection to dispatch on Dict vs Seq, then produce lazy or inherently-eager
//! results depending on the operation.
//!
//! - `reduce`: fully lazy (PendingCall chain for Dict, PendingBuiltin recursion for Seq)
//! - `join`: inherently eager (must stringify all elements)
//! - `concat`: lazy for Seq (PendingBuiltin step chain), eager for Dict (full merge)
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration in `standard_builtins()` and `create_root_env()` remains in `builtins.rs`.

use std::rc::Rc;

use indexmap::IndexMap;

use crate::builtins::{
    builtin, bytes_to_seq, flatten_overlay, ok_val, reject_named, require_string, stringify,
    MAX_COLLECT_SIZE, MAX_STRING_SIZE,
};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{string_val, BuiltinArgs, Key, Thunk, Value};

/// `reduce`: Fold a function over a Dict or Seq.
/// Inherently materializing: accumulator pattern requires sequential evaluation.
///
/// - For Dict: build a chain of PendingCall thunks, one per value
/// - For Seq: use recursive helper to build lazy chain
///
/// Args: (f, init, xs)
pub(crate) fn builtin_reduce(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
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

    let f_thunk = Rc::clone(&args[0]);
    let init_thunk = Rc::clone(&args[1]);
    let xs = materialize(&args[2], None, &ctx)?;

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
            // Dict path: build a chain of PendingCall thunks
            let mut acc = init_thunk;
            for (_key, value_thunk_id) in map.iter() {
                let value_thunk = ctx.get_thunk(*value_thunk_id);
                acc = Rc::new(Thunk::new_pending_call(
                    Rc::clone(&f_thunk),
                    vec![acc, Rc::clone(&value_thunk)],
                    IndexMap::new(),
                    call_span,
                    Rc::clone(&ctx.config.stdlib_env),
                    value_thunk.span,
                    Some(Rc::from("reduce")),
                    Rc::clone(&ctx),
                ));
            }
            Ok(acc)
        }
        Value::Seq { head, tail } => {
            // Seq path: use recursive step function
            let head_thunk = ctx.get_thunk(head);
            let tail_thunk = ctx.get_thunk(tail);
            let step_args = vec![
                Rc::clone(&f_thunk),
                init_thunk,
                Rc::clone(&head_thunk),
                Rc::clone(&tail_thunk),
            ];
            Ok(Rc::new(Thunk::new_pending_builtin(
                builtin!("reduce", builtin_reduce_seq_step),
                step_args,
                None,
                call_span,
                Some(Rc::from("call $reduce")),
                Rc::clone(&ctx),
            )))
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

/// Helper for `reduce` on Seq: process one element and recurse.
///
/// Args: (f, acc, head, tail)
pub(crate) fn builtin_reduce_seq_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("reduce_seq_step", named, call_span)?;
    if args.len() != 4 {
        return Err(EvalError::arity_mismatch(4, args.len(), call_span).into());
    }

    let f_thunk = Rc::clone(&args[0]);
    let acc_thunk = Rc::clone(&args[1]);
    let head_thunk = Rc::clone(&args[2]);
    let tail_thunk = Rc::clone(&args[3]);

    // Create new accumulator: f(acc, head)
    let new_acc = Rc::new(Thunk::new_pending_call(
        Rc::clone(&f_thunk),
        vec![acc_thunk, head_thunk],
        IndexMap::new(),
        call_span,
        Rc::clone(&ctx.config.stdlib_env),
        tail_thunk.span,
        Some(Rc::from("reduce")),
        Rc::clone(&ctx),
    ));

    // Check if tail is empty (sequence end)
    let tail_val = materialize(&tail_thunk, None, &ctx)?;
    match tail_val {
        Value::Dict(_) => {
            // Empty dict = end of sequence, return accumulator
            Ok(new_acc)
        }
        Value::Seq { head, tail } => {
            // Continue reducing
            let head_thunk = ctx.get_thunk(head);
            let tail_thunk = ctx.get_thunk(tail);
            let step_args = vec![
                Rc::clone(&f_thunk),
                new_acc,
                Rc::clone(&head_thunk),
                Rc::clone(&tail_thunk),
            ];
            Ok(Rc::new(Thunk::new_pending_builtin(
                builtin!("reduce", builtin_reduce_seq_step),
                step_args,
                None,
                call_span,
                Some(Rc::from("call $reduce")),
                Rc::clone(&ctx),
            )))
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
pub(crate) fn builtin_join(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
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

    let sep = materialize(&args[0], None, &ctx)?;
    let sep_str = require_string("join", sep, args[0].span)?;

    let xs = materialize(&args[1], None, &ctx)?;
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
pub(crate) fn builtin_concat(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
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
    let xs = materialize(&args[0], None, &ctx)?;
    let ys_thunk = Rc::clone(&args[1]);
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
            let step_args = vec![Rc::clone(&tail_thunk), ys_thunk];
            let result_thunk = Rc::new(Thunk::new_pending_builtin(
                builtin!("concat", builtin_concat_seq_step),
                step_args,
                None,
                call_span,
                Some(Rc::from("call $concat")),
                Rc::clone(&ctx),
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
pub(crate) fn builtin_concat_seq_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    let xs_tail_thunk = Rc::clone(&args[0]);
    let ys_thunk = Rc::clone(&args[1]);
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
            let step_args = vec![Rc::clone(&tail_thunk), ys_thunk];
            let new_tail = Rc::new(Thunk::new_pending_builtin(
                builtin!("concat", builtin_concat_seq_step),
                step_args,
                None,
                call_span,
                Some(Rc::from("call $concat")),
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
            "concat".to_string(),
            "Dict or Seq",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}
