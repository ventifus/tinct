//! Async concurrency primitives: task, await, channel, send, recv, select-once,
//! broadcast-channel, oneshot-channel, try-send, par, par-map, par-filter, and cancellation
//! context primitives (context, with-cancel, with-timeout, with-deadline, cancelled?, cancel-task).
//!
//! Design notes (from doc/whatif/async-eval.md):
//! - `task` spawns a concurrent evaluation via tokio::task::spawn_local
//! - `await` blocks until the task completes, returns its result
//! - `channel N` creates a bounded channel with capacity N (minimum 1)
//! - `send chan value` sends a value on the channel (suspends if full)
//! - `recv chan` receives a value from the channel (suspends until available)
//! - `select-once context sources` waits for first channel to fire, calls its handler
//! - `broadcast-channel N` creates a broadcast channel with capacity N
//! - `oneshot-channel` creates a one-shot channel, returns [receiver sender]
//! - `try-send chan value` sends without blocking; returns [Ok] or [Full] or [Closed]
//! - `par expr` eagerly spawns a task (hint for parallel evaluation)
//! - `par-map fn seq` applies fn to each element concurrently
//! - `par-filter pred seq` filters sequence in parallel
//!
//! Cancellation context primitives (see §Cancellation and Contexts in async-eval.md):
//! - `context` → Context — creates a root cancellation context (fresh CancellationToken)
//! - `with-cancel ctx` → [child-ctx cancel-fn] — child context + explicit cancel function
//! - `with-timeout ctx ms` → child-ctx — auto-cancels after `ms` milliseconds
//! - `with-deadline ctx unix-ms` → child-ctx — auto-cancels at absolute Unix timestamp (ms)
//! - `cancelled? ctx` → Bool — true if the context's token is cancelled
//! - `cancel-task ctx` → Null — explicitly cancel the context (and all its children)
//!
//! Current implementation: real tokio::sync::mpsc channels and tokio::task::spawn_local tasks.
//! Value::Context(CancellationToken) is fully implemented.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::ok_val;
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::eval_core::eval_core_expr;
use crate::value::{BuiltinArgs, ClockCapInner, HashableValue, Thunk, Value};

/// Helper to check argument count and extract first argument as a thunk.
/// Returns the thunk without materializing it. Named `take_one_thunk` to
/// distinguish from `builtins::expect_one_arg` which forces and returns a `Value`.
fn take_one_thunk(
    name: &str,
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    call_span: Span,
    _ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    if !named.as_ref().is_none_or(|n| n.is_empty()) {
        return Err(EvalError::user_error(
            format!("{name} does not accept named arguments"),
            call_span,
        )
        .into());
    }
    if args.len() != 1 {
        return Err(EvalError::user_error(
            format!("{name} expects 1 argument, got {}", args.len()),
            call_span,
        )
        .into());
    }
    Ok(Arc::clone(&args[0]))
}

/// Helper to check argument count for two arguments and extract them as thunks.
/// Returns both thunks without materializing them. Named `take_two_thunks` to
/// distinguish from `builtins::expect_one_arg` which forces and returns a `Value`.
fn take_two_thunks(
    name: &str,
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    call_span: Span,
    _ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<(Arc<Thunk>, Arc<Thunk>)> {
    if !named.as_ref().is_none_or(|n| n.is_empty()) {
        return Err(EvalError::user_error(
            format!("{name} does not accept named arguments"),
            call_span,
        )
        .into());
    }
    if args.len() != 2 {
        return Err(EvalError::user_error(
            format!("{name} expects 2 arguments, got {}", args.len()),
            call_span,
        )
        .into());
    }
    Ok((Arc::clone(&args[0]), Arc::clone(&args[1])))
}

fn take_three_thunks(
    name: &str,
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    call_span: Span,
    _ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<(Arc<Thunk>, Arc<Thunk>, Arc<Thunk>)> {
    if !named.as_ref().is_none_or(|n| n.is_empty()) {
        return Err(EvalError::user_error(
            format!("{name} does not accept named arguments"),
            call_span,
        )
        .into());
    }
    if args.len() != 3 {
        return Err(EvalError::user_error(
            format!("{name} expects 3 arguments, got {}", args.len()),
            call_span,
        )
        .into());
    }
    Ok((
        Arc::clone(&args[0]),
        Arc::clone(&args[1]),
        Arc::clone(&args[2]),
    ))
}

/// Helper to collect a Seq into a Vec<Arc<Thunk>> by walking the linked list.
/// Returns the thunks in order. Materializes each tail to check for continuation or termination.
/// Collect elements from an integer-keyed Dict into a Vec of Arc<Thunk>.
/// Used by par-map, par-filter, and select-once which now accept Dict input.
async fn collect_dict_to_vec(
    dict_val: Value,
    _ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
    _name: &str,
) -> EvalResult<Vec<Arc<Thunk>>> {
    let map = match dict_val {
        Value::Dict(m) => m,
        other => return Err(EvalError::type_mismatch("Dict", other.type_name(), call_span).into()),
    };

    Ok(map.into_values().collect())
}

/// Helper to build an integer-keyed Dict from a Vec<Arc<Thunk>>.
/// Returns an Arc<Thunk> wrapping the resulting Dict value.
fn build_dict_from_vec(
    items: Vec<Arc<Thunk>>,
    _ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
) -> Arc<Thunk> {
    let mut dict: indexmap::IndexMap<crate::value::HashableValue, Arc<Thunk>> =
        indexmap::IndexMap::new();
    for (i, thunk) in items.into_iter().enumerate() {
        dict.insert(crate::value::HashableValue::Int(i as i64), thunk);
    }
    Arc::new(Thunk::value(Value::Dict(dict), call_span))
}

/// `task`: Spawn a concurrent evaluation.
///
/// Signature: `expr → Task@T`
///
/// The argument is evaluated concurrently via tokio::task::spawn_local. If the
/// result is a zero-arg Function or Builtin, it is called and its return value
/// becomes the task result. Any other materialized value (e.g. `[task 42]`)
/// is returned directly as the task result — the spec signature `expr → Task@T`
/// allows any expression, not just zero-arg functions.
///
/// Per runtime-v2.md: `spawn_local` fires when the `task` expression itself is
/// materialized — not when `await` demands the handle. An undemanded task thunk
/// is never spawned.
pub(crate) fn builtin_task(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let func_thunk = take_one_thunk("task", &args, named.as_ref(), call_span.clone(), &ctx)?;

        // Clone what we need for the 'static async block
        let ctx_clone = Arc::clone(&ctx);
        let call_span_clone = call_span.clone();
        let func_thunk_clone = Arc::clone(&func_thunk);

        // Spawn the task using spawn_local
        let handle = crate::async_rt::spawn_local(async move {
            // Materialize the function
            let func_value =
                materialize(&func_thunk_clone, Some(&call_span_clone), &ctx_clone).await?;

            // Evaluate the function call
            match func_value {
                Value::Function {
                    params,
                    body,
                    closure_env,
                    annotation: _,
                    ..
                } => {
                    // Check for zero-arg function
                    if !params.is_empty() {
                        return Err(EvalError::user_error(
                            format!(
                                "task expects a zero-arg function, got {} parameter(s)",
                                params.len()
                            ),
                            call_span_clone,
                        )
                        .into());
                    }
                    let thunk = eval_core_expr(
                        &body,
                        &crate::value::EvalFrame::for_function_call(
                            Arc::clone(&closure_env),
                            vec![],
                        ),
                        &ctx_clone,
                    )
                    .await?;
                    materialize(&thunk, None, &ctx_clone).await
                }
                Value::Builtin(def) => {
                    // Call the builtin with no arguments
                    let result = (def.func)(BuiltinArgs {
                        args: vec![],
                        named: None,
                        call_span: call_span_clone,
                        caller_env_id: None,
                        ctx: Arc::clone(&ctx_clone),
                    })
                    .await?;
                    // Materialize the result
                    materialize(&result, None, &ctx_clone).await
                }
                // Any other materialized value: return it directly as the task result.
                // The spec signature is `expr → Task@T`, so `[task 42]` is valid and
                // immediately resolves to 42.
                other => Ok(other),
            }
        });

        // Return a Task value wrapping the JoinHandle
        ok_val(
            Value::Task(Arc::new(tokio::sync::Mutex::new(
                crate::value::TaskState::Pending(handle),
            ))),
            call_span,
        )
    })
}

/// `await`: Block until a task completes and return its result.
///
/// Signature: `Task@T → T`
///
/// Suspends the caller until the task finishes, then returns the task's result.
/// Propagates any error from the task.
pub(crate) fn builtin_await(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let task_thunk = take_one_thunk("await", &args, named.as_ref(), call_span.clone(), &ctx)?;
        let task_val = materialize(&task_thunk, Some(&call_span), &ctx).await?;

        match task_val {
            Value::Task(state_mutex) => {
                // Lock the TaskState mutex
                let mut guard = state_mutex.lock().await;

                // The Tokio async mutex is held across the .await below, which serializes
                // concurrent callers: only one task can be inside this block at a time.
                // While the first awaiter holds the lock and awaits the JoinHandle, any
                // second caller blocks at `state_mutex.lock().await` and does not observe
                // the temporary sentinel placed by mem::replace. When the first awaiter
                // completes, it stores the real result in Done(Ok(result)) before releasing
                // the lock, so the second caller then reads the correct cached value.
                //
                // The Done(result) match arm restores the result to the guard BEFORE
                // extracting with `?` to ensure subsequent awaits see the same result,
                // whether success or error.
                match std::mem::replace(
                    &mut *guard,
                    crate::value::TaskState::Done(Ok(Value::Dict(IndexMap::new()))),
                ) {
                    crate::value::TaskState::Pending(handle) => {
                        // Await the handle (JoinHandle<EvalResult<Value>>), racing against
                        // context cancellation.
                        let val_result: EvalResult<Value> = tokio::select! {
                            join_result = handle => {
                                match join_result {
                                    Ok(inner) => inner,
                                    Err(e) => Err(EvalError::user_error(
                                        format!("task panicked: {e}"),
                                        call_span.clone(),
                                    ).into()),
                                }
                            }
                            _ = ctx.cancel.cancelled() => {
                                // Cache cancellation error so subsequent awaits see it too
                                let err: Box<EvalError> = EvalError::user_error(
                                    "await: cancelled".to_string(), call_span.clone()
                                ).into();
                                *guard = crate::value::TaskState::Done(Err(err.clone()));
                                return Err(err);
                            }
                        };

                        // ALWAYS cache the real result (Ok or Err) before returning.
                        // This prevents the sentinel Done(Ok({})) from being seen by
                        // subsequent awaits when the first await fails.
                        *guard = crate::value::TaskState::Done(val_result.clone());

                        // Return the result
                        let val = val_result?;
                        ok_val(val, call_span)
                    }
                    crate::value::TaskState::Done(result) => {
                        // Task already completed — restore the result before extracting.
                        // This ensures subsequent awaits see the cached result, not the sentinel.
                        *guard = crate::value::TaskState::Done(result.clone());
                        let val = result?;
                        ok_val(val, call_span)
                    }
                }
            }
            _ => Err(EvalError::type_mismatch("Task", task_val.type_name(), call_span).into()),
        }
    })
}

