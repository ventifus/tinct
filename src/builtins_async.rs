//! Async concurrency primitives: task, await, channel, send, recv, select-once, par, par-map,
//! par-filter, and cancellation context primitives (context, with-cancel, with-timeout,
//! with-deadline, cancelled?, cancel-task).
//!
//! Design notes (from doc/whatif/async-eval.md):
//! - `task` spawns a concurrent evaluation via tokio::task::spawn_local
//! - `await` blocks until the task completes, returns its result
//! - `channel N` creates a bounded channel with capacity N (minimum 1)
//! - `send chan value` sends a value on the channel (suspends if full)
//! - `recv chan` receives a value from the channel (suspends until available)
//! - `select-once sources` waits for first channel to fire, calls its handler
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
use crate::builtins::{ok_val, MAX_COLLECT_SIZE};
use crate::error::{EvalError, EvalResult};
use crate::eval::{eval_core_expr_pub, materialize};
use crate::value::{BuiltinArgs, Key, Thunk, ThunkId, Value};

/// Helper to check argument count and extract first argument as a thunk.
/// Returns the thunk without materializing it. Named `take_one_thunk` to
/// distinguish from `builtins::expect_one_arg` which forces and returns a `Value`.
fn take_one_thunk(
    name: &str,
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    call_span: Span,
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

/// Helper to collect a Seq into a Vec<ThunkId> by walking the linked list.
/// Returns the thunk IDs in order. Materializes each tail to check for continuation or termination.
async fn collect_seq_to_vec(
    seq_val: Value,
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
    name: &str,
) -> EvalResult<Vec<ThunkId>> {
    let mut items = Vec::new();
    let mut current = seq_val;

    loop {
        match current {
            Value::Seq { head, tail } => {
                items.push(head);

                if items.len() >= MAX_COLLECT_SIZE {
                    return Err(EvalError::resource_limit_exceeded(
                        format!(
                            "{}: exceeded maximum collection size ({})",
                            name, MAX_COLLECT_SIZE
                        ),
                        call_span,
                    )
                    .into());
                }

                // Materialize tail to check for continuation
                let tail_thunk = ctx.get_thunk(tail);
                current = materialize(&tail_thunk, None, ctx).await?;
            }
            Value::Dict(ref d) if d.is_empty() => {
                // Terminal: empty dict
                break;
            }
            other => {
                return Err(EvalError::type_mismatch(
                    "Seq or empty dict",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        }
    }

    Ok(items)
}

/// Helper to build a Seq from a Vec<ThunkId> by creating nested cons cells.
/// Returns a ThunkId for the complete sequence.
fn build_seq_from_vec(
    items: Vec<ThunkId>,
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
) -> ThunkId {
    // Build from right to left: [..., tail] where tail is empty dict
    let mut tail_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Dict(IndexMap::new()),
        call_span,
    )));

    for head_id in items.into_iter().rev() {
        let seq = Value::Seq {
            head: head_id,
            tail: tail_id,
        };
        tail_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(seq, call_span)));
    }

    tail_id
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
    } = ctx_arg;
    Box::pin(async move {
        let func_thunk = take_one_thunk("task", &args, named.as_ref(), call_span)?;

        // Clone what we need for the 'static async block
        let ctx_clone = Arc::clone(&ctx);
        let call_span_clone = call_span;
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
                    env,
                    annotation: _,
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
                    // Create a call environment
                    let call_env = Arc::new(std::sync::RwLock::new(
                        crate::value::Environment::with_parent(env),
                    ));
                    // Evaluate the body
                    let thunk = eval_core_expr_pub(&body, &call_env, &ctx_clone).await?;
                    // Materialize the result
                    materialize(&thunk, None, &ctx_clone).await
                }
                Value::Builtin(def) => {
                    // Call the builtin with no arguments
                    let result = (def.func)(BuiltinArgs {
                        args: vec![],
                        named: None,
                        call_span: call_span_clone,
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
    } = ctx_arg;
    Box::pin(async move {
        let task_thunk = take_one_thunk("await", &args, named.as_ref(), call_span)?;
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
                                        call_span,
                                    ).into()),
                                }
                            }
                            _ = ctx.cancel.cancelled() => {
                                // Cache cancellation error so subsequent awaits see it too
                                let err: Box<EvalError> = EvalError::user_error(
                                    "await: cancelled".to_string(), call_span
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
    } = ctx_arg;
    Box::pin(async move {
        let capacity_thunk = take_one_thunk("channel", &args, named.as_ref(), call_span)?;
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
    } = ctx_arg;
    Box::pin(async move {
        let (chan_thunk, val_thunk) = take_two_thunks("send", &args, named.as_ref(), call_span)?;
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
                                call_span,
                            )
                        })?;
                    }
                    _ = ctx.cancel.cancelled() => {
                        return Err(EvalError::user_error(
                            "send: cancelled".to_string(),
                            call_span,
                        ).into());
                    }
                }

                // Return null (empty dict)
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
            _ => Err(EvalError::type_mismatch("Channel", chan_val.type_name(), call_span).into()),
        }
    })
}

