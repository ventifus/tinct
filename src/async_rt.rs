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
/// runtime from within a runtime". This variant detects the runtime flavor and adapts:
///
/// - **Multi-thread runtime**: uses `block_in_place` to step the current thread out of
///   the async context before driving the future synchronously.
/// - **Current-thread runtime** (e.g. `#[tokio::test]` or the test helper `mat()`):
///   `block_in_place` is not supported. Uses a minimal spin-poll executor that drives
///   the future to completion without entering any tokio runtime context. This works
///   correctly for pure-compute futures (eval, materialize) that have no I/O awaits.
/// - **No runtime** (called from a plain sync thread): creates a fresh `current_thread`
///   runtime directly.
///
/// Use this for pure eval futures (`materialize`, `eval`, `invoke_function`) that do
/// not need the `LocalSet` background driver. IO builtins that require `LocalSet`
/// (QUIC, HTTP/3 driver tasks) must continue to use [`block_on`].
pub fn block_on_anywhere<F>(fut: F) -> F::Output
where
    F: std::future::Future,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            use tokio::runtime::RuntimeFlavor;
            if handle.runtime_flavor() == RuntimeFlavor::MultiThread {
                tokio::task::block_in_place(|| handle.block_on(fut))
            } else {
                // current_thread runtime: block_in_place panics, and creating a new
                // Runtime::block_on also panics ("cannot start a runtime from within a
                // runtime"). Use a minimal spin-poll executor that is completely
                // independent of tokio's runtime context. This is correct for
                // pure-compute eval/materialize futures that have no real I/O awaits.
                poll_future_sync(fut)
            }
        }
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("async_rt: failed to create tokio runtime");
            rt.block_on(fut)
        }
    }
}

/// Drive a future to completion using a minimal spin-poll executor.
///
/// This polls the future in a tight loop with a no-op waker until it returns
/// `Poll::Ready`. It does NOT enter any tokio runtime context, making it safe
/// to call from within a `current_thread` tokio runtime.
///
/// Only use this for futures that are known to be purely synchronous under the
/// hood (i.e., all `.await` points resolve immediately without blocking on I/O).
/// The eval/materialize futures satisfy this property.
fn poll_future_sync<F: Future>(fut: F) -> F::Output {
    use std::pin::Pin;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    // No-op waker: wake() does nothing because we spin unconditionally.
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |ptr| RawWaker::new(ptr, &VTABLE), // clone
        |_| {},                            // wake
        |_| {},                            // wake_by_ref
        |_| {},                            // drop
    );
    let raw = RawWaker::new(std::ptr::null(), &VTABLE);
    // SAFETY: the vtable functions are all no-ops or return a valid RawWaker.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);

    let mut fut = std::pin::pin!(fut);
    loop {
        match Pin::new(&mut fut).poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {
                // Spin: for pure-compute futures, Pending is transient and
                // the next poll will make progress.
                std::hint::spin_loop();
            }
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