/// `channel`: Create a bounded channel with the specified capacity.
///
/// Signature: `Int → Channel@T`
///
/// The capacity must be ≥ 1. `[channel 0]` is a runtime error.
pub(crate) fn builtin_channel(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let capacity_thunk =
            take_one_thunk("channel", &args, named.as_ref(), call_span.clone(), &ctx)?;
        let capacity_val = materialize(&capacity_thunk, Some(&call_span), &ctx).await?;

        match capacity_val {
            Value::Int(n) if n >= 1 => {
                // Create the channel
                let (tx, rx) = tokio::sync::mpsc::channel(n as usize);
                let channel_inner = crate::value::ChannelInner {
                    sender: tx,
                    receiver: tokio::sync::Mutex::new(rx),
                    capacity: n,
                };
                ok_val(Value::Channel(Arc::new(channel_inner)), call_span)
            }
            Value::Int(n) if n < 1 => Err(EvalError::user_error(
                format!("channel capacity must be ≥ 1, got {n}"),
                call_span,
            )
            .into()),
            _ => Err(EvalError::type_mismatch("Int", capacity_val.type_name(), call_span).into()),
        }
    })
}

/// `send`: Send a value on a channel.
///
/// Signature: `Channel@T → T → Null`
///
/// Suspends if the channel buffer is full. Returns null on success.
pub(crate) fn builtin_send(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let (chan_thunk, val_thunk) =
            take_two_thunks("send", &args, named.as_ref(), call_span.clone(), &ctx)?;
        let chan_val = materialize(&chan_thunk, Some(&call_span), &ctx).await?;

        match chan_val {
            Value::Channel(channel_inner) => {
                // Materialize the value to send
                let value = materialize(&val_thunk, Some(&call_span), &ctx).await?;

                // Send the value, racing against context cancellation.
                tokio::select! {
                    result = channel_inner.sender.send(value) => {
                        result.map_err(|_| {
                            EvalError::user_error(
                                "channel closed (receiver dropped)".to_string(),
                                call_span.clone(),
                            )
                        })?;
                    }
                    _ = ctx.cancel.cancelled() => {
                        return Err(EvalError::user_error(
                            "send: cancelled".to_string(),
                            call_span.clone(),
                        ).into());
                    }
                }

                // Return null (empty dict)
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
            Value::BroadcastChannel(channel_inner) => {
                // Materialize the value to send
                let value = materialize(&val_thunk, Some(&call_span), &ctx).await?;

                // Send to all subscribers. broadcast::send doesn't block.
                channel_inner.sender.send(value).map_err(|_| {
                    EvalError::user_error(
                        "broadcast channel closed (all receivers dropped)".to_string(),
                        call_span.clone(),
                    )
                })?;

                // Return null (empty dict)
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
            Value::OneshotSender(sender_inner) => {
                // Materialize the value to send
                let value = materialize(&val_thunk, Some(&call_span), &ctx).await?;

                // Take the sender (single-use)
                let mut tx_opt = sender_inner.sender.lock().await;
                let tx = tx_opt.take().ok_or_else(|| {
                    EvalError::user_error(
                        "oneshot sender already used".to_string(),
                        call_span.clone(),
                    )
                })?;

                // Send the value. oneshot::send is non-blocking.
                tx.send(value).map_err(|_| {
                    EvalError::user_error(
                        "oneshot receiver dropped before send".to_string(),
                        call_span.clone(),
                    )
                })?;

                // Return null (empty dict)
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
            _ => Err(EvalError::type_mismatch(
                "Channel, BroadcastChannel, or OneshotSender",
                chan_val.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `recv`: Receive a value from a channel.
///
/// Signature: `Channel@T → T | [Closed]`
///
/// Suspends until a value is available. Returns the value directly on success, `[Closed]` if the
/// channel is closed (sender dropped). Context cancellation still raises an exception.
pub(crate) fn builtin_recv(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let chan_thunk = take_one_thunk("recv", &args, named.as_ref(), call_span.clone(), &ctx)?;
        let chan_val = materialize(&chan_thunk, Some(&call_span), &ctx).await?;

        match chan_val {
            Value::Channel(channel_inner) => {
                // Lock the receiver
                let mut rx = channel_inner.receiver.lock().await;

                // Receive a value, racing against context cancellation.
                let result = tokio::select! {
                    result = rx.recv() => result,
                    _ = ctx.cancel.cancelled() => {
                        return Err(EvalError::user_error(
                            "recv: cancelled".to_string(),
                            call_span.clone(),
                        ).into());
                    }
                };

                // Return value directly on success, [Closed] if channel closed
                match result {
                    Some(value) => ok_val(value, call_span),
                    None => {
                        // Channel closed
                        ok_val(
                            Value::Variant {
                                tycon: Arc::from("Closed"),
                                ctor: Arc::from("Closed"),
                                payload: None,
                            },
                            call_span,
                        )
                    }
                }
            }
            Value::BroadcastChannel(channel_inner) => {
                // Create a new subscriber
                let mut rx = channel_inner.sender.subscribe();

                // Receive a value, racing against context cancellation.
                let result = tokio::select! {
                    result = rx.recv() => result,
                    _ = ctx.cancel.cancelled() => {
                        return Err(EvalError::user_error(
                            "recv: cancelled".to_string(),
                            call_span.clone(),
                        ).into());
                    }
                };

                // Return value directly on success, [Closed] or [Lagged n] on recv error
                match result {
                    Ok(value) => ok_val(value, call_span),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Subscriber too slow, missed n messages
                        let count_thunk =
                            Arc::new(Thunk::value(Value::Int(n as i64), call_span.clone()));
                        ok_val(
                            Value::Variant {
                                tycon: Arc::from("Lagged"),
                                ctor: Arc::from("Lagged"),
                                payload: Some(count_thunk),
                            },
                            call_span,
                        )
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Channel closed
                        ok_val(
                            Value::Variant {
                                tycon: Arc::from("Closed"),
                                ctor: Arc::from("Closed"),
                                payload: None,
                            },
                            call_span,
                        )
                    }
                }
            }
            Value::OneshotReceiver(receiver_inner) => {
                // Take the receiver (single-use)
                let mut rx_opt = receiver_inner.receiver.lock().await;
                let rx = rx_opt.take().ok_or_else(|| {
                    EvalError::user_error(
                        "oneshot receiver already used".to_string(),
                        call_span.clone(),
                    )
                })?;

                // Receive the single value, racing against context cancellation.
                let result = tokio::select! {
                    result = rx => result,
                    _ = ctx.cancel.cancelled() => {
                        return Err(EvalError::user_error(
                            "recv: cancelled".to_string(),
                            call_span.clone(),
                        ).into());
                    }
                };

                // Return value directly on success, [Closed] if sender dropped
                match result {
                    Ok(value) => ok_val(value, call_span),
                    Err(_) => {
                        // Sender dropped before sending
                        ok_val(
                            Value::Variant {
                                tycon: Arc::from("Closed"),
                                ctor: Arc::from("Closed"),
                                payload: None,
                            },
                            call_span,
                        )
                    }
                }
            }
            _ => Err(EvalError::type_mismatch(
                "Channel, BroadcastChannel, or OneshotReceiver",
                chan_val.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `broadcast-channel`: Create a broadcast channel where each subscriber receives all values.
///
/// Signature: `Int → BroadcastChannel`
///
/// Uses tokio::sync::broadcast::channel(capacity). Returns a BroadcastChannel value
/// that can be passed to `recv` (each subscriber gets every value sent).
/// Multiple subscribers can each call `recv` on the same channel.
///
/// SUBSCRIPTION SEMANTICS: Each call to `recv` on a BroadcastChannel creates a new
/// subscriber via `tokio::sync::broadcast::Sender::subscribe()`. Tokio broadcast
/// receivers only receive messages sent *after* `subscribe()` is called. This means
/// values sent before the first `recv` call are not visible to that receiver:
///
///   ch: [broadcast-channel 10]
///   _:  [send ch 42]   # sent before any subscriber — lost
///   v:  [recv ch]      # subscribes NOW — misses 42, blocks forever
///
/// This is expected tokio::sync::broadcast semantics. To use broadcast-channel
/// correctly, ensure subscribers (`recv` calls) are established before senders
/// produce values — typically by spawning the receiver task first.
pub(crate) fn builtin_broadcast_channel(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let capacity_thunk = take_one_thunk(
            "broadcast-channel",
            &args,
            named.as_ref(),
            call_span.clone(),
            &ctx,
        )?;
        let capacity_val = materialize(&capacity_thunk, Some(&call_span), &ctx).await?;

        match capacity_val {
            Value::Int(n) if n >= 1 => {
                // Create the broadcast channel
                let (tx, _rx) = tokio::sync::broadcast::channel(n as usize);
                let channel_inner = crate::value::BroadcastChannelInner {
                    sender: tx,
                    capacity: n,
                };
                ok_val(Value::BroadcastChannel(Arc::new(channel_inner)), call_span)
            }
            Value::Int(n) if n < 1 => Err(EvalError::user_error(
                format!("broadcast-channel capacity must be ≥ 1, got {n}"),
                call_span,
            )
            .into()),
            _ => Err(EvalError::type_mismatch("Int", capacity_val.type_name(), call_span).into()),
        }
    })
}

/// `oneshot-channel`: Create a oneshot channel for single-value request/response patterns.
///
/// Signature: `→ [Seq Channel Channel]`
///
/// Uses tokio::sync::oneshot::channel(). Returns a 2-element Seq: [receiver, sender].
/// Exactly one value can be sent on the sender channel; subsequent sends fail.
/// The receiver can recv exactly once; subsequent recvs fail.
pub(crate) fn builtin_oneshot_channel(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx: _ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        if !named.as_ref().is_none_or(|n| n.is_empty()) {
            return Err(EvalError::user_error(
                "oneshot-channel does not accept named arguments".to_string(),
                call_span,
            )
            .into());
        }
        if !args.is_empty() {
            return Err(EvalError::user_error(
                format!("oneshot-channel expects 0 arguments, got {}", args.len()),
                call_span,
            )
            .into());
        }

        // Create the oneshot channel
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sender_inner = crate::value::OneshotSenderInner {
            sender: tokio::sync::Mutex::new(Some(tx)),
        };
        let receiver_inner = crate::value::OneshotReceiverInner {
            receiver: tokio::sync::Mutex::new(Some(rx)),
        };

        // Return {0: receiver, 1: sender}
        let sender_thunk = Arc::new(Thunk::value(
            Value::OneshotSender(Arc::new(sender_inner)),
            call_span.clone(),
        ));
        let receiver_thunk = Arc::new(Thunk::value(
            Value::OneshotReceiver(Arc::new(receiver_inner)),
            call_span.clone(),
        ));
        let mut dict = indexmap::IndexMap::new();
        dict.insert(HashableValue::Int(0), receiver_thunk);
        dict.insert(HashableValue::Int(1), sender_thunk);
        ok_val(Value::Dict(dict), call_span)
    })
}

/// `try-send`: Non-blocking send. Returns empty dict on success, [Full] if channel is at capacity.
///
/// Signature: `Channel@T → T → {} | [Full] | [Closed]`
///
/// Uses mpsc::try_send. Never suspends. If the channel buffer is full, returns [Full]
/// and the value is dropped. Returns [Closed] if the receiver has been dropped.
pub(crate) fn builtin_try_send(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let (chan_thunk, val_thunk) =
            take_two_thunks("try-send", &args, named.as_ref(), call_span.clone(), &ctx)?;
        let chan_val = materialize(&chan_thunk, Some(&call_span), &ctx).await?;

        match chan_val {
            Value::Channel(channel_inner) => {
                // Materialize the value to send
                let value = materialize(&val_thunk, Some(&call_span), &ctx).await?;

                // Try to send the value
                match channel_inner.sender.try_send(value) {
                    Ok(_) => {
                        // Success: return empty dict (unit value)
                        ok_val(Value::Dict(IndexMap::new()), call_span)
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        // Channel full: return [Full]
                        ok_val(
                            Value::Variant {
                                tycon: Arc::from("Full"),
                                ctor: Arc::from("Full"),
                                payload: None,
                            },
                            call_span,
                        )
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        // Channel closed: return [Closed] variant, consistent with recv/select-once
                        ok_val(
                            Value::Variant {
                                tycon: Arc::from("Closed"),
                                ctor: Arc::from("Closed"),
                                payload: None,
                            },
                            call_span,
                        )
                    }
                }
            }
            _ => Err(EvalError::type_mismatch("Channel", chan_val.type_name(), call_span).into()),
        }
    })
}

/// `select-once`: Wait for the first of multiple sources to complete.
///
/// Signature: `Context → [Seq {ch: Channel|BroadcastChannel|OneshotReceiver  handler: Fn}] → T | [Closed]`
///
/// Takes a context (for cancellation checking) and a sequence of source dicts, where each
/// source has a `ch:` field (Channel, BroadcastChannel, or OneshotReceiver) and a `handler:` field (Fn).
/// Waits for the FIRST channel to have a value available, then calls that channel's handler
/// with the received value. Returns the handler result directly on success; `Closed.Closed` if
/// all channels are closed (not an error). Context cancellation still raises an exception.
///
/// Channel type semantics:
/// - Channel (mpsc): Standard FIFO channel, each value consumed once
/// - BroadcastChannel: Creates a subscriber at select-once start; receives values sent after subscription
/// - OneshotReceiver: Single-use receiver; after receiving one value, marked as closed
///
/// Implementation note: uses a manual polling loop over all channels. When a channel
/// produces a value, we call its handler and return. Closed channels are removed from
/// consideration. If all channels are closed, returns `Closed.Closed`.
///
/// Fairness: channels are checked in order, but since this is a cooperative runtime,
/// fairness emerges naturally from the event loop.
pub(crate) fn builtin_select_once(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named: _,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        // First arg: context (for cancellation checking)
        if args.is_empty() {
            return Err(EvalError::user_error(
                "select-once requires a context argument".to_string(),
                call_span,
            )
            .into());
        }

        // Second arg: sources
        if args.len() < 2 {
            return Err(EvalError::user_error(
                "select-once requires sources as second argument".to_string(),
                call_span,
            )
            .into());
        }

        // Materialize and validate the context argument.
        let user_ctx_thunk = Arc::clone(&args[0]);
        let user_ctx_val = materialize(&user_ctx_thunk, Some(&call_span), &ctx).await?;
        let user_token = match user_ctx_val {
            Value::Context(token) => token,
            other => {
                return Err(
                    EvalError::type_mismatch("Context", other.type_name(), call_span).into(),
                )
            }
        };

        let sources_thunk = Arc::clone(&args[1]);
        let sources_val = materialize(&sources_thunk, Some(&call_span), &ctx).await?;

        // Collect the sequence of sources into a vec
        let source_ids =
            collect_dict_to_vec(sources_val, &ctx, call_span.clone(), "select-once").await?;

        if source_ids.is_empty() {
            return Err(EvalError::user_error(
                "select-once requires at least one source".to_string(),
                call_span,
            )
            .into());
        }

        // Define the channel source enum to support all three channel types.
        // For BroadcastChannel, we create a subscriber immediately so it sees messages
        // sent during the select operation.
        enum ChannelSource {
            Channel(Arc<crate::value::ChannelInner>),
            BroadcastChannel(tokio::sync::broadcast::Receiver<Value>),
            OneshotReceiver(Arc<crate::value::OneshotReceiverInner>),
        }

        // Parse each source as a {ch:, handler:} Dict
        let mut sources: Vec<(ChannelSource, Arc<Thunk>)> = Vec::with_capacity(source_ids.len());
        for source_thunk in source_ids {
            let source_val = materialize(&source_thunk, Some(&call_span), &ctx).await?;

            match source_val {
                Value::Dict(ref map) => {
                    // Validate that the dict has the required `ch:` and `handler:` keys.
                    let ch_thunk_opt = map.get(&HashableValue::Str("ch".into())).cloned();
                    let handler_thunk_opt = map.get(&HashableValue::Str("handler".into())).cloned();
                    match (ch_thunk_opt, handler_thunk_opt) {
                        (Some(ch_thunk_val), Some(handler_id)) => {
                            let ch_val = materialize(&ch_thunk_val, Some(&call_span), &ctx).await?;
                            match ch_val {
                                Value::Channel(ch) => {
                                    sources.push((ChannelSource::Channel(ch), handler_id));
                                }
                                Value::BroadcastChannel(ch) => {
                                    // Subscribe immediately so we see messages sent during select
                                    let rx = ch.sender.subscribe();
                                    sources.push((ChannelSource::BroadcastChannel(rx), handler_id));
                                }
                                Value::OneshotReceiver(ch) => {
                                    sources.push((ChannelSource::OneshotReceiver(ch), handler_id));
                                }
                                _ => {
                                    return Err(EvalError::type_mismatch(
                                        "Channel, BroadcastChannel, or OneshotReceiver",
                                        ch_val.type_name(),
                                        call_span,
                                    )
                                    .into())
                                }
                            }
                        }
                        _ => {
                            return Err(EvalError::type_mismatch(
                                "Dict with ch: and handler: fields",
                                "Dict",
                                call_span,
                            )
                            .into());
                        }
                    }
                }
                other => {
                    return Err(EvalError::type_mismatch(
                        "Dict with ch: and handler: fields",
                        other.type_name(),
                        call_span,
                    )
                    .into())
                }
            }
        }

        // Track which channels are closed (TryRecvError::Disconnected).
        // When all channels are closed, return [Closed].
        let sources_len = sources.len();
        let mut closed = vec![false; sources_len];
        // O(1) all-closed check: increment on each close, compare to sources_len.
        let mut closed_count: usize = 0;

        // Poll all channels in a loop until one produces a value.
        // Impose a maximum iteration limit to prevent infinite busy-poll loops
        // when all channels are empty (e.g., due to lazy evaluation not forcing
        // send operations before select-once runs).
        const MAX_SELECT_POLLS: usize = 10_000;
        let mut poll_count = 0;

        loop {
            poll_count += 1;
            if poll_count > MAX_SELECT_POLLS {
                return Err(EvalError::resource_limit_exceeded(
                    "select-once: channel poll limit exceeded — no channel was ready".to_string(),
                    call_span,
                )
                .into());
            }

            for (i, (channel_source, handler_id)) in sources.iter_mut().enumerate() {
                if closed[i] {
                    continue;
                }

                // Try to receive from the channel based on its type
                let recv_result: Result<Value, ()> = match channel_source {
                    ChannelSource::Channel(channel_inner) => {
                        // Use try_lock() to avoid blocking on contended receivers.
                        // If the lock is held by a concurrent `recv`, skip this channel
                        // this iteration — we will retry after yield_now().
                        let mut rx = match channel_inner.receiver.try_lock() {
                            Ok(guard) => guard,
                            Err(_) => continue, // lock contended, try next channel
                        };

                        match rx.try_recv() {
                            Ok(value) => Ok(value),
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Err(()),
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                // Channel is closed
                                if !closed[i] {
                                    closed[i] = true;
                                    closed_count += 1;
                                }
                                if closed_count == sources_len {
                                    // All channels closed — return [Closed]
                                    return ok_val(
                                        Value::Variant {
                                            tycon: Arc::from("Closed"),
                                            ctor: Arc::from("Closed"),
                                            payload: None,
                                        },
                                        call_span,
                                    );
                                }
                                continue;
                            }
                        }
                    }
                    ChannelSource::BroadcastChannel(rx) => {
                        // We have a persistent subscriber created at the start.
                        // Try to receive from it.
                        match rx.try_recv() {
                            Ok(value) => Ok(value),
                            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => Err(()),
                            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                                // Subscriber lagged — treat as empty and continue
                                Err(())
                            }
                            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                                // Channel is closed
                                if !closed[i] {
                                    closed[i] = true;
                                    closed_count += 1;
                                }
                                if closed_count == sources_len {
                                    // All channels closed — return [Closed]
                                    return ok_val(
                                        Value::Variant {
                                            tycon: Arc::from("Closed"),
                                            ctor: Arc::from("Closed"),
                                            payload: None,
                                        },
                                        call_span,
                                    );
                                }
                                continue;
                            }
                        }
                    }
                    ChannelSource::OneshotReceiver(receiver_inner) => {
                        // Try to take the receiver (single-use)
                        let mut rx_opt = match receiver_inner.receiver.try_lock() {
                            Ok(guard) => guard,
                            Err(_) => continue, // lock contended, try next channel
                        };

                        // Check if the receiver has already been consumed
                        if rx_opt.is_none() {
                            // Receiver already used — mark as closed
                            if !closed[i] {
                                closed[i] = true;
                                closed_count += 1;
                            }
                            if closed_count == sources_len {
                                // All channels closed — return [Closed]
                                return ok_val(
                                    Value::Variant {
                                        tycon: Arc::from("Closed"),
                                        ctor: Arc::from("Closed"),
                                        payload: None,
                                    },
                                    call_span,
                                );
                            }
                            continue;
                        }

                        // Try to receive without consuming the receiver yet.
                        // We need to use try_recv which consumes, so we must be careful.
                        let mut rx = rx_opt.take().unwrap();
                        match rx.try_recv() {
                            Ok(value) => {
                                // Got a value! Receiver is now consumed (already taken).
                                Ok(value)
                            }
                            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                                // No value available yet — put the receiver back and retry.
                                *rx_opt = Some(rx);
                                Err(())
                            }
                            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                                // Sender dropped without sending — channel is closed.
                                if !closed[i] {
                                    closed[i] = true;
                                    closed_count += 1;
                                }
                                if closed_count == sources_len {
                                    // All channels closed — return [Closed]
                                    return ok_val(
                                        Value::Variant {
                                            tycon: Arc::from("Closed"),
                                            ctor: Arc::from("Closed"),
                                            payload: None,
                                        },
                                        call_span,
                                    );
                                }
                                continue;
                            }
                        }
                    }
                };

                // If we got a value, call the handler
                if let Ok(value) = recv_result {
                    // Retrieve and materialize the handler.
                    let handler_val = materialize(&handler_id, Some(&call_span), &ctx).await?;

                    // Call the handler with the received value.
                    let result_thunk = match handler_val {
                        Value::Function {
                            params,
                            body,
                            closure_env,
                            annotation: _,
                            ..
                        } => {
                            if params.len() != 1 {
                                return Err(EvalError::user_error(
                                    format!(
                                        "select-once handler expects 1 parameter, got {}",
                                        params.len()
                                    ),
                                    call_span,
                                )
                                .into());
                            }

                            // T-1558: Use closure_env as call scope.
                            // T-1555 gap: par-map closures skip bind_args_thunks — single-param function body evaluated
                            // directly in closure_env rather than a dedicated call frame. Pattern variables not bound
                            // in FlatEnv. Tracked as T-1555 match-arm-closures gap.
                            eval_core_expr(
                                &body,
                                &crate::value::EvalFrame::for_function_call(
                                    Arc::clone(&closure_env),
                                    vec![],
                                ),
                                &ctx,
                            )
                            .await?
                        }
                        Value::Builtin(def) => {
                            // Call builtin with the value — return result thunk directly.
                            let arg_thunk = Arc::new(Thunk::value(value, call_span.clone()));
                            (def.func)(BuiltinArgs {
                                args: vec![arg_thunk],
                                named: None,
                                call_span: call_span.clone(),
                                caller_env_id: None,
                                ctx: Arc::clone(&ctx),
                            })
                            .await?
                        }
                        _ => {
                            return Err(EvalError::type_mismatch(
                                "Function",
                                handler_val.type_name(),
                                call_span,
                            )
                            .into())
                        }
                    };

                    // Return the handler result directly — result_thunk stays lazy.
                    return Ok(result_thunk);
                }
            }

            // Check for context cancellation before yielding.
            // Honour both the EvalContext token and the user-supplied Context token.
            if ctx.cancel.is_cancelled() || user_token.is_cancelled() {
                return Err(
                    EvalError::user_error("select-once: cancelled".to_string(), call_span).into(),
                );
            }

            // No channel had a value this pass — yield and retry.
            tokio::task::yield_now().await;
        }
    })
}

