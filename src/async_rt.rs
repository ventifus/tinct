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

/// Like [`block_on`] but safe to call from within an existing tokio runtime context.
///
/// When already inside a tokio runtime (e.g. called from a `#[tokio::test]` or from
/// `reqwest::blocking` internals), `LocalSet::block_on` panics with "cannot start a
/// runtime from within a runtime". This variant uses `block_in_place` to step the
/// current thread out of the async context before driving the future synchronously.
///
/// Use this for pure eval futures (`materialize`, `eval`, `invoke_function`) that do
/// not need the `LocalSet` background driver. IO builtins that require `LocalSet`
/// (QUIC, HTTP/3 driver tasks) must continue to use [`block_on`].
///
/// Falls back to a fresh `Runtime` when called outside any tokio context.
///
/// # Panics
///
/// Panics if called from within a `current_thread` tokio runtime context, such as
/// from within [`block_on`] or a `spawn_local` task. `block_in_place` requires a
/// multi-thread (work-stealing) runtime — calling it inside a `current_thread` runtime
/// (e.g. `#[tokio::test(flavor = "current_thread")]`) will panic at runtime.
/// All callers must be invoked outside any `block_on` context.
pub fn block_on_anywhere<F>(fut: F) -> F::Output
where
    F: std::future::Future,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => {
            let rt =
                tokio::runtime::Runtime::new().expect("async_rt: failed to create tokio runtime");
            rt.block_on(fut)
        }
    }
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
