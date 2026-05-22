//! Async concurrency primitives: task, await, channel, send, recv.
//!
//! Design notes (from doc/whatif/runtime-v2.md):
//! - `task` spawns a concurrent evaluation via tokio::task::spawn_local
//! - `await` blocks until the task completes, returns its result
//! - `channel N` creates a bounded channel with capacity N (minimum 1)
//! - `send chan value` sends a value on the channel (suspends if full)
//! - `recv chan` receives a value from the channel (suspends until available)
//!
//! Current implementation: real tokio::sync::mpsc channels and tokio::task::spawn_local tasks.
//! Value::Context remains a skeleton; full cancellation context is deferred.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::ok_val;
use crate::error::{EvalError, EvalResult};
use crate::eval::{eval, materialize};
use crate::value::{BuiltinArgs, Thunk, Value};

/// Helper to check argument count and extract first argument.
fn expect_one_arg(
    name: &str,
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    call_span: Span,
) -> EvalResult<Arc<Thunk>> {
    if !named.as_ref().map_or(true, |n| n.is_empty()) {
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

/// Helper to check argument count for two arguments.
fn expect_two_args(
    name: &str,
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    call_span: Span,
) -> EvalResult<(Arc<Thunk>, Arc<Thunk>)> {
    if !named.as_ref().map_or(true, |n| n.is_empty()) {
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
        let func_thunk = expect_one_arg("task", &args, named.as_ref(), call_span)?;

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
                    let thunk = eval(body, call_env, &ctx_clone).await?;
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
        let task_thunk = expect_one_arg("await", &args, named.as_ref(), call_span)?;
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
                // NOTE (FIX LATER): the error-path has a subtle issue — if the task errors,
                // `result?` moves the error out of Done, leaving the sentinel in place. A
                // subsequent await on the same errored Task returns {} instead of the error.
                // Fix: use Done(Result<Value, Arc<EvalError>>) so errors can be cloned and
                // re-returned. Tracked in TODO.md.
                match std::mem::replace(
                    &mut *guard,
                    crate::value::TaskState::Done(Ok(Value::Dict(IndexMap::new()))),
                ) {
                    crate::value::TaskState::Pending(handle) => {
                        // Await the handle
                        let result = handle.await.map_err(|e| {
                            EvalError::user_error(format!("task panicked: {e}"), call_span)
                        })??;

                        // Cache the result
                        *guard = crate::value::TaskState::Done(Ok(result.clone()));

                        // Return the result
                        ok_val(result, call_span)
                    }
                    crate::value::TaskState::Done(result) => {
                        // Task already completed, return cached result
                        let val = result?;
                        *guard = crate::value::TaskState::Done(Ok(val.clone()));
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
        let capacity_thunk = expect_one_arg("channel", &args, named.as_ref(), call_span)?;
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
        let (chan_thunk, val_thunk) = expect_two_args("send", &args, named.as_ref(), call_span)?;
        let chan_val = materialize(&chan_thunk, Some(&call_span), &ctx).await?;

        match chan_val {
            Value::Channel(channel_inner) => {
                // Materialize the value to send
                let value = materialize(&val_thunk, Some(&call_span), &ctx).await?;

                // Send the value
                channel_inner.sender.send(value).await.map_err(|_| {
                    EvalError::user_error(
                        "channel closed (receiver dropped)".to_string(),
                        call_span,
                    )
                })?;

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
        let chan_thunk = expect_one_arg("recv", &args, named.as_ref(), call_span)?;
        let chan_val = materialize(&chan_thunk, Some(&call_span), &ctx).await?;

        match chan_val {
            Value::Channel(channel_inner) => {
                // Lock the receiver
                let mut rx = channel_inner.receiver.lock().await;

                // Receive a value
                let value = rx.recv().await.ok_or_else(|| {
                    EvalError::user_error("channel closed (sender dropped)".to_string(), call_span)
                })?;

                // Return the received value
                ok_val(value, call_span)
            }
            _ => Err(EvalError::type_mismatch("Channel", chan_val.type_name(), call_span).into()),
        }
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
    #[tokio::test]
    async fn test_task_await_basic() {
        let result = crate::eval_source_with_config("[await [task [fn [] 42]]]", false);
        // Output is the Value Display format; Int(42) renders as "Int(42)" via eval_source.
        // Just confirm it succeeded and contains 42.
        let output = result.unwrap();
        assert!(
            output.contains("42"),
            "expected 42 in output, got: {output:?}"
        );
    }
}
