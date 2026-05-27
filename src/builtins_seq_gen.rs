//! Sequence generator builtins: `range`, `repeat`, `cycle`, `iterate`, `unfold`.
//!
//! These builtins produce potentially-infinite lazy sequences via `PendingBuiltin`
//! corecursion. Each tail is a deferred thunk, not an eagerly-evaluated list.
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration in `standard_builtins()` remains in `builtins.rs`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::{CoreExpr, Span, Spanned};
use crate::builtins::{builtin, ok_val, reject_named};
use crate::error::{ArityBound, EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{BuiltinArgs, Thunk, Value};

/// Helper: create a synthetic CoreExpr::Call for builtin-generated calls.
fn synthetic_call_expr(span: Span) -> Arc<Spanned<CoreExpr>> {
    Arc::new(Spanned {
        node: CoreExpr::Call {
            func: Arc::new(Spanned {
                node: CoreExpr::Int(0),
                span,
            }),
            args: vec![],
            named_args: vec![],
            implied: false,
        },
        span,
    })
}

/// `range`: Sequence of integers from start to end (exclusive), or infinite.
///
/// - `[call $range start]` → infinite Seq: start, start+1, start+2, ...
/// - `[call $range start end]` → finite Seq: start, start+1, ..., end-1
///   (empty if start >= end)
///
/// Both args must be Int. Uses checked_add for overflow detection.
pub(crate) fn builtin_range(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("range", named.as_ref(), call_span)?;
        if args.len() != 1 && args.len() != 2 {
            return Err(EvalError::arity_mismatch_bound(
                ArityBound::Range(1, 2),
                args.len(),
                call_span,
            )
            .into());
        }

        // args[0] is Seq-pre-materialized by pos_strictness[0] (public call) or passed as a
        // materialized Int thunk (recursive tail call). Both paths guarantee materialization.
        let start = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");
        let start_int = match start {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "range".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        if args.len() == 1 {
            // Infinite range: [start, start+1, start+2, ...]
            let next_start = start_int
                .checked_add(1)
                .ok_or_else(|| EvalError::integer_overflow("range".to_string(), call_span))?;
            let head = ok_val(Value::Int(start_int), call_span)?;
            let tail_args = vec![ok_val(Value::Int(next_start), call_span)?];
            let tail = Arc::new(Thunk::new_pending_builtin(
                builtin!("builtin-range", builtin_range),
                tail_args,
                None,
                call_span,
                Some(Arc::from("call $range")),
                Arc::clone(&ctx),
            ));
            let head_id = ctx.alloc_thunk(head);
            let tail_id = ctx.alloc_thunk(tail);
            ok_val(
                Value::Seq {
                    head: head_id,
                    tail: tail_id,
                },
                call_span,
            )
        } else {
            // Finite range: [start, start+1, ..., end-1]
            // Safe conditional: args.len() check (line 66) doesn't force thunks
            // SAFE: args[1] is always materialized in both dispatch paths:
            //   (1) CEK dispatch: pre-materialized by pos_strictness[1]=Seq
            //   (2) Inline recursive thunk (builtin!("range", builtin_range) below):
            //       tail_args are constructed via ok_val() → Thunk::new_materialized
            let end = args[1]
                .try_get_materialized()
                .expect("pre-materialized: pos_strictness[1]=Seq (CEK) or ok_val (inline thunk)");
            let end_int = match end {
                Value::Int(n) => n,
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "range".to_string(),
                        "Int",
                        other.type_name(),
                        call_span,
                    )
                    .into())
                }
            };

            if start_int >= end_int {
                // Empty range
                ok_val(Value::Dict(IndexMap::new()), call_span)
            } else {
                let next_start = start_int
                    .checked_add(1)
                    .ok_or_else(|| EvalError::integer_overflow("range".to_string(), call_span))?;
                let head = ok_val(Value::Int(start_int), call_span)?;
                let tail_args = vec![
                    ok_val(Value::Int(next_start), call_span)?,
                    ok_val(Value::Int(end_int), call_span)?,
                ];
                let tail = Arc::new(Thunk::new_pending_builtin(
                    builtin!("builtin-range", builtin_range),
                    tail_args,
                    None,
                    call_span,
                    Some(Arc::from("call $range")),
                    Arc::clone(&ctx),
                ));
                let head_id = ctx.alloc_thunk(head);
                let tail_id = ctx.alloc_thunk(tail);
                ok_val(
                    Value::Seq {
                        head: head_id,
                        tail: tail_id,
                    },
                    call_span,
                )
            }
        }
    })
}