/// `par`: Eagerly evaluate an expression in parallel.
///
/// Signature: `expr → Task@T`
///
/// Spawns evaluation of `expr` immediately via spawn_local. Returns a Task handle
/// that can be awaited. This is a hint to the runtime to start work now rather than
/// waiting for demand.
///
/// Implementation: identical to `task` but the name signals eager intent.
pub(crate) fn builtin_par(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    // For now, par is just an alias for task
    // In a full multi-threaded implementation, this would use tokio::spawn instead of spawn_local
    builtin_task(ctx_arg)
}

/// `par-map`: Apply a function to each element of a sequence in parallel.
///
/// Signature: `[Fn@B [A]] → [Seq A] → [Seq B]`
///
/// Spawns concurrent tasks for each element, collects results in order.
pub(crate) fn builtin_par_map(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let (func_thunk, seq_thunk) =
            take_two_thunks("par-map", &args, named.as_ref(), call_span.clone(), &ctx)?;

        // Materialize the sequence
        let seq_val = materialize(&seq_thunk, Some(&call_span), &ctx).await?;

        // Collect sequence items
        let item_ids = collect_dict_to_vec(seq_val, &ctx, call_span.clone(), "par-map").await?;

        // Spawn a task for each item
        let mut tasks = Vec::new();
        for item_thunk in &item_ids {
            let func_thunk_clone = Arc::clone(&func_thunk);
            let item_thunk_clone = Arc::clone(item_thunk);
            let ctx_clone = Arc::clone(&ctx);
            let call_span_clone = call_span.clone();

            let handle = crate::async_rt::spawn_local(async move {
                // Materialize the function
                let func_val =
                    materialize(&func_thunk_clone, Some(&call_span_clone), &ctx_clone).await?;

                // Materialize the item
                let item_val =
                    materialize(&item_thunk_clone, Some(&call_span_clone), &ctx_clone).await?;

                // Call the function with the item
                match func_val {
                    Value::Function {
                        params,
                        body,
                        closure_env,
                        annotation: _,
                        ..
                    } => {
                        if params.len() != 1 {
                            return Err(EvalError::user_error(
                                format!(
                                    "par-map function expects 1 parameter, got {}",
                                    params.len()
                                ),
                                call_span_clone,
                            )
                            .into());
                        }

                        // T-1558: Use closure_env as call scope.
                        // T-1555 gap: par-map closures skip bind_args_thunks — single-param function body evaluated
                        // directly in closure_env rather than a dedicated call frame. Pattern variables not bound
                        // in FlatEnv. Tracked as T-1555 match-arm-closures gap.
                        let result_thunk = eval_core_expr(
                            &body,
                            &crate::value::EvalFrame::for_function_call(
                                Arc::clone(&closure_env),
                                vec![],
                            ),
                            &ctx_clone,
                        )
                        .await?;
                        materialize(&result_thunk, None, &ctx_clone).await
                    }
                    Value::Builtin(def) => {
                        // Call builtin with the item
                        let item_thunk_arg =
                            Arc::new(Thunk::value(item_val, call_span_clone.clone()));
                        let result = (def.func)(BuiltinArgs {
                            args: vec![item_thunk_arg],
                            named: None,
                            call_span: call_span_clone,
                            caller_env_id: None,
                            ctx: ctx_clone.clone(),
                        })
                        .await?;
                        materialize(&result, None, &ctx_clone).await
                    }
                    _ => Err(EvalError::type_mismatch(
                        "Function",
                        func_val.type_name(),
                        call_span_clone,
                    )
                    .into()),
                }
            });

            tasks.push(handle);
        }

        // Await all tasks and collect results, aborting remaining handles on cancellation.
        let mut result_thunks: Vec<Arc<Thunk>> = Vec::new();
        let mut remaining = tasks.into_iter();
        for handle in remaining.by_ref() {
            // Race each handle against context cancellation.
            let result_val = tokio::select! {
                join_result = handle => {
                    join_result.map_err(|e| {
                        EvalError::user_error(format!("par-map task panicked: {e}"), call_span.clone())
                    })??
                }
                _ = ctx.cancel.cancelled() => {
                    // Abort all handles that have not yet been awaited.
                    for h in remaining {
                        h.abort();
                    }
                    return Err(EvalError::user_error(
                        "par-map: cancelled".to_string(),
                        call_span,
                    ).into());
                }
            };
            result_thunks.push(Arc::new(Thunk::value(result_val, call_span.clone())));
        }

        // Build the result sequence
        let result_seq = build_dict_from_vec(result_thunks, &ctx, call_span);
        Ok(result_seq)
    })
}