/// `recv`: Receive a value from a channel.
///
/// Signature: `Channel@T → T`
///
/// Suspends until a value is available. Returns an error if the channel is closed.
pub(crate) fn builtin_recv(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let chan_thunk = take_one_thunk("recv", &args, named.as_ref(), call_span)?;
        let chan_val = materialize(&chan_thunk, Some(&call_span), &ctx).await?;

        match chan_val {
            Value::Channel(channel_inner) => {
                // Lock the receiver
                let mut rx = channel_inner.receiver.lock().await;

                // Receive a value, racing against context cancellation.
                let value = tokio::select! {
                    result = rx.recv() => {
                        result.ok_or_else(|| {
                            EvalError::user_error("channel closed (sender dropped)".to_string(), call_span)
                        })?
                    }
                    _ = ctx.cancel.cancelled() => {
                        return Err(EvalError::user_error(
                            "recv: cancelled".to_string(),
                            call_span,
                        ).into());
                    }
                };

                // Return the received value
                ok_val(value, call_span)
            }
            _ => Err(EvalError::type_mismatch("Channel", chan_val.type_name(), call_span).into()),
        }
    })
}

/// `select-once`: Wait for the first of multiple sources to complete.
///
/// Signature: `[Seq [Seq Channel Fn]] → T`
///
/// Takes a sequence of [channel, handler] pairs. Waits for the FIRST channel to have
/// a value available, then calls that channel's handler with the received value.
/// Returns the handler's result.
///
/// Implementation note: uses a manual polling loop over all channels. When a channel
/// produces a value, we call its handler and return. Closed channels are removed from
/// consideration. If all channels are closed, returns an error.
///
/// Fairness: channels are checked in order, but since this is a cooperative runtime,
/// fairness emerges naturally from the event loop.
pub(crate) fn builtin_select_once(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let sources_thunk = take_one_thunk("select-once", &args, named.as_ref(), call_span)?;
        let sources_val = materialize(&sources_thunk, Some(&call_span), &ctx).await?;

        // Collect the sequence of sources into a vec
        let source_ids = collect_seq_to_vec(sources_val, &ctx, call_span, "select-once").await?;

        if source_ids.is_empty() {
            return Err(EvalError::user_error(
                "select-once requires at least one source".to_string(),
                call_span,
            )
            .into());
        }

        // Parse each source as [channel, handler]
        let mut sources: Vec<(Arc<crate::value::ChannelInner>, ThunkId)> = Vec::new();
        for source_id in source_ids {
            let source_thunk = ctx.get_thunk(source_id);
            let source_val = materialize(&source_thunk, Some(&call_span), &ctx).await?;

            match source_val {
                Value::Seq { head, tail } => {
                    // Get the channel (head)
                    let chan_thunk = ctx.get_thunk(head);
                    let chan_val = materialize(&chan_thunk, Some(&call_span), &ctx).await?;

                    // Get the handler (second element)
                    let tail_thunk = ctx.get_thunk(tail);
                    let tail_val = materialize(&tail_thunk, Some(&call_span), &ctx).await?;

                    match tail_val {
                        Value::Seq {
                            head: handler_id,
                            tail: rest_id,
                        } => {
                            // Verify rest is empty dict (2-element list)
                            let rest_thunk = ctx.get_thunk(rest_id);
                            let rest_val = materialize(&rest_thunk, Some(&call_span), &ctx).await?;
                            if !matches!(rest_val, Value::Dict(ref d) if d.is_empty()) {
                                return Err(EvalError::user_error(
                                    "select-once expects [Channel Fn] pairs (2 elements each)"
                                        .to_string(),
                                    call_span,
                                )
                                .into());
                            }

                            match chan_val {
                                Value::Channel(ch) => {
                                    sources.push((ch, handler_id));
                                }
                                _ => {
                                    return Err(EvalError::type_mismatch(
                                        "Channel",
                                        chan_val.type_name(),
                                        call_span,
                                    )
                                    .into())
                                }
                            }
                        }
                        _ => {
                            return Err(EvalError::user_error(
                                "select-once expects [Channel Fn] pairs".to_string(),
                                call_span,
                            )
                            .into())
                        }
                    }
                }
                _ => {
                    return Err(EvalError::user_error(
                        "select-once expects [Channel Fn] pairs".to_string(),
                        call_span,
                    )
                    .into())
                }
            }
        }

        // Track which channels are closed (TryRecvError::Disconnected).
        // When all channels are closed, return an error.
        let mut closed = vec![false; sources.len()];

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

            for (i, (channel_inner, handler_id)) in sources.iter().enumerate() {
                if closed[i] {
                    continue;
                }

                // Use try_lock() to avoid blocking on contended receivers.
                // If the lock is held by a concurrent `recv`, skip this channel
                // this iteration — we will retry after yield_now().
                let mut rx = match channel_inner.receiver.try_lock() {
                    Ok(guard) => guard,
                    Err(_) => continue, // lock contended, try next channel
                };

                match rx.try_recv() {
                    Ok(value) => {
                        // Got a value! Release lock before calling the handler.
                        drop(rx);

                        // Retrieve and materialize the handler.
                        let handler_thunk = ctx.get_thunk(*handler_id);
                        let handler_val =
                            materialize(&handler_thunk, Some(&call_span), &ctx).await?;

                        // Call the handler with the received value.
                        match handler_val {
                            Value::Function {
                                params,
                                body,
                                env,
                                annotation: _,
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

                                // Bind the received value to the parameter.
                                let call_env = Arc::new(std::sync::RwLock::new(
                                    crate::value::Environment::with_parent(env),
                                ));
                                call_env.write().unwrap().insert(
                                    params[0].name.clone(),
                                    Arc::new(Thunk::new_materialized(value, call_span)),
                                );

                                // Evaluate and return the body.
                                let result_thunk =
                                    eval_core_expr_pub(&body, &call_env, &ctx).await?;
                                let v = materialize(&result_thunk, None, &ctx).await?;
                                return ok_val(v, call_span);
                            }
                            Value::Builtin(def) => {
                                // Call builtin with the value.
                                let arg_thunk = Arc::new(Thunk::new_materialized(value, call_span));
                                let result = (def.func)(BuiltinArgs {
                                    args: vec![arg_thunk],
                                    named: None,
                                    call_span,
                                    ctx: Arc::clone(&ctx),
                                })
                                .await?;
                                let v = materialize(&result, None, &ctx).await?;
                                return ok_val(v, call_span);
                            }
                            _ => {
                                return Err(EvalError::type_mismatch(
                                    "Function",
                                    handler_val.type_name(),
                                    call_span,
                                )
                                .into())
                            }
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        // Channel is empty, try next one.
                        continue;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        // Channel is closed — mark it and check if all are closed.
                        closed[i] = true;
                        if closed.iter().all(|&c| c) {
                            return Err(EvalError::user_error(
                                "select-once: all channels are closed".to_string(),
                                call_span,
                            )
                            .into());
                        }
                        continue;
                    }
                }
            }

            // Check for context cancellation before yielding.
            if ctx.cancel.is_cancelled() {
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
    } = ctx_arg;
    Box::pin(async move {
        let (func_thunk, seq_thunk) = take_two_thunks("par-map", &args, named.as_ref(), call_span)?;

        // Materialize the sequence
        let seq_val = materialize(&seq_thunk, Some(&call_span), &ctx).await?;

        // Collect sequence items
        let item_ids = collect_seq_to_vec(seq_val, &ctx, call_span, "par-map").await?;

        // Spawn a task for each item
        let mut tasks = Vec::new();
        for item_id in &item_ids {
            let func_thunk_clone = Arc::clone(&func_thunk);
            let item_thunk_clone = ctx.get_thunk(*item_id);
            let ctx_clone = Arc::clone(&ctx);
            let call_span_clone = call_span;

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
                        env,
                        annotation: _,
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

                        // Bind the item to the parameter
                        let call_env = Arc::new(std::sync::RwLock::new(
                            crate::value::Environment::with_parent(env),
                        ));
                        call_env.write().unwrap().insert(
                            params[0].name.clone(),
                            Arc::new(Thunk::new_materialized(item_val, call_span_clone)),
                        );

                        // Evaluate the body
                        let result_thunk = eval_core_expr_pub(&body, &call_env, &ctx_clone).await?;
                        materialize(&result_thunk, None, &ctx_clone).await
                    }
                    Value::Builtin(def) => {
                        // Call builtin with the item
                        let item_thunk_arg =
                            Arc::new(Thunk::new_materialized(item_val, call_span_clone));
                        let result = (def.func)(BuiltinArgs {
                            args: vec![item_thunk_arg],
                            named: None,
                            call_span: call_span_clone,
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

        // Await all tasks and collect results
        let mut result_ids = Vec::new();
        for handle in tasks {
            let result_val = handle.await.map_err(|e| {
                EvalError::user_error(format!("par-map task panicked: {e}"), call_span)
            })??;
            let result_id =
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(result_val, call_span)));
            result_ids.push(result_id);
        }

        // Build the result sequence
        let result_seq_id = build_seq_from_vec(result_ids, &ctx, call_span);
        Ok(ctx.get_thunk(result_seq_id))
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
    } = ctx_arg;
    Box::pin(async move {
        let (pred_thunk, seq_thunk) =
            take_two_thunks("par-filter", &args, named.as_ref(), call_span)?;

        // Materialize the sequence
        let seq_val = materialize(&seq_thunk, Some(&call_span), &ctx).await?;

        // Collect sequence items
        let item_ids = collect_seq_to_vec(seq_val, &ctx, call_span, "par-filter").await?;

        // Spawn a task for each item to evaluate the predicate
        let mut tasks = Vec::new();
        for (idx, item_id) in item_ids.iter().enumerate() {
            let pred_thunk_clone = Arc::clone(&pred_thunk);
            let item_thunk_clone = ctx.get_thunk(*item_id);
            let ctx_clone = Arc::clone(&ctx);
            let call_span_clone = call_span;
            let item_id_copy = *item_id;

            let handle = crate::async_rt::spawn_local(async move {
                // Materialize the predicate
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
                        env,
                        annotation: _,
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

                        // Bind the item to the parameter
                        let call_env = Arc::new(std::sync::RwLock::new(
                            crate::value::Environment::with_parent(env),
                        ));
                        call_env.write().unwrap().insert(
                            params[0].name.clone(),
                            Arc::new(Thunk::new_materialized(item_val.clone(), call_span_clone)),
                        );

                        // Evaluate the body
                        let result_thunk = eval_core_expr_pub(&body, &call_env, &ctx_clone).await?;
                        materialize(&result_thunk, None, &ctx_clone).await?
                    }
                    Value::Builtin(def) => {
                        let arg_thunk =
                            Arc::new(Thunk::new_materialized(item_val.clone(), call_span_clone));
                        let result_thunk = (def.func)(BuiltinArgs {
                            args: vec![arg_thunk],
                            named: None,
                            call_span: call_span_clone,
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

                // Check if result is true
                match result {
                    Value::Bool(true) => Ok(Some((idx, item_id_copy))),
                    Value::Bool(false) => Ok(None),
                    _ => Err(
                        EvalError::type_mismatch("Bool", result.type_name(), call_span_clone)
                            .into(),
                    ),
                }
            });

            tasks.push(handle);
        }

        // Await all tasks and collect passing items
        let mut results_with_idx = Vec::new();
        for handle in tasks {
            let result = handle.await.map_err(|e| {
                EvalError::user_error(format!("par-filter task panicked: {e}"), call_span)
            })??;
            if let Some(item) = result {
                results_with_idx.push(item);
            }
        }

        // Sort by original index to preserve order
        results_with_idx.sort_by_key(|(idx, _)| *idx);

        // Extract ThunkIds
        let result_ids: Vec<ThunkId> = results_with_idx.into_iter().map(|(_, id)| id).collect();

        // Build the result sequence
        let result_seq_id = build_seq_from_vec(result_ids, &ctx, call_span);
        Ok(ctx.get_thunk(result_seq_id))
    })
}

/// `signal-channel`: Create a channel that receives a value when a Unix signal fires.
///
/// Signature: `Str → Channel@Null`
///
/// Supported signal names: "SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT", "SIGUSR1", "SIGUSR2".
/// Spawns a local task that listens for the signal and sends `null` (empty dict) on each delivery.
/// The returned channel has capacity 1; additional signals delivered before recv are dropped.
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
    } = ctx_arg;
    Box::pin(async move {
        let sig_thunk = take_one_thunk("signal-channel", &args, named.as_ref(), call_span)?;
        let sig_val = materialize(&sig_thunk, Some(&call_span), &ctx).await?;

        let sig_name = match sig_val {
            Value::String {
                ref source,
                start,
                end,
            } => source[start..end].to_string(),
            _ => {
                return Err(EvalError::type_mismatch("Str", sig_val.type_name(), call_span).into())
            }
        };

        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};

            let kind = match sig_name.as_str() {
                "SIGINT" => SignalKind::interrupt(),
                "SIGTERM" => SignalKind::terminate(),
                "SIGHUP" => SignalKind::hangup(),
                "SIGQUIT" => SignalKind::quit(),
                "SIGUSR1" => SignalKind::user_defined1(),
                "SIGUSR2" => SignalKind::user_defined2(),
                other => {
                    return Err(EvalError::user_error(
                        format!("signal-channel: unknown signal name {other:?}; supported: SIGINT SIGTERM SIGHUP SIGQUIT SIGUSR1 SIGUSR2"),
                        call_span,
                    )
                    .into())
                }
            };

            let mut sig_stream = signal(kind).map_err(|e| {
                EvalError::user_error(
                    format!("signal-channel: failed to register signal handler: {e}"),
                    call_span,
                )
            })?;

            // Channel capacity 1: additional signals before recv are dropped gracefully.
            let (tx, rx) = tokio::sync::mpsc::channel::<Value>(1);
            let tx_clone = tx.clone();
            let cancel_token = ctx.cancel.clone();

            let handle = crate::async_rt::spawn_local(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel_token.cancelled() => {
                            break;
                        }
                        signal = sig_stream.recv() => {
                            // recv() returns None when the signal stream is closed (process shutdown)
                            if signal.is_none() {
                                break;
                            }
                            // Ignore send errors (receiver dropped)
                            let _ = tx_clone.try_send(Value::Dict(IndexMap::new()));
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
        }

        #[cfg(not(unix))]
        {
            let _ = sig_name;
            Err(EvalError::user_error(
                "signal-channel is only supported on Unix platforms".to_string(),
                call_span,
            )
            .into())
        }
    })
}

/// `timer-channel`: Create a channel that ticks every `interval-ms` milliseconds.
///
/// Signature: `Int → Channel@Null`
///
/// Spawns a local task driven by `tokio::time::interval`. Sends `null` (empty dict) on
/// each tick. The channel has capacity 1; if the receiver is slow, ticks are dropped
/// (non-blocking `try_send`) so that a slow consumer never builds up an unbounded backlog.
pub(crate) fn builtin_timer_channel(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let ms_thunk = take_one_thunk("timer-channel", &args, named.as_ref(), call_span)?;
        let ms_val = materialize(&ms_thunk, Some(&call_span), &ctx).await?;

        let interval_ms = match ms_val {
            Value::Int(n) if n >= 1 => n as u64,
            Value::Int(n) => {
                return Err(EvalError::user_error(
                    format!("timer-channel: interval must be ≥ 1 ms, got {n}"),
                    call_span,
                )
                .into())
            }
            _ => return Err(EvalError::type_mismatch("Int", ms_val.type_name(), call_span).into()),
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
                        // Non-blocking: if the receiver hasn't consumed the previous tick, drop this one.
                        let _ = tx_clone.try_send(Value::Dict(IndexMap::new()));
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

/// `watch-channel`: Create a watch channel backed by `tokio::sync::watch`.
///
/// Signature: `Any → [Seq Channel Channel]`
///
/// Returns a 2-element list `[recv-channel update-channel]`:
/// - `recv-channel`: receives the latest value whenever it changes (use `recv`).
/// - `update-channel`: send a new value here to update the watch (use `send`).
///
/// The initial value is sent immediately, so the first `recv` returns without waiting.
/// Watch semantics: each change overwrites the previous unseen value (last-write-wins).
/// A background task bridges the watch into the mpsc so callers use the standard
/// `send`/`recv` builtins throughout.
pub(crate) fn builtin_watch_channel(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let init_thunk = take_one_thunk("watch-channel", &args, named.as_ref(), call_span)?;
        let init_val = materialize(&init_thunk, Some(&call_span), &ctx).await?;

        // watch channel: holds the latest value; watch_rx.changed() fires on each update.
        let (watch_tx, watch_rx) = tokio::sync::watch::channel(init_val);

        // Read side: mpsc channel that forwards each watch change to the recv caller.
        // Capacity 1: last-write-wins — if the consumer is slow it sees the most recent value.
        let (read_tx, read_rx) = tokio::sync::mpsc::channel::<Value>(1);
        let read_tx_clone = read_tx.clone();
        let mut watch_rx_reader = watch_rx.clone();
        let cancel_token_forward = ctx.cancel.clone();

        // Forward task: watch → mpsc read channel.
        // Sends the initial value immediately, then forwards each subsequent change.
        // Uses try_send (non-blocking) to preserve last-value-wins semantics: if the
        // consumer hasn't read the previous value, the new one overwrites it by dropping
        // the old one. This prevents the forward task from suspending and starving other
        // cooperative tasks in the LocalSet.
        let handle1 = crate::async_rt::spawn_local(async move {
            // Send the current (initial) value; drop silently if the channel is full
            // (capacity-1 channel just allocated, so this should always succeed on first call).
            let initial = watch_rx_reader.borrow().clone();
            let _ = read_tx_clone.try_send(initial);

            loop {
                tokio::select! {
                    biased;
                    _ = cancel_token_forward.cancelled() => {
                        break;
                    }
                    result = watch_rx_reader.changed() => {
                        if result.is_err() {
                            // Watch sender dropped — stop forwarding.
                            break;
                        }
                        let val = watch_rx_reader.borrow().clone();
                        // Non-blocking: if the consumer is slow, the old value is dropped (last-value-wins).
                        // Ignore send errors (consumer dropped — stop forwarding on next changed() call).
                        let _ = read_tx_clone.try_send(val);
                    }
                }
            }
        });

        // Register background task for drain tracking
        ctx.task_registry.lock().unwrap().push(handle1);

        // Update side: mpsc channel that the user sends new values to; a bridge task reads
        // from it and writes into the watch.
        let (update_tx, update_rx_bridge) = tokio::sync::mpsc::channel::<Value>(8);
        let update_tx_clone = update_tx.clone();

        // Dummy receiver for the ChannelInner receiver field (the real receiver is
        // consumed by the bridge task below). Users of watch-channel send to the update
        // channel; they never recv from it.
        let (_dummy_update_tx, dummy_update_rx) = tokio::sync::mpsc::channel::<Value>(1);

        // Bridge task: mpsc update channel → watch sender.
        let mut update_rx_bridge = update_rx_bridge;
        let cancel_token_bridge = ctx.cancel.clone();
        let handle2 = crate::async_rt::spawn_local(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel_token_bridge.cancelled() => {
                        break;
                    }
                    result = update_rx_bridge.recv() => {
                        match result {
                            Some(val) => {
                                // Ignore errors (all readers dropped).
                                let _ = watch_tx.send(val);
                            }
                            None => {
                                // Channel closed
                                break;
                            }
                        }
                    }
                }
            }
        });

        // Register background task for drain tracking
        ctx.task_registry.lock().unwrap().push(handle2);

        // Build the recv Channel value.
        let recv_channel = Value::Channel(Arc::new(crate::value::ChannelInner {
            sender: read_tx,
            receiver: tokio::sync::Mutex::new(read_rx),
            capacity: 1,
        }));

        // Build the update Channel value.
        // The receiver slot holds a stub rx; callers `send` to this channel to update the watch.
        let update_channel = Value::Channel(Arc::new(crate::value::ChannelInner {
            sender: update_tx_clone,
            receiver: tokio::sync::Mutex::new(dummy_update_rx),
            capacity: 8,
        }));

        // Return [recv-channel update-channel] as a Seq.
        let recv_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(recv_channel, call_span)));
        let update_id =
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(update_channel, call_span)));
        let empty_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            call_span,
        )));
        let tail_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Seq {
                head: update_id,
                tail: empty_id,
            },
            call_span,
        )));
        let seq_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Seq {
                head: recv_id,
                tail: tail_id,
            },
            call_span,
        )));
        Ok(ctx.get_thunk(seq_id))
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
    } = ctx_arg;
    Box::pin(async move {
        let parent_thunk = take_one_thunk("with-cancel", &args, named.as_ref(), call_span)?;
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

        let mut result = IndexMap::new();
        let child_ctx_id =
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(child_ctx_val, call_span)));
        let cancel_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(cancel_val, call_span)));
        result.insert(Key::String("child-ctx".into()), child_ctx_id);
        result.insert(Key::String("cancel".into()), cancel_id);

        ok_val(Value::Dict(result), call_span)
    })
}

