//! Shared async runtime for QUIC/HTTP3 builtins.
//! Single-threaded current_thread runtime initialized once per thread.
//! reqwest's blocking client has its own internal async runtime — no conflict.
//!
//! Design notes:
//! - `block_on` drives a `current_thread` tokio runtime. Every call polls all
//!   spawned tasks cooperatively, so background driver tasks (e.g. the h3
//!   `Connection` driver) make progress on each `block_on` call.
//! - `spawn_local` queues a `!Send` future as a local task on the `LocalSet`
//!   that wraps every `block_on` call. This avoids `Send` bounds for quinn/h3
//!   types that don't implement `Send`.

use std::future::Future;

std::thread_local! {
    static TOKIO_RT: tokio::runtime::Runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("async_rt: failed to create tokio runtime");

    /// LocalSet shared across all `block_on` calls on this thread.
    /// Futures spawned via `spawn_local` are polled whenever `block_on_local` drives the set.
    static LOCAL_SET: tokio::task::LocalSet = tokio::task::LocalSet::new();
}

/// Drive `fut` to completion on the thread-local tokio runtime, also polling
/// any local tasks that were spawned via [`spawn_local`].
pub fn block_on<F: Future>(fut: F) -> F::Output {
    TOKIO_RT.with(|rt| LOCAL_SET.with(|ls| ls.block_on(rt, fut)))
}

/// Spawn a `!Send + 'static` future as a local task.
///
/// The task is polled cooperatively on the next (and every subsequent)
/// [`block_on`] call on this thread. Use this for background driver tasks
/// (e.g. the HTTP/3 connection driver) that must run concurrently with requests.
///
/// Returns a `JoinHandle` that can be stored to keep the task alive; dropping
/// the handle detaches the task (it continues running until it completes or
/// the runtime shuts down).
pub fn spawn_local<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    LOCAL_SET.with(|ls| {
        TOKIO_RT.with(|rt| {
            let _guard = rt.enter();
            ls.spawn_local(fut)
        })
    })
}