/// `par-filter`: Filter a sequence in parallel.
///
/// Signature: `[Fn@Bool [A]] → [Seq A] → [Seq A]`
///
/// Evaluates the predicate on all elements concurrently, returns sequence of passing elements.
pub(crate) fn builtin_par_filter(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let (pred_thunk, seq_thunk) =
            take_two_thunks("par-filter", &args, named.as_ref(), call_span.clone(), &ctx)?;

        // Pre-materialize the predicate to extract its return type annotation once.
        // This lets us pre-resolve the Matchable binding name before spawning tasks,
        // avoiding repeated runtime type derivation on every predicate invocation.
        let pred_fn_val = materialize(&pred_thunk, Some(&call_span), &ctx).await?;
        let pred_matchable_binding = crate::eval::resolve_matchable_binding_from_fn(&pred_fn_val);
        // Re-wrap as a materialized thunk so tasks can still use the standard call path.
        let pred_thunk = Arc::new(Thunk::value(pred_fn_val, call_span.clone()));

        // Materialize the sequence
        let seq_val = materialize(&seq_thunk, Some(&call_span), &ctx).await?;

        // Collect sequence items
        let item_ids = collect_dict_to_vec(seq_val, &ctx, call_span.clone(), "par-filter").await?;

        // Spawn a task for each item to evaluate the predicate
        let mut tasks = Vec::new();
        for (idx, item_thunk) in item_ids.iter().enumerate() {
            let pred_thunk_clone = Arc::clone(&pred_thunk);
            let item_thunk_clone = Arc::clone(item_thunk);
            let ctx_clone = Arc::clone(&ctx);
            let call_span_clone = call_span.clone();
            let item_thunk_copy = Arc::clone(item_thunk);
            let matchable_binding_clone = pred_matchable_binding.clone();

            let handle = crate::async_rt::spawn_local(async move {
                // Materialize the predicate (already cached from pre-materialization above)
                let pred_val =
                    materialize(&pred_thunk_clone, Some(&call_span_clone), &ctx_clone).await?;

                // Materialize the item
                let item_val =
                    materialize(&item_thunk_clone, Some(&call_span_clone), &ctx_clone).await?;

                // Call the predicate
                let result = match pred_val {
                    Value::Function {
                        params,
                        body,
                        closure_env,
                        annotation: _,
                        ..
                    } => {
                        if params.len() != 1 {
                            return Err(Box::new(EvalError::user_error(
                                format!(
                                    "par-filter predicate expects 1 parameter, got {}",
                                    params.len()
                                ),
                                call_span_clone,
                            )));
                        }

                        // T-1558: Use closure_env as call scope.
                        // T-1555 gap: par-filter closures skip bind_args_thunks — single-param function body evaluated
                        // directly in closure_env rather than a dedicated call frame. Pattern variables not bound
                        // in FlatEnv. Tracked as T-1555 match-arm-closures gap.
                        let result_thunk = eval_core_expr(
                            &body,
                            &crate::value::EvalFrame::for_function_call(
                                Arc::clone(&closure_env),
                                vec![],
                            ),
                            &ctx_clone,
                        )
                        .await?;
                        materialize(&result_thunk, None, &ctx_clone).await?
                    }
                    Value::Builtin(def) => {
                        let arg_thunk =
                            Arc::new(Thunk::value(item_val.clone(), call_span_clone.clone()));
                        let result_thunk = (def.func)(BuiltinArgs {
                            args: vec![arg_thunk],
                            named: None,
                            call_span: call_span_clone.clone(),
                            caller_env_id: None,
                            ctx: ctx_clone.clone(),
                        })
                        .await?;
                        materialize(&result_thunk, None, &ctx_clone).await?
                    }
                    _ => {
                        return Err(EvalError::type_mismatch(
                            "Function",
                            pred_val.type_name(),
                            call_span_clone,
                        )
                        .into())
                    }
                };

                // Check if result is truthy via Matchable dispatch.
                // Use pre-resolved binding name if available (avoids runtime type derivation).
                // call_to_match_opt_resolved ignores the env arg (B-515 tracks FlatEnv arm binding lookup).
                let dummy_env = Arc::new(std::sync::RwLock::new(crate::value::Environment::new()));
                if crate::eval::call_to_match_opt_resolved(
                    &result,
                    matchable_binding_clone.as_deref(),
                    &dummy_env,
                    &ctx_clone,
                    &call_span_clone,
                )
                .await
                {
                    Ok(Some((idx, item_thunk_copy)))
                } else {
                    Ok(None)
                }
            });

            tasks.push(handle);
        }

        // Await all tasks and collect passing items, aborting remaining handles on cancellation.
        let mut results_with_idx = Vec::new();
        let mut remaining = tasks.into_iter();
        for handle in remaining.by_ref() {
            // Race each handle against context cancellation.
            let result = tokio::select! {
                join_result = handle => {
                    join_result.map_err(|e| {
                        EvalError::user_error(format!("par-filter task panicked: {e}"), call_span.clone())
                    })??
                }
                _ = ctx.cancel.cancelled() => {
                    // Abort all handles that have not yet been awaited.
                    for h in remaining {
                        h.abort();
                    }
                    return Err(EvalError::user_error(
                        "par-filter: cancelled".to_string(),
                        call_span,
                    ).into());
                }
            };
            if let Some(item) = result {
                results_with_idx.push(item);
            }
        }

        // Sort by original index to preserve order
        results_with_idx.sort_by_key(|(idx, _)| *idx);

        // Extract Arc<Thunk>s
        let result_thunks: Vec<Arc<Thunk>> = results_with_idx.into_iter().map(|(_, t)| t).collect();

        // Build the result sequence
        let result_seq = build_dict_from_vec(result_thunks, &ctx, call_span);
        Ok(result_seq)
    })
}