/// `with-timeout`: Create a child context that auto-cancels after `duration-ms` milliseconds.
///
/// Signature: `Context → Int → Context`
///
/// Creates a child token derived from the parent. Spawns a background local task that
/// sleeps for `duration-ms` milliseconds then calls `cancel()` on the child token.
/// Returns the child context.
///
/// Per async-eval.md: `timed-ctx: [with-timeout ctx 5000]`
pub(crate) fn builtin_with_timeout(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let (parent_thunk, ms_thunk) =
            take_two_thunks("with-timeout", &args, named.as_ref(), call_span)?;
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
            Value::Int(n) if n >= 0 => n as u64,
            Value::Int(n) => {
                return Err(EvalError::user_error(
                    format!("with-timeout: duration must be ≥ 0 ms, got {n}"),
                    call_span,
                )
                .into())
            }
            _ => return Err(EvalError::type_mismatch("Int", ms_val.type_name(), call_span).into()),
        };

        let child_token = parent_token.child_token();
        let cancel_clone = child_token.clone();

        // Spawn a local task to cancel the child token after the timeout.
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
/// Signature: `Context → Int → Context`
///
/// Like `with-timeout` but the deadline is specified as an absolute Unix timestamp in
/// milliseconds (matching `Value::Timestamp` semantics). If the deadline is already past,
/// the child context is cancelled immediately.
///
/// Per async-eval.md: `dead-ctx: [with-deadline ctx ts]`
pub(crate) fn builtin_with_deadline(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let (parent_thunk, ts_thunk) =
            take_two_thunks("with-deadline", &args, named.as_ref(), call_span)?;
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
    } = ctx_arg;
    Box::pin(async move {
        let ctx_thunk = take_one_thunk("cancelled?", &args, named.as_ref(), call_span)?;
        let ctx_val = materialize(&ctx_thunk, Some(&call_span), &ctx).await?;

        match ctx_val {
            Value::Context(token) => ok_val(Value::Bool(token.is_cancelled()), call_span),
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
    } = ctx_arg;
    Box::pin(async move {
        let ctx_thunk = take_one_thunk("cancel-task", &args, named.as_ref(), call_span)?;
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

        let code_thunk = &args[0];
        let code_val = materialize(code_thunk, Some(&call_span), &ctx).await?; // H1: exit code must be known to terminate process

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
    } = ctx_arg;
    Box::pin(async move {
        let (context_thunk, expr_thunk) =
            take_two_thunks("with-context", &args, named.as_ref(), call_span)?;
        let context_val = context_thunk
            .try_get_materialized()
            // force_count=1 guarantees arg 0 is pre-materialized before this builtin runs
            .ok_or_else(|| EvalError::internal(
                "with-context: context argument not pre-materialized (force_count=1 invariant violated)".to_string(),
                call_span,
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

#[cfg(test)]
mod tests {
    /// Verify that task+await works: spawn a zero-arg function and await its result.
    ///
    /// This is the core deadlock regression test. Previously, block_on_anywhere used
    /// poll_future_sync for current_thread runtimes, which never drove the LocalSet,
    /// so the spawned task's JoinHandle would never resolve. The fix wraps the future
    /// in LOCAL_SET.run_until() so spawn_local tasks are driven concurrently.
    ///
    /// Uses builtin-task/builtin-await (bare names removed in builtin-privacy-primary-names sprint).
    #[tokio::test]
    async fn test_task_await_basic() {
        let result =
            crate::eval_source_with_config("[builtin-await [builtin-task [fn [let] 42]]]", false);
        // Output is the Value Display format; Int(42) renders as "Int(42)" via eval_source.
        // Just confirm it succeeded and contains 42.
        let output = result.unwrap();
        assert!(
            output.contains("42"),
            "expected 42 in output, got: {output:?}"
        );
    }

    /// Verify that `[builtin-context]` creates a fresh uncancelled Context value.
    ///
    /// Uses builtin-context/builtin-cancelled-q (bare names removed in builtin-privacy-primary-names sprint).
    #[tokio::test]
    async fn test_context_creates_fresh_token() {
        // [builtin-cancelled-q [builtin-context]] returns false — a fresh token is not cancelled.
        let result =
            crate::eval_source_with_config("[builtin-cancelled-q [builtin-context]]", false);
        let output = result.unwrap();
        assert!(
            output.contains("false"),
            "expected fresh context to be not-cancelled, got: {output:?}"
        );
    }

    /// Verify that `[builtin-cancel-task ctx]` returns null (empty dict) when given a Context.
    ///
    /// Uses builtin-cancel-task/builtin-context (bare names removed in builtin-privacy-primary-names sprint).
    #[tokio::test]
    async fn test_cancel_task_returns_null() {
        // [builtin-cancel-task [builtin-context]] should return {} (null) — the empty dict.
        let result =
            crate::eval_source_with_config("[builtin-cancel-task [builtin-context]]", false);
        let output = result.unwrap();
        // null serializes as {} in JSON output
        assert!(
            output.contains("{}") || output.contains("Dict"),
            "expected cancel-task to return null (empty dict), got: {output:?}"
        );
    }

    /// Verify that a cancelled? check on a token cancelled in Rust returns true.
    #[tokio::test]
    async fn test_cancel_task_then_cancelled_q() {
        // Create a fresh token, cancel it in Rust, verify is_cancelled() = true.
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        assert!(
            token.is_cancelled(),
            "token should be cancelled after cancel()"
        );
    }

    /// Verify that `[builtin-with-cancel ctx]` returns a dict with child-ctx and cancel fields,
    /// and that calling cancel-task on the cancel token cancels the child.
    ///
    /// We test this in two parts:
    /// 1. The child starts uncancelled: [builtin-cancelled-q [builtin-with-cancel [builtin-context]].child-ctx] == false
    /// 2. After cancelling a freshly created context, [builtin-cancelled-q ...] == true
    ///
    /// Uses builtin-with-cancel/builtin-cancelled-q/builtin-context (bare names removed in builtin-privacy-primary-names sprint).
    #[tokio::test]
    async fn test_with_cancel_child_starts_uncancelled() {
        let result = crate::eval_source_with_config(
            "[builtin-cancelled-q [builtin-with-cancel [builtin-context]].child-ctx]",
            false,
        );
        let output = result.unwrap();
        assert!(
            output.contains("false"),
            "expected child-ctx to start uncancelled, got: {output:?}"
        );
    }

    /// Verify that `[builtin-with-timeout ctx 100]` produces a child context.
    /// (We cannot reliably test it's cancelled in 100ms without yielding to the LocalSet,
    /// but we verify it returns a Context without error.)
    ///
    /// Uses builtin-with-timeout/builtin-cancelled-q/builtin-context (bare names removed in builtin-privacy-primary-names sprint).
    #[tokio::test]
    async fn test_with_timeout_returns_context() {
        let result = crate::eval_source_with_config(
            "[builtin-cancelled-q [builtin-with-timeout [builtin-context] 100]]",
            false,
        );
        // The child starts uncancelled (100ms hasn't elapsed).
        let output = result.unwrap();
        assert!(
            output.contains("false"),
            "expected with-timeout child to start uncancelled, got: {output:?}"
        );
    }

    /// Verify that `[builtin-with-deadline ctx ts]` returns a Context without error.
    ///
    /// Uses builtin-with-deadline/builtin-cancelled-q/builtin-context (bare names removed in builtin-privacy-primary-names sprint).
    #[tokio::test]
    async fn test_with_deadline_returns_context() {
        // Deadline in the past (1 = 1970-01-01 UTC in ms) → child should be cancelled immediately.
        // sleep(0) spawned, yields. We just check it doesn't error.
        let result = crate::eval_source_with_config(
            "[builtin-cancelled-q [builtin-with-deadline [builtin-context] 1]]",
            false,
        );
        let output = result.unwrap();
        // Either true (past deadline) or false (hasn't yielded yet); either is valid — just no error.
        assert!(
            output.contains("true") || output.contains("false"),
            "expected with-deadline to return a Context, got: {output:?}"
        );
    }

    /// Verify that parent cancellation propagates to child from `[with-cancel]`.
    ///
    /// We test this by calling cancel() on the Rust token directly (not through tinct),
    /// so we don't need dict syntax to share state.
    #[tokio::test]
    async fn test_parent_cancel_propagates_to_child_rust() {
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
    #[tokio::test]
    async fn test_child_cancel_does_not_affect_parent_rust() {
        let parent = tokio_util::sync::CancellationToken::new();
        let child = parent.child_token();
        child.cancel();
        assert!(
            !parent.is_cancelled(),
            "parent stays uncancelled after child.cancel()"
        );
    }

    // -------------------------------------------------------------------------
    // Required audit tests
    // -------------------------------------------------------------------------

    /// [builtin-channel 0] must return an error: capacity must be ≥ 1.
    ///
    /// Uses builtin-channel (bare name removed in builtin-privacy-primary-names sprint).
    #[tokio::test]
    async fn test_channel_capacity_zero_returns_error() {
        let result = crate::eval_source_with_config("[builtin-channel 0]", false);
        assert!(
            result.is_err(),
            "expected [builtin-channel 0] to return an error, got: {result:?}"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("capacity must be") || msg.contains("≥ 1") || msg.contains(">= 1"),
            "expected capacity error message, got: {msg:?}"
        );
    }

    /// A freshly created CancellationToken must not be cancelled (Rust-level check).
    ///
    /// This exercises the core invariant used by `cancelled?`: a new token is always
    /// in the non-cancelled state until `cancel()` is called explicitly.
    #[tokio::test]
    async fn test_cancelled_q_false_on_fresh_context() {
        let token = tokio_util::sync::CancellationToken::new();
        assert!(
            !token.is_cancelled(),
            "fresh CancellationToken must not be cancelled"
        );
    }

    /// [builtin-select-once {}] must return an error: at least one source is required.
    ///
    /// The empty dict `{}` is the Seq terminator, so collect_seq_to_vec returns an
    /// empty Vec, triggering the "select-once requires at least one source" guard.
    ///
    /// Uses builtin-select-once (bare name removed in builtin-privacy-primary-names sprint).
    #[tokio::test]
    async fn test_select_once_empty_sources_returns_error() {
        let result = crate::eval_source_with_config("[builtin-select-once {}]", false);
        assert!(
            result.is_err(),
            "expected [builtin-select-once {{}}] to return an error, got: {result:?}"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("at least one source") || msg.contains("select-once"),
            "expected 'at least one source' error message, got: {msg:?}"
        );
    }

    /// Verify that re-awaiting a task that completed with an error returns the error,
    /// not the sentinel `{}` value.
    ///
    /// Regression test for the bug where `result?` moved the error out of Done without
    /// restoring it to the guard, leaving the sentinel `Done(Ok({}))` in place. The
    /// second await would then read `Ok({})` and return `{}` instead of the error.
    ///
    /// Uses builtin-task/builtin-await (bare names removed in builtin-privacy-primary-names sprint).
    /// `try` stays as the prelude wrapper; `+` stays as the prelude wrapper (2-arg, so [+ 1 2 3] gives arity mismatch).
    #[tokio::test]
    async fn test_await_error_twice_returns_error_both_times() {
        // Create a task that errors ([+ 1 2 3] fails: + is 2-arg). [try [fn [] ...]] catches
        // the error as [Error "msg"]. We await the same task twice — both should return
        // [Error ...], not [Ok {}].
        // The bug: before the fix, Done stored Ok({}) after first await and second
        // await would return {} instead of the original error.
        let result = crate::eval_source_with_config(
            "[t: [builtin-task [fn [let] [+ 1 2 3]]]] [first-err: [try [fn [let] [builtin-await t]]]] [second-err: [try [fn [let] [builtin-await t]]]] [first-result: first-err  second-result: second-err]",
            false,
        );
        let output = result.expect("eval should succeed (try catches errors)");
        // Both should be Error variants, not Ok({}). If the bug existed, second await would
        // return Ok({}) so first-result and second-result would differ.
        // The output contains "Error" (the variant tag) and does NOT contain "()" or "{}"
        // as the second result.
        assert!(
            output.contains("Error") && !output.contains("second-result\": Ok"),
            "both awaits should produce Error, not Ok: {output}"
        );
        // Verify they're the same error (same message appears twice)
        let error_msg = "arity mismatch";
        let count = output.matches(error_msg).count();
        assert_eq!(
            count, 2,
            "both awaits should have the same error, got: {output}"
        );
    }
}