/// `repeat`: Infinite sequence of a repeated value.
///
/// `[call $repeat val]` → infinite Seq: val, val, val, ...
///
/// The value is kept as a thunk (fully lazy — never materialized).
pub(crate) fn builtin_repeat(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        reject_named("repeat", named.as_ref(), call_span)?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        let head = Arc::clone(&args[0]);
        let tail_args = vec![Arc::clone(&args[0])];
        let tail = Arc::new(Thunk::new_pending_builtin(
            builtin!("builtin-repeat", builtin_repeat),
            tail_args,
            None,
            call_span,
            Some(Arc::from("call $repeat")),
            Arc::clone(&ctx),
        ));
        ok_val(
            Value::Seq {
                head: ctx.alloc_thunk(head),
                tail: ctx.alloc_thunk(tail),
            },
            call_span,
        )
    })
}

/// Internal helper for `cycle`: produces the next element in the cycle.
///
/// Takes (dict_thunk, index_thunk) where dict is the original collection to cycle
/// through and index is the current position (wrapped modulo length).
pub(crate) fn builtin_cycle_step(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("cycle_step", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let dict = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let map = match &dict {
            Value::Dict(m) => m,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "cycle".to_string(),
                    "Dict",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        let idx = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let idx_int = match idx {
            Value::Int(i) => i,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "cycle".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        if map.is_empty() {
            return Err(EvalError::empty_collection("cycle".to_string(), call_span).into());
        }

        let len = map.len() as i64;
        let current_idx = idx_int % len;
        let next_idx = (idx_int + 1) % len;

        // Get the value at current_idx
        let head_id = map
            .get_index(current_idx as usize)
            .map(|(_, v)| *v)
            .ok_or_else(|| {
                EvalError::internal("cycle: index out of bounds".to_string(), call_span)
            })?;

        // Create tail as PendingBuiltin for next step
        let tail_args = vec![
            Arc::clone(&args[0]),
            ok_val(Value::Int(next_idx), call_span)?,
        ];
        let tail = Arc::new(Thunk::new_pending_builtin(
            builtin!("builtin-cycle", builtin_cycle_step, [], 2),
            tail_args,
            None,
            call_span,
            Some(Arc::from("call $cycle")),
            Arc::clone(&ctx),
        ));

        ok_val(
            Value::Seq {
                head: head_id,
                tail: ctx.alloc_thunk(tail),
            },
            call_span,
        )
    })
}

/// `cycle`: Infinite sequence cycling through entries of a dict.
///
/// `[call $cycle xs]` → infinite Seq cycling through entries of xs by position.
///
/// Materializes xs to verify it's a non-empty Dict, then delegates to
/// `cycle_step` helper for lazy iteration.
pub(crate) fn builtin_cycle(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("cycle", named.as_ref(), call_span)?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        match val {
            Value::Dict(map) => {
                if map.is_empty() {
                    return Err(EvalError::empty_collection("cycle".to_string(), call_span).into());
                }
                drop(map); // ensure map (and thus val) is dropped before .await
                           // Start cycling from index 0
                builtin_cycle_step(BuiltinArgs {
                    args: vec![Arc::clone(&args[0]), ok_val(Value::Int(0), call_span)?],
                    named: None,
                    call_span,
                    ctx: Arc::clone(&ctx),
                })
                .await
            }
            other => Err(EvalError::type_mismatch_ctx(
                "cycle".to_string(),
                "Dict",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `iterate`: Infinite sequence of iterated function applications.
///
/// `[call $iterate $f $x]` → infinite Seq: x, f(x), f(f(x)), ...
///
/// Both f and x are kept as thunks (fully lazy). The tail contains a PendingCall
/// for f(x), wrapped in a PendingBuiltin for the next iterate step.
pub(crate) fn builtin_iterate(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("iterate", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let f = Arc::clone(&args[0]);
        let x = Arc::clone(&args[1]);

        // head = x (lazy)
        let head = Arc::clone(&x);

        // Create f(x) as PendingCall
        // Use stdlib env as caller_env since there's no lexical call site for builtin-internal calls
        let f_of_x = Arc::new(Thunk::new_pending_call(
            Arc::clone(&f),
            vec![Arc::clone(&x)],
            IndexMap::new(),
            call_span,
            Arc::clone(&ctx.config.stdlib_env),
            call_span,
            Some(Arc::from("iterate")),
            Arc::clone(&ctx),
            synthetic_call_expr(call_span),
        ));

        // tail = iterate(f, f(x))
        let tail_args = vec![Arc::clone(&f), f_of_x];
        let tail = Arc::new(Thunk::new_pending_builtin(
            builtin!("builtin-iterate", builtin_iterate),
            tail_args,
            None,
            call_span,
            Some(Arc::from("call $iterate")),
            Arc::clone(&ctx),
        ));

        ok_val(
            Value::Seq {
                head: ctx.alloc_thunk(head),
                tail: ctx.alloc_thunk(tail),
            },
            call_span,
        )
    })
}

/// Internal helper for `unfold`: performs one unfold step.
///
/// Takes (step_function, seed) and calls step(seed), which should return either:
/// - A 2-element dict [value next_seed] to continue
/// - An empty dict [] to terminate
pub(crate) fn builtin_unfold_step(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("unfold_step", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let step = Arc::clone(&args[0]);
        let seed = Arc::clone(&args[1]);

        // Call step(seed) as PendingCall, then materialize it
        let step_result_thunk = Arc::new(Thunk::new_pending_call(
            step.clone(),
            vec![seed],
            IndexMap::new(),
            call_span,
            Arc::clone(&ctx.config.stdlib_env),
            call_span,
            Some(Arc::from("unfold")),
            Arc::clone(&ctx),
            synthetic_call_expr(call_span),
        ));
        let step_result = materialize(&step_result_thunk, None, &ctx).await?;

        match step_result {
            Value::Dict(ref map) if map.is_empty() => {
                // Termination: return empty dict
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
            Value::Dict(ref map) if map.len() >= 2 => {
                // Extract first two values (ignore keys)
                let mut iter = map.values();
                let value_id = *iter.next().unwrap();
                let next_seed_id = *iter.next().unwrap();
                let next_seed = ctx.get_thunk(next_seed_id);

                // head = value (lazy)
                let head = value_id;

                // tail = unfold_step(step, next_seed)
                let tail_args = vec![step, Arc::clone(&next_seed)];
                let tail = Arc::new(Thunk::new_pending_builtin(
                    builtin!("builtin-unfold", builtin_unfold_step),
                    tail_args,
                    None,
                    call_span,
                    Some(Arc::from("call $unfold")),
                    Arc::clone(&ctx),
                ));

                ok_val(
                    Value::Seq {
                        head,
                        tail: ctx.alloc_thunk(tail),
                    },
                    call_span,
                )
            }
            Value::Dict(ref map) => Err(EvalError::type_mismatch_ctx(
                "unfold".to_string(),
                "Dict with at least 2 entries",
                &format!(
                    "Dict with {} {}",
                    map.len(),
                    if map.len() == 1 { "entry" } else { "entries" }
                ),
                call_span,
            )
            .into()),
            other => Err(EvalError::type_mismatch_ctx(
                "unfold".to_string(),
                "Dict",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `unfold`: Generate a sequence from a step function and seed.
///
/// `[call $unfold $step $seed]` → Seq where step(seed) returns [value next_seed]
/// or [] to stop.
///
/// Fully lazy — the step function is not called until the result is materialized.
pub(crate) fn builtin_unfold(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("unfold", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Return PendingBuiltin wrapping unfold_step — fully lazy
        let tail_args = vec![Arc::clone(&args[0]), Arc::clone(&args[1])];
        let result = Arc::new(Thunk::new_pending_builtin(
            builtin!("unfold", builtin_unfold_step),
            tail_args,
            None,
            call_span,
            Some(Arc::from("call $unfold")),
            Arc::clone(&ctx),
        ));
        Ok(result)
    })
}