/// `signal-channel`: Create a channel that receives a `Signal` variant when a Unix signal fires.
///
/// Signature: `[Fn [signals@[Seq Signal]] [Channel Signal]]`
///
/// Supported signals: SIGINT, SIGTERM, SIGHUP, SIGQUIT, SIGUSR1, SIGUSR2.
/// Spawns one background task per signal; all write to the same channel.
/// Channel capacity = number of signals; additional deliveries before recv are dropped.
///
/// On non-Unix platforms this builtin always returns an error.
pub(crate) fn builtin_signal_channel(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let signals_thunk = take_one_thunk(
            "signal-channel",
            &args,
            named.as_ref(),
            call_span.clone(),
            &ctx,
        )?;
        let signals_val = materialize(&signals_thunk, Some(&call_span), &ctx).await?;

        // Collect signal names from an integer-keyed Dict of Signal variants.
        let sig_dict = match signals_val {
            Value::Dict(d) => d,
            other => {
                return Err(EvalError::type_mismatch(
                    "Dict of Signal",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };
        let mut sig_names: Vec<String> = Vec::new();
        for (_idx, head_thunk) in &sig_dict {
            let head_val = materialize(&head_thunk, Some(&call_span), &ctx).await?;
            let name = match head_val {
                Value::Variant {
                    ref tycon,
                    ref ctor,
                    ..
                } => format!("{}.{}", tycon, ctor),
                _ => {
                    return Err(
                        EvalError::type_mismatch("Signal", head_val.type_name(), call_span).into(),
                    )
                }
            };
            sig_names.push(name);
        }

        if sig_names.is_empty() {
            return Err(EvalError::user_error(
                "signal-channel: requires at least one signal".to_string(),
                call_span,
            )
            .into());
        }

        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};

            let capacity = sig_names.len();
            let (tx, rx) = tokio::sync::mpsc::channel::<Value>(capacity);

            for sig_name in sig_names {
                let kind = match sig_name.as_str() {
                    "SIGINT" => SignalKind::interrupt(),
                    "SIGTERM" => SignalKind::terminate(),
                    "SIGHUP" => SignalKind::hangup(),
                    "SIGQUIT" => SignalKind::quit(),
                    "SIGUSR1" => SignalKind::user_defined1(),
                    "SIGUSR2" => SignalKind::user_defined2(),
                    other => {
                        return Err(EvalError::user_error(
                            format!("signal-channel: unknown signal {other:?}; supported: SIGINT SIGTERM SIGHUP SIGQUIT SIGUSR1 SIGUSR2"),
                            call_span,
                        )
                        .into())
                    }
                };

                let mut sig_stream = signal(kind).map_err(|e| {
                    EvalError::user_error(
                        format!("signal-channel: failed to register signal handler: {e}"),
                        call_span.clone(),
                    )
                })?;

                let tx_clone = tx.clone();
                let cancel_token = ctx.cancel.clone();

                let handle = crate::async_rt::spawn_local(async move {
                    loop {
                        tokio::select! {
                            biased;
                            _ = cancel_token.cancelled() => {
                                break;
                            }
                            result = sig_stream.recv() => {
                                if result.is_none() {
                                    break;
                                }
                                let (sig_tycon, sig_ctor) = sig_name
                                    .split_once('.')
                                    .unwrap_or(("", sig_name.as_str()));
                                let _ = tx_clone.try_send(Value::Variant {
                                    tycon: Arc::from(sig_tycon),
                                    ctor: Arc::from(sig_ctor),
                                    payload: None,
                                });
                            }
                        }
                    }
                });

                ctx.task_registry.lock().unwrap().push(handle);
            }

            let channel_inner = crate::value::ChannelInner {
                sender: tx,
                receiver: tokio::sync::Mutex::new(rx),
                capacity: capacity as i64,
            };
            ok_val(Value::Channel(Arc::new(channel_inner)), call_span)
        }

        #[cfg(not(unix))]
        {
            let _ = sig_names;
            Err(EvalError::user_error(
                "signal-channel is only supported on Unix platforms".to_string(),
                call_span,
            )
            .into())
        }
    })
}

/// `timer-channel`: Create a channel that ticks every `interval` duration.
///
/// Signature: `ClockCap → Duration → Channel@Timestamp`
///
/// Spawns a local task driven by `tokio::time::interval`. Sends `Value::Timestamp` (scheduled
/// tick time in nanoseconds since Unix epoch) on each tick. The channel has capacity 1; if the
/// receiver is slow, ticks are dropped (non-blocking `try_send`) so that a slow consumer never
/// builds up an unbounded backlog.
///
/// Backward compatibility: also accepts a bare Int (treated as milliseconds).
pub(crate) fn builtin_timer_channel(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let (clock_thunk, interval_thunk) = take_two_thunks(
            "timer-channel",
            &args,
            named.as_ref(),
            call_span.clone(),
            &ctx,
        )?;

        // Validate ClockCap (force_count=1 in builtin registry pre-materializes it)
        let clock_val = clock_thunk
            .try_get_value()
            .expect("pre-materialized by force_count=1")
            .clone();
        let clock_inner: ClockCapInner = match &clock_val {
            Value::ClockCap(inner) => inner.as_ref().clone(),
            _ => {
                return Err(EvalError::type_mismatch(
                    "ClockCap",
                    clock_val.type_name(),
                    call_span.clone(),
                )
                .into())
            }
        };

        let interval_val = materialize(&interval_thunk, Some(&call_span), &ctx).await?;

        let interval_ms = match interval_val {
            // Duration is stored as nanoseconds (i64) — divide by 1_000_000 to get milliseconds
            Value::Duration(nanos) if nanos >= 1_000_000 => (nanos / 1_000_000) as u64,
            Value::Duration(nanos) => {
                return Err(EvalError::user_error(
                    format!("timer-channel: interval must be ≥ 1 ms, got {} ns", nanos),
                    call_span,
                )
                .into())
            }
            // Backward compatibility: accept bare Int as milliseconds
            Value::Int(n) if n >= 1 => n as u64,
            Value::Int(n) => {
                return Err(EvalError::user_error(
                    format!("timer-channel: interval must be ≥ 1 ms, got {n}"),
                    call_span,
                )
                .into())
            }
            _ => {
                return Err(EvalError::type_mismatch(
                    "Duration or Int",
                    interval_val.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // Capacity 1: non-blocking try_send drops ticks if the receiver is slow.
        let (tx, rx) = tokio::sync::mpsc::channel::<Value>(1);
        let tx_clone = tx.clone();
        let cancel_token = ctx.cancel.clone();

        let handle = crate::async_rt::spawn_local(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
            // Skip the first immediate tick so interval_ms elapses before the first send.
            interval.tick().await;
            loop {
                tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                    _ = interval.tick() => {
                        // Get current time as nanoseconds since Unix epoch.
                        // Respects ClockCapInner::Fixed for deterministic testing.
                        let now_nanos = match &clock_inner {
                            ClockCapInner::Real => std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos() as i64,
                            ClockCapInner::Fixed(nanos) => *nanos,
                        };
                        // Non-blocking: if the receiver hasn't consumed the previous tick, drop this one.
                        let _ = tx_clone.try_send(Value::Timestamp(now_nanos));
                    }
                }
            }
        });

        // Register background task for drain tracking
        ctx.task_registry.lock().unwrap().push(handle);

        let channel_inner = crate::value::ChannelInner {
            sender: tx,
            receiver: tokio::sync::Mutex::new(rx),
            capacity: 1,
        };
        ok_val(Value::Channel(Arc::new(channel_inner)), call_span)
    })
}

