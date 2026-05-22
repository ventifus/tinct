//! Async concurrency primitives: task, await, channel, send, recv.
//!
//! Design notes (from doc/whatif/runtime-v2.md):
//! - `task` spawns a concurrent evaluation via tokio::task::spawn_local
//! - `await` blocks until the task completes, returns its result
//! - `channel N` creates a bounded channel with capacity N (minimum 1)
//! - `send chan value` sends a value on the channel (suspends if full)
//! - `recv chan` receives a value from the channel (suspends until available)
//!
//! Current implementation constraints:
//! - Value::Task, Value::Channel, Value::Context are skeleton variants
//! - Full Arc/OnceCell thunk implementation is deferred to runtime-v2 Sprint 2
//! - This module provides minimal working implementations for pre-1.0

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::ok_val;
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize_sync as materialize;
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
/// The task builtin takes a thunk (typically a zero-arg function) and spawns it
/// for concurrent evaluation via tokio::task::spawn_local.
///
/// Per runtime-v2.md: `spawn_local` fires when the `task` expression itself is
/// materialized — not when `await` demands the handle. An undemanded task thunk
/// is never spawned.
///
/// Current implementation: Returns Value::Task skeleton. Full implementation
/// requires OnceCell-based TaskState (runtime-v2 Sprint 2, Part B).
pub(crate) fn builtin_task(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let _thunk = expect_one_arg("task", &args, named.as_ref(), call_span)?;

        // TODO (runtime-v2 Sprint 2, Part B):
        // 1. Clone thunk and ctx into 'static-capable owned values
        // 2. Spawn via tokio::task::spawn_local(async move { materialize(thunk, ...).await })
        // 3. Store JoinHandle in TaskState::Pending
        // 4. Return Value::Task(Arc::new(Mutex::new(TaskState::Pending(handle))))
        //
        // For now: return skeleton Value::Task
        ok_val(Value::Task, call_span)
    })
}

/// `await`: Block until a task completes and return its result.
///
/// Signature: `Task@T → T`
///
/// Suspends the caller until the task finishes, then returns the task's result.
/// Propagates any error from the task.
///
/// Current implementation: Returns null for skeleton Value::Task. Full
/// implementation requires polling TaskState::Pending JoinHandle.
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
        let task_val = materialize(&task_thunk, Some(&call_span), &ctx)?;

        match &task_val {
            Value::Task => {
                // TODO (runtime-v2 Sprint 2, Part B):
                // 1. Lock the TaskState mutex
                // 2. Match on state:
                //    - Pending(handle): handle.await?, move result to Done, return result
                //    - Done(value): return value.clone()
                // 3. Propagate cancellation via ctx.cancel.cancelled().await
                //
                // For now: return null for skeleton
                ok_val(Value::Dict(IndexMap::new()), call_span)
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
///
/// Current implementation: Returns Value::Channel skeleton. Full implementation
/// requires tokio::sync::mpsc::channel and ChannelInner.
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
        let capacity_val = materialize(&capacity_thunk, Some(&call_span), &ctx)?;

        match &capacity_val {
            Value::Int(n) if n >= &1 => {
                // TODO (runtime-v2 Sprint 2, Part B):
                // 1. let (tx, rx) = tokio::sync::mpsc::channel(*n as usize);
                // 2. Create ChannelInner { tx, rx: Mutex::new(rx), capacity: *n, background_task: None }
                // 3. Return Value::Channel(Arc::new(channel_inner))
                //
                // For now: return skeleton
                ok_val(Value::Channel, call_span)
            }
            Value::Int(n) if n < &1 => Err(EvalError::user_error(
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
///
/// Current implementation: Returns null for skeleton Value::Channel. Full
/// implementation requires tokio::sync::mpsc::Sender::send().await.
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
        let (chan_thunk, _val_thunk) = expect_two_args("send", &args, named.as_ref(), call_span)?;
        let chan_val = materialize(&chan_thunk, Some(&call_span), &ctx)?;

        match &chan_val {
            Value::Channel => {
                // TODO (runtime-v2 Sprint 2, Part B):
                // 1. Materialize val_thunk to a Value
                // 2. channel_inner.tx.send(value).await
                // 3. Check ctx.cancel.cancelled().await for cancellation
                // 4. Return null
                //
                // For now: return null for skeleton
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
///
/// Current implementation: Returns null for skeleton Value::Channel. Full
/// implementation requires tokio::sync::mpsc::Receiver::recv().await.
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
        let chan_val = materialize(&chan_thunk, Some(&call_span), &ctx)?;

        match &chan_val {
            Value::Channel => {
                // TODO (runtime-v2 Sprint 2, Part B):
                // 1. Lock channel_inner.rx mutex
                // 2. rx.recv().await → Some(value) | None
                // 3. Check ctx.cancel.cancelled().await for cancellation
                // 4. Return value or error if channel closed
                //
                // For now: return null for skeleton
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
            _ => Err(EvalError::type_mismatch("Channel", chan_val.type_name(), call_span).into()),
        }
    })
}