/// `watch-channel`: Create a filesystem watch channel.
///
/// Signature: `DirCap → String → Channel@Null`
///
/// Watches the file or directory at the given path (resolved relative to the DirCap)
/// and sends `Value::Dict([])` (null) on the channel whenever the file's metadata changes.
/// Uses polling-based detection: checks `modified()` timestamp every 1 second.
///
/// The channel has capacity 1; if the consumer is slow, only the most recent event is
/// retained (last-write-wins semantics).
///
/// Example usage:
/// ```tinct
/// dir: [builtin-dir-cap "."]
/// ch: [builtin-watch-channel dir "config.toml"]
/// # Modifying config.toml will produce a recv event after ~1 second
/// event: [builtin-recv ch]  # blocks until file changes
/// ```
///
/// Implementation note: This is a simplified polling-based watcher. A production-grade
/// implementation would use the `notify` crate for OS-level filesystem events (inotify,
/// FSEvents, etc.), but this polling approach has no external dependencies.
pub(crate) fn builtin_watch_channel(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let (dir_cap_thunk, path_thunk) = take_two_thunks(
            "watch-channel",
            &args,
            named.as_ref(),
            call_span.clone(),
            &ctx,
        )?;
        let dir_cap_val = materialize(&dir_cap_thunk, Some(&call_span), &ctx).await?;
        let path_val = materialize(&path_thunk, Some(&call_span), &ctx).await?;

        // Extract DirCap
        let dir = match dir_cap_val {
            Value::DirCap { dir, perms: _ } => dir,
            _ => {
                return Err(
                    EvalError::type_mismatch("DirCap", dir_cap_val.type_name(), call_span).into(),
                )
            }
        };

        // Extract path String
        let path = match path_val {
            Value::String { source, start, end } => source[start..end].to_string(),
            _ => {
                return Err(
                    EvalError::type_mismatch("String", path_val.type_name(), call_span).into(),
                )
            }
        };

        // Create the channel (capacity 1 for last-write-wins)
        let (tx, rx) = tokio::sync::mpsc::channel::<Value>(1);
        let tx_clone = tx.clone();
        let cancel_token = ctx.cancel.clone();

        // Spawn the filesystem polling task
        let handle = crate::async_rt::spawn_local(async move {
            // Get the initial metadata to establish a baseline
            let mut last_modified = match dir.metadata(&path) {
                Ok(meta) => meta.modified().ok(),
                Err(_) => None,
            };

            loop {
                tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        // Check current metadata
                        match dir.metadata(&path) {
                            Ok(meta) => {
                                if let Ok(current_modified) = meta.modified() {
                                    // Compare with last known modification time
                                    // Only send if we have a previous baseline AND it's different
                                    if let Some(last) = last_modified {
                                        if current_modified != last {
                                            // File changed — send null on the channel
                                            // Use try_send (non-blocking) for last-write-wins semantics
                                            let _ = tx_clone.try_send(Value::Dict(IndexMap::new()));
                                            last_modified = Some(current_modified);
                                        }
                                    } else {
                                        // File appeared — treat as a change
                                        let _ = tx_clone.try_send(Value::Dict(IndexMap::new()));
                                        last_modified = Some(current_modified);
                                    }
                                }
                            }
                            Err(_) => {
                                // File no longer exists or is inaccessible — treat as a change
                                if last_modified.is_some() {
                                    let _ = tx_clone.try_send(Value::Dict(IndexMap::new()));
                                    last_modified = None;
                                }
                            }
                        }
                    }
                }
            }
        });

        // Register background task for drain tracking
        ctx.task_registry.lock().unwrap().push(handle);

        // Build and return the Channel value
        let channel_inner = crate::value::ChannelInner {
            sender: tx,
            receiver: tokio::sync::Mutex::new(rx),
            capacity: 1,
        };
        ok_val(Value::Channel(Arc::new(channel_inner)), call_span)
    })
}

// =============================================================================
// Cancellation context primitives
// =============================================================================

/// `context`: Create a root cancellation context.
///
/// Signature: `→ Context`
///
/// Returns a fresh root `CancellationToken` wrapped as `Value::Context`. This context
/// is independent of any existing context — use `[with-cancel parent]` to create a
/// child that inherits cancellation from a parent.
///
/// Per async-eval.md: the runtime creates a root context for every program run.
/// `[context]` gives user code access to a fresh independent root context.
pub(crate) fn builtin_context(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx: _ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        if !args.is_empty() {
            return Err(EvalError::user_error(
                format!("context expects 0 arguments, got {}", args.len()),
                call_span,
            )
            .into());
        }
        if !named.as_ref().is_none_or(|n| n.is_empty()) {
            return Err(EvalError::user_error(
                "context does not accept named arguments".to_string(),
                call_span,
            )
            .into());
        }
        let token = tokio_util::sync::CancellationToken::new();
        ok_val(Value::Context(token), call_span)
    })
}

/// `with-cancel`: Create a child context plus a cancel handle.
///
/// Signature: `Context → {child-ctx: Context, cancel: Context}`
///
/// Returns a dict with two fields:
/// - `child-ctx`: a child `Context` that is automatically cancelled when the parent is cancelled.
///   Cancelling `child-ctx` does NOT cancel the parent.
/// - `cancel`: a `Context` value holding the same child token. Call `[cancel-task pair.cancel]`
///   to fire cancellation. Note: the spec describes `cancel` as a zero-arg callable `Fn@[]@Null`,
///   but the implementation uses a `Context` value to avoid closing over a Rust
///   `CancellationToken` inside a `Value::Function`. The functional semantics are equivalent.
///
/// Per async-eval.md: `[child-ctx: child  cancel: cancel-ctx]: [with-cancel ctx]`
pub(crate) fn builtin_with_cancel(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let parent_thunk = take_one_thunk(
            "with-cancel",
            &args,
            named.as_ref(),
            call_span.clone(),
            &ctx,
        )?;
        let parent_val = materialize(&parent_thunk, Some(&call_span), &ctx).await?;

        let parent_token = match parent_val {
            Value::Context(token) => token,
            _ => {
                return Err(
                    EvalError::type_mismatch("Context", parent_val.type_name(), call_span).into(),
                )
            }
        };

        // Create a child token that inherits cancellation from the parent.
        let child_token = parent_token.child_token();
        let cancel_token = child_token.clone();

        // cancel is a Context clone of child_token; caller fires it with [cancel-task pair.cancel].
        // See doc/08-evaluation.md §Cancellation Primitives for the spec deviation rationale.
        let child_ctx_val = Value::Context(child_token);
        let cancel_val = Value::Context(cancel_token);

        // Build the payload dict with the same structure as before
        let mut payload_dict = IndexMap::new();
        payload_dict.insert(
            HashableValue::Str("child-ctx".into()),
            Arc::new(Thunk::value(child_ctx_val, call_span.clone())),
        );
        payload_dict.insert(
            HashableValue::Str("cancel".into()),
            Arc::new(Thunk::value(cancel_val, call_span.clone())),
        );

        // Wrap the dict in a Variant with tag "CancelHandle"
        let payload_thunk = Arc::new(Thunk::value(Value::Dict(payload_dict), call_span.clone()));

        ok_val(
            Value::Variant {
                tycon: Arc::from("CancelHandle"),
                ctor: Arc::from("CancelHandle"),
                payload: Some(payload_thunk),
            },
            call_span,
        )
    })
}

/// `with-timeout`: Create a child context that auto-cancels after `duration-ms` milliseconds.
///
/// Signature: `ClockCap → Context → Duration → Context`
///
/// Creates a child token derived from the parent. Spawns a background local task that
/// sleeps for `duration-ms` milliseconds then calls `cancel()` on the child token.
/// Returns the child context.
///
/// Requires ClockCap to access wall-clock time, consistent with `timer-channel`.
///
/// Per async-eval.md: `timed-ctx: [with-timeout %clock ctx [duration 5 "s"]]`
pub(crate) fn builtin_with_timeout(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let (clock_thunk, parent_thunk, ms_thunk) = take_three_thunks(
            "with-timeout",
            &args,
            named.as_ref(),
            call_span.clone(),
            &ctx,
        )?;

        // Validate ClockCap (force_count=1 in builtin registry pre-materializes it)
        let clock_val = clock_thunk
            .try_get_value()
            .expect("pre-materialized by force_count=1")
            .clone();
        let _clock_inner: ClockCapInner = match &clock_val {
            Value::ClockCap(inner) => inner.as_ref().clone(),
            _ => {
                return Err(EvalError::type_mismatch(
                    "ClockCap",
                    clock_val.type_name(),
                    call_span.clone(),
                )
                .into())
            }
        };

        let parent_val = materialize(&parent_thunk, Some(&call_span), &ctx).await?;
        let ms_val = materialize(&ms_thunk, Some(&call_span), &ctx).await?;

        let parent_token = match parent_val {
            Value::Context(token) => token,
            _ => {
                return Err(
                    EvalError::type_mismatch("Context", parent_val.type_name(), call_span).into(),
                )
            }
        };

        let duration_ms = match ms_val {
            // Duration is stored as nanoseconds (i64) — divide by 1_000_000 to get milliseconds
            Value::Duration(nanos) if nanos >= 0 => (nanos / 1_000_000).max(1) as u64,
            Value::Duration(nanos) => {
                return Err(EvalError::user_error(
                    format!("with-timeout: duration must be ≥ 0, got {} ns", nanos),
                    call_span,
                )
                .into())
            }
            // Backward compatibility: accept bare Int as milliseconds
            Value::Int(n) if n >= 0 => n as u64,
            Value::Int(n) => {
                return Err(EvalError::user_error(
                    format!("with-timeout: duration must be ≥ 0 ms, got {n}"),
                    call_span,
                )
                .into())
            }
            _ => {
                return Err(EvalError::type_mismatch(
                    "Duration or Int",
                    ms_val.type_name(),
                    call_span,
                )
                .into())
            }
        };

        let child_token = parent_token.child_token();
        let cancel_clone = child_token.clone();

        // Spawn a local task to cancel the child token after the timeout.
        // Note: We use real wall-clock time via tokio::time::sleep regardless of ClockCapInner.
        // The ClockCap gate is for authorization, not for deterministic sleep behavior.
        let handle = crate::async_rt::spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;
            cancel_clone.cancel();
        });

        // Register background task for drain tracking
        ctx.task_registry.lock().unwrap().push(handle);

        ok_val(Value::Context(child_token), call_span)
    })
}

/// `with-deadline`: Create a child context that auto-cancels at an absolute Unix timestamp (ms).
///
/// Signature: `ClockCap → Context → Timestamp → Context`
///
/// Like `with-timeout` but the deadline is specified as an absolute Unix timestamp in
/// milliseconds (matching `Value::Timestamp` semantics). If the deadline is already past,
/// the child context is cancelled immediately.
///
/// Requires ClockCap to access wall-clock time, consistent with `timer-channel`.
///
/// Per async-eval.md: `dead-ctx: [with-deadline %clock ctx ts]`
pub(crate) fn builtin_with_deadline(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let (clock_thunk, parent_thunk, ts_thunk) = take_three_thunks(
            "with-deadline",
            &args,
            named.as_ref(),
            call_span.clone(),
            &ctx,
        )?;

        // Validate ClockCap (force_count=1 in builtin registry pre-materializes it)
        let clock_val = clock_thunk
            .try_get_value()
            .expect("pre-materialized by force_count=1")
            .clone();
        let _clock_inner: ClockCapInner = match &clock_val {
            Value::ClockCap(inner) => inner.as_ref().clone(),
            _ => {
                return Err(EvalError::type_mismatch(
                    "ClockCap",
                    clock_val.type_name(),
                    call_span.clone(),
                )
                .into())
            }
        };

        let parent_val = materialize(&parent_thunk, Some(&call_span), &ctx).await?;
        let ts_val = materialize(&ts_thunk, Some(&call_span), &ctx).await?;

        let parent_token = match parent_val {
            Value::Context(token) => token,
            _ => {
                return Err(
                    EvalError::type_mismatch("Context", parent_val.type_name(), call_span).into(),
                )
            }
        };

        // Accept Int (unix-ms) or Timestamp (nanoseconds since Unix epoch as i64).
        let deadline_unix_ns: i64 = match ts_val {
            Value::Int(n) => {
                // Treat Int as Unix milliseconds — convert to nanoseconds for uniform handling.
                n.saturating_mul(1_000_000)
            }
            Value::Timestamp(nanos) => nanos,
            _ => {
                return Err(EvalError::type_mismatch(
                    "Int or Timestamp",
                    ts_val.type_name(),
                    call_span,
                )
                .into())
            }
        };

        let child_token = parent_token.child_token();
        let cancel_clone = child_token.clone();

        // Compute delay: deadline_unix_ns - now_unix_ns.
        // Note: We use real wall-clock time via SystemTime::now() regardless of ClockCapInner.
        // The ClockCap gate is for authorization, not for deterministic deadline behavior.
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;
        let delay_ns = (deadline_unix_ns - now_ns).max(0) as u64;

        let handle = crate::async_rt::spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_nanos(delay_ns)).await;
            cancel_clone.cancel();
        });

        // Register background task for drain tracking
        ctx.task_registry.lock().unwrap().push(handle);

        ok_val(Value::Context(child_token), call_span)
    })
}

/// `cancelled?`: Check if a context has been cancelled.
///
/// Signature: `Context → Bool`
///
/// Returns `true` if the context's CancellationToken has been cancelled, `false` otherwise.
/// This is a synchronous (non-blocking) check — it does not suspend.
///
/// Per async-eval.md: `[if [cancelled? ctx] [cleanup] [continue]]`
pub(crate) fn builtin_cancelled_q(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let ctx_thunk =
            take_one_thunk("cancelled?", &args, named.as_ref(), call_span.clone(), &ctx)?;
        let ctx_val = materialize(&ctx_thunk, Some(&call_span), &ctx).await?;

        match ctx_val {
            Value::Context(token) => ok_val(
                Value::Int(if token.is_cancelled() { 1 } else { 0 }),
                call_span,
            ),
            _ => Err(EvalError::type_mismatch("Context", ctx_val.type_name(), call_span).into()),
        }
    })
}

/// `cancel-task`: Explicitly cancel a context (and all its children).
///
/// Signature: `Context → Null`
///
/// Calls `cancel()` on the context's CancellationToken. All child contexts derived from
/// this context are also cancelled immediately. Returns null.
///
/// Per async-eval.md: `[cancel-fn]` — the cancel field from `[with-cancel ctx]` is passed here.
pub(crate) fn builtin_cancel_task(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let ctx_thunk = take_one_thunk(
            "cancel-task",
            &args,
            named.as_ref(),
            call_span.clone(),
            &ctx,
        )?;
        let ctx_val = materialize(&ctx_thunk, Some(&call_span), &ctx).await?;

        match ctx_val {
            Value::Context(token) => {
                token.cancel();
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
            _ => Err(EvalError::type_mismatch("Context", ctx_val.type_name(), call_span).into()),
        }
    })
}

// =============================================================================
// Shutdown primitives
// =============================================================================

/// `cancel-root`: Cancel the root CancellationToken.
///
/// Signature: `→ Null`
///
/// Cancels the root context (EvalContext.cancel), signaling all tasks to stop.
/// Returns null (empty dict).
///
/// Per async-eval.md: NOT capability-gated. Security = OS process isolation (tinct run boundary).
pub(crate) fn builtin_cancel_root(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        if !args.is_empty() {
            return Err(EvalError::user_error(
                format!("cancel-root expects 0 arguments, got {}", args.len()),
                call_span,
            )
            .into());
        }
        if !named.as_ref().is_none_or(|n| n.is_empty()) {
            return Err(EvalError::user_error(
                "cancel-root does not accept named arguments".to_string(),
                call_span,
            )
            .into());
        }

        // Cancel the root context
        ctx.cancel.cancel();

        // Return null (empty dict)
        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `drain`: Wait for in-flight tasks to complete.
///
/// Signature: `→ Null`
///
/// Waits for all registered background tasks (signal-channel, timer-channel, watch-channel,
/// with-timeout, with-deadline) to complete. Does NOT wait for user-spawned tasks created
/// via `task` or `par` — those must be explicitly awaited by the caller before calling drain.
///
/// **Graceful shutdown:** Background tasks check `ctx.cancel.cancelled()` on every loop
/// iteration via `tokio::select!`. The recommended shutdown sequence is
/// `[cancel-root] [drain] [exit-now code]`: `[cancel-root]` signals all loops to exit cleanly,
/// then `[drain]` awaits their `JoinHandle`s (which complete promptly), then `[exit-now]`
/// terminates the process. Without a prior `[cancel-root]`, `drain` calls `handle.abort()` on
/// each registered handle as a fallback — tasks are terminated abruptly at the next `.await`
/// point, which may leave resources in an inconsistent state.
///
/// Design note: user-spawned tasks (via `task`) store their JoinHandle<EvalResult<Value>>
/// inside Value::Task so that `await` can retrieve the typed result. Background tasks store
/// JoinHandle<()> in task_registry. These are different types, so task registry cannot hold
/// both without architectural changes. The `par-map` and `par-filter` builtins spawn and await
/// their handles inline, so they complete before the builtin returns and do not need registration.
///
/// Per async-eval.md: includes cluster-local workers (Tokio tasks), excludes remote workers.
pub(crate) fn builtin_drain(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        if !args.is_empty() {
            return Err(EvalError::user_error(
                format!("drain expects 0 arguments, got {}", args.len()),
                call_span,
            )
            .into());
        }
        if !named.as_ref().is_none_or(|n| n.is_empty()) {
            return Err(EvalError::user_error(
                "drain does not accept named arguments".to_string(),
                call_span,
            )
            .into());
        }

        // Take all registered background tasks and await them
        let handles = {
            let mut registry = ctx.task_registry.lock().unwrap();
            std::mem::take(&mut *registry)
        };

        for handle in handles {
            // Always abort before awaiting: abort() on an already-finished task is a no-op,
            // and abort() on a one-shot sleep task (with-timeout, with-deadline) prevents
            // drain from blocking for the full sleep duration even after cancel-root. Infinite
            // background loops (signal/timer/watch) will have already exited cleanly via their
            // select! branch when cancel-root fired, so their abort() is also a no-op.
            handle.abort();
            let _ = handle.await; // Ok(()) for clean exit, Err(JoinError::Cancelled) for aborted
        }

        // Return null (empty dict)
        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `exit-now`: Immediately terminate the process.
///
/// Signature: `Int → Null`
///
/// Calls `std::process::exit(code)` to terminate the process immediately.
/// The code argument defaults to 0 if not provided (though the signature requires it).
///
/// Per async-eval.md: used by stdlib/async.llt `exit` and `graceful-exit` after
/// cancel-root and drain.
pub(crate) fn builtin_exit_now(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        if args.len() != 1 {
            return Err(EvalError::user_error(
                format!("exit-now expects 1 argument, got {}", args.len()),
                call_span,
            )
            .into());
        }
        if !named.as_ref().is_none_or(|n| n.is_empty()) {
            return Err(EvalError::user_error(
                "exit-now does not accept named arguments".to_string(),
                call_span,
            )
            .into());
        }

        let code_thunk = Arc::clone(&args[0]);
        let code_val = materialize(&code_thunk, Some(&call_span), &ctx).await?; // H1: exit code must be known to terminate process

        let exit_code = match code_val {
            Value::Int(n) => n.clamp(0, 255) as i32,
            _ => {
                return Err(EvalError::type_mismatch("Int", code_val.type_name(), call_span).into())
            }
        };

        // Terminate the process immediately
        std::process::exit(exit_code);
    })
}

/// `non-cancellable`: Create a fresh root context that nothing will ever cancel.
///
/// Signature: `→ Context`
///
/// Returns a fresh root `CancellationToken` wrapped as `Value::Context`. This context
/// is completely independent — no parent will cancel it, and it's not a child of any
/// existing context. Used for cleanup code that must run even when the main context
/// is cancelled (e.g., `finally` blocks in stdlib/async.llt).
///
/// Per async-eval.md: `[with-context [non-cancellable] cleanup-fn]` ensures cleanup
/// runs to completion even if the caller's context is cancelled.
pub(crate) fn builtin_non_cancellable(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx: _ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        if !args.is_empty() {
            return Err(EvalError::user_error(
                format!("non-cancellable expects 0 arguments, got {}", args.len()),
                call_span,
            )
            .into());
        }
        if !named.as_ref().is_none_or(|n| n.is_empty()) {
            return Err(EvalError::user_error(
                "non-cancellable does not accept named arguments".to_string(),
                call_span,
            )
            .into());
        }
        let token = tokio_util::sync::CancellationToken::new();
        ok_val(Value::Context(token), call_span)
    })
}

/// `with-context`: Evaluate a thunk under a specific cancellation context.
///
/// Signature: `Context → Fn@[]@T → T`
///
/// Takes a Context and a zero-arg function (or any thunk), evaluates the thunk with
/// `ctx.cancel` replaced by the given context's token. The thunk's evaluation will
/// respond to the given context's cancellation state, not the caller's.
///
/// Per async-eval.md: `[with-context [non-cancellable] [fn [] cleanup]]` runs cleanup
/// in a non-cancellable context even if the caller is cancelled.
///
/// Implementation note: creates a new EvalContext with the same config/state/arenas but
/// the given CancellationToken. This is safe because CancellationToken is the only
/// evaluation-local state that should differ between parent and child evaluation contexts.
pub(crate) fn builtin_with_context(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let (context_thunk, expr_thunk) = take_two_thunks(
            "with-context",
            &args,
            named.as_ref(),
            call_span.clone(),
            &ctx,
        )?;
        let context_val = context_thunk
            .try_get_value()
            .cloned()
            // force_count=1 guarantees arg 0 is pre-materialized before this builtin runs
            .ok_or_else(|| EvalError::internal(
                "with-context: context argument not pre-materialized (force_count=1 invariant violated)".to_string(),
                call_span.clone(),
            ))?;

        let new_cancel = match context_val {
            Value::Context(token) => token,
            _ => {
                return Err(
                    EvalError::type_mismatch("Context", context_val.type_name(), call_span).into(),
                )
            }
        };

        // Create a new EvalContext with the given CancellationToken.
        // Share all arenas/config/state, but replace the cancel token.
        let new_ctx = ctx.with_explicit_cancel(new_cancel);

        // Rebirth the thunk under new_ctx so that evaluation uses the new cancellation
        // context rather than the parent birth context embedded in the thunk's
        // UnevaluatedState. This is required because materialize() ignores its `ctx`
        // parameter for actual evaluation — it uses the ctx stored in the thunk's
        // UnevaluatedState (Launchbury birth-context semantics).
        //
        // If the thunk is already materialized (or in-progress), fall back to the
        // original thunk — the context override has no effect in that case, but it
        // is safe because the value is already computed.
        let thunk_to_eval = expr_thunk
            .with_replaced_ctx(Arc::clone(&new_ctx))
            .unwrap_or_else(|| Arc::clone(&expr_thunk));
        let result = materialize(&thunk_to_eval, Some(&call_span), &new_ctx).await?;
        ok_val(result, call_span)
    })
}

// =============================================================================
// Reactive cell primitives (T-831)
// =============================================================================

/// `reactive-cell`: Create a new reactive cell with an initial value.
///
/// Signature: `T → ReactiveCell@T`
///
/// Creates a `tokio::sync::watch` channel seeded with the given value. The cell
/// stores the Sender (for writes) and a Receiver clone (for reads). All readers
/// always see the most recently written value — last-write-wins broadcast.
///
/// The initial value is materialized before the channel is created so that the
/// watch sender has a concrete `Value` to hold.
pub(crate) fn builtin_reactive_cell(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let initial_thunk = take_one_thunk(
            "reactive-cell",
            &args,
            named.as_ref(),
            call_span.clone(),
            &ctx,
        )?;
        // Materialize the initial value — the watch sender must hold a concrete Value.
        let initial_val = materialize(&initial_thunk, Some(&call_span), &ctx).await?;

        let (tx, rx) = tokio::sync::watch::channel(initial_val);
        let cell_inner = crate::value::ReactiveCellInner {
            sender: tokio::sync::Mutex::new(tx),
            receiver: rx,
        };
        ok_val(Value::ReactiveCell(Arc::new(cell_inner)), call_span)
    })
}

/// `cell-get`: Read the latest value from a reactive cell (non-blocking).
///
/// Signature: `ReactiveCell@T → T`
///
/// Borrows the current value from the watch receiver. Never blocks — always
/// returns the most recently written value immediately.
pub(crate) fn builtin_cell_get(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let cell_thunk =
            take_one_thunk("cell-get", &args, named.as_ref(), call_span.clone(), &ctx)?;
        let cell_val = materialize(&cell_thunk, Some(&call_span), &ctx).await?;

        match cell_val {
            Value::ReactiveCell(cell_inner) => {
                // borrow() returns a reference to the current value; clone it to get ownership.
                let current = cell_inner.receiver.borrow().clone();
                ok_val(current, call_span)
            }
            _ => Err(
                EvalError::type_mismatch("ReactiveCell", cell_val.type_name(), call_span).into(),
            ),
        }
    })
}

/// `cell-set`: Replace the value in a reactive cell.
///
/// Signature: `T → ReactiveCell@T → Null`
///
/// Sends a new value on the watch channel. All current and future `cell-get`
/// callers will see this new value. Returns null on success. Concurrent writes
/// are serialized by the Mutex on the sender.
///
/// The new value is materialized before writing so that the watch sender always
/// holds a concrete `Value` (not a lazy thunk that could be freed after the call).
pub(crate) fn builtin_cell_set(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let (val_thunk, cell_thunk) =
            take_two_thunks("cell-set", &args, named.as_ref(), call_span.clone(), &ctx)?;
        let cell_val = materialize(&cell_thunk, Some(&call_span), &ctx).await?;

        match cell_val {
            Value::ReactiveCell(cell_inner) => {
                // Materialize the new value before acquiring the sender lock.
                let new_val = materialize(&val_thunk, Some(&call_span), &ctx).await?;

                // Lock the sender and send. `watch::Sender::send` fails only when all
                // receivers have been dropped — which cannot happen here because we hold
                // a Receiver in ReactiveCellInner for the lifetime of the cell.
                let tx = cell_inner.sender.lock().await;
                tx.send(new_val).map_err(|_| {
                    EvalError::user_error(
                        "cell-set: reactive cell has been dropped (no receivers)".to_string(),
                        call_span.clone(),
                    )
                })?;

                // Return null (empty dict)
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
            _ => Err(
                EvalError::type_mismatch("ReactiveCell", cell_val.type_name(), call_span).into(),
            ),
        }
    })
}

/// Returns all "async" module Rust builtins.
///
/// These are the task, channel, and cancellation builtins that are NOT in the Core-46 set.
/// The Core-46 items (builtin-channel, builtin-send) stay in core_builtins() for
/// loader.llt which only has `--- uses: ["core"]`.
///
/// Consumed exclusively by `builtin_module("async")` in `src/builtins.rs`.
pub fn async_builtins() -> Vec<crate::value::BuiltinDef> {
    use crate::builtins::builtin;
    use crate::value::Strictness;
    vec![
        // ── Task lifecycle ─────────────────────────────────────────────────────────────
        builtin!("builtin-task", builtin_task),
        builtin!("builtin-await", builtin_await),
        builtin!("builtin-par", builtin_par),
        builtin!("builtin-par-map", builtin_par_map),
        builtin!("builtin-par-filter", builtin_par_filter),
        builtin!(
            "builtin-exit-now",
            builtin_exit_now,
            [Strictness::Seq],
            0,
            ["code"]
        ),
        builtin!("builtin-non-cancellable", builtin_non_cancellable),
        // ── Channels (non-Core-46) ────────────────────────────────────────────────────
        builtin!("builtin-recv", builtin_recv),
        builtin!("builtin-broadcast-channel", builtin_broadcast_channel),
        builtin!("builtin-oneshot-channel", builtin_oneshot_channel),
        builtin!("builtin-try-send", builtin_try_send),
        builtin!("builtin-select-once", builtin_select_once),
        builtin!("builtin-signal-channel", builtin_signal_channel),
        builtin!(
            "builtin-timer-channel",
            builtin_timer_channel,
            [Strictness::Seq],
            1,
            ["duration"]
        ),
        builtin!("builtin-watch-channel", builtin_watch_channel),
        builtin!("builtin-drain", builtin_drain),
        // ── Context and cancellation ───────────────────────────────────────────────────
        builtin!("builtin-context", builtin_context),
        builtin!(
            "builtin-with-cancel",
            builtin_with_cancel,
            [Strictness::Seq],
            0,
            ["ctx"]
        ),
        builtin!(
            "builtin-with-timeout",
            builtin_with_timeout,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            1,
            ["ctx", "duration", "f"]
        ),
        builtin!(
            "builtin-with-deadline",
            builtin_with_deadline,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            1,
            ["ctx", "deadline", "f"]
        ),
        builtin!(
            "builtin-cancelled-q",
            builtin_cancelled_q,
            [Strictness::Seq],
            0,
            ["ctx"]
        ),
        builtin!(
            "builtin-cancel-task",
            builtin_cancel_task,
            [Strictness::Seq],
            0,
            ["task"]
        ),
        builtin!(
            "builtin-with-context",
            builtin_with_context,
            [Strictness::Seq, Strictness::Id],
            1,
            ["ctx", "f"]
        ),
        builtin!("builtin-cancel-root", builtin_cancel_root),
        // ── Reactive cells ────────────────────────────────────────────────────────────
        builtin!("builtin-reactive-cell", builtin_reactive_cell),
        builtin!("builtin-cell-get", builtin_cell_get),
        builtin!("builtin-cell-set", builtin_cell_set),
    ]
}

#[cfg(test)]
mod tests {
    /// Verify that a cancelled? check on a token cancelled in Rust returns true.
    #[test]
    fn test_cancel_task_then_cancelled_q() {
        // Create a fresh token, cancel it in Rust, verify is_cancelled() = true.
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        assert!(
            token.is_cancelled(),
            "token should be cancelled after cancel()"
        );
    }

    /// Verify that parent cancellation propagates to child from `[with-cancel]`.
    ///
    /// We test this by calling cancel() on the Rust token directly (not through tinct),
    /// so we don't need dict syntax to share state.
    #[test]
    fn test_parent_cancel_propagates_to_child_rust() {
        let parent = tokio_util::sync::CancellationToken::new();
        let child = parent.child_token();
        assert!(!child.is_cancelled(), "child starts uncancelled");
        parent.cancel();
        assert!(
            child.is_cancelled(),
            "child is cancelled after parent.cancel()"
        );
    }

    /// Verify that cancelling a child does NOT cancel the parent (Rust-level check).
    #[test]
    fn test_child_cancel_does_not_affect_parent_rust() {
        let parent = tokio_util::sync::CancellationToken::new();
        let child = parent.child_token();
        child.cancel();
        assert!(
            !parent.is_cancelled(),
            "parent stays uncancelled after child.cancel()"
        );
    }

    /// A freshly created CancellationToken must not be cancelled (Rust-level check).
    ///
    /// This exercises the core invariant used by `cancelled?`: a new token is always
    /// in the non-cancelled state until `cancel()` is called explicitly.
    #[test]
    fn test_cancelled_q_false_on_fresh_context() {
        let token = tokio_util::sync::CancellationToken::new();
        assert!(
            !token.is_cancelled(),
            "fresh CancellationToken must not be cancelled"
        );
    }
}
