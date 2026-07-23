//! Shared async runtime for QUIC/HTTP3 builtins and the async reqwest HTTP/2 client.
//! Multi-threaded tokio runtime. BuiltinFn futures are `+ Send` (T-1841).
//! Both the h3 QUIC stack and reqwest's async client (used by Http2Session) share this runtime.
//!
//! Design notes:
//! - `block_on` drives a multi-thread tokio runtime (new_multi_thread). Every call polls all
//!   spawned tasks cooperatively, so background driver tasks (e.g. the h3
//!   `Connection` driver) make progress on each `block_on` call.
//! - `spawn_local` remains for the sole `!Send` use: the h3 `Connection` driver task in
//!   builtins_net.rs. All other spawned tasks use `tokio::spawn` directly.
//!   The LocalSet wraps `block_on` so the h3 driver is driven on each call.

use std::future::Future;

std::thread_local! {
    static TOKIO_RT: tokio::runtime::Runtime = tokio::runtime::Builder::new_multi_thread()
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
///   `block_in_place` is not supported. Wraps the future in the thread-local
///   `LocalSet::run_until()` and spin-polls the combined future. This drives both
///   the caller's future and any tasks spawned via [`spawn_local`] concurrently,
///   enabling `task`/`await` to work correctly from within existing async contexts.
/// - **No runtime** (called from a plain sync thread): delegates to [`block_on`], which
///   uses the thread-local `TOKIO_RT` + `LOCAL_SET`. This ensures `spawn_local` calls
///   inside `fut` are driven by the same LocalSet.
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
                // current_thread runtime: block_in_place panics, and Handle::block_on
                // panics ("cannot call block_on from within an async context"). Use the
                // thread-local LocalSet's run_until to drive both the future and any
                // spawn_local tasks concurrently. We spin-poll the combined future so
                // spawned tasks make progress while the caller awaits their handles.
                LOCAL_SET.with(|ls| poll_future_sync(ls.run_until(fut)))
            }
        }
        Err(_) => {
            // No existing runtime: use the thread-local runtime and LocalSet so
            // that any spawn_local calls from within `fut` are driven by the same
            // LocalSet that spawn_local writes into. Using a fresh LocalSet here
            // would cause spawn_local tasks to accumulate in LOCAL_SET and never
            // be polled during this block_on call.
            block_on(fut)
        }
    }
}

/// Drive a future to completion using a minimal spin-poll executor.
///
/// This polls the future in a tight loop with a no-op waker until it returns
/// `Poll::Ready`. It does NOT enter any tokio runtime context, making it safe
/// to call from within a `current_thread` tokio runtime.
///
/// The caller wraps the target future in `LocalSet::run_until()`, so any
/// `spawn_local` tasks queued during evaluation are driven concurrently on
/// each spin iteration. This handles futures that spawn concurrent tasks via
/// `LOCAL_SET`; the spin loop gives those tasks opportunities to progress.
///
/// # INVARIANT: No genuine I/O suspension
///
/// This executor is correct ONLY because tinct's async eval functions —
/// `eval()`, `materialize()`, and `force_step()` — use `async fn` as syntax
/// sugar for cooperative task switching between `spawn_local` subtasks. They
/// contain NO genuine I/O suspension points (no `tokio::fs`, no `tokio::net`,
/// no `tokio::time::sleep`, no channel awaits on external data).
///
/// Any future that returns `Poll::Pending` without an I/O-backed waker
/// registration will cause this function to busy-spin at 100% CPU until the
/// future eventually makes progress on the next poll. For tinct's eval
/// futures, `Pending` is transient — the `LocalSet::run_until` wrapper
/// drives spawned subtasks forward on each iteration, and evaluation
/// terminates because ASTs are finite and evaluation is productive.
///
/// CONTRACT: futures passed to `poll_future_sync` MUST NOT perform real
/// network or file I/O in their hot path. If tinct gains genuine async I/O
/// (e.g., via `NetCap` builtins), those callers must use a real tokio
/// executor, not this function.
fn poll_future_sync<F: Future>(fut: F) -> F::Output {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    let mut cx = Context::from_waker(std::task::Waker::noop());

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

/// Spawn a `!Send + 'static` future as a local task on the thread-local LocalSet.
///
/// Use ONLY for futures that are genuinely `!Send` (e.g. the h3 `Connection` driver
/// that captures `h3::client::Connection<h3_quinn::H3Connection>` which is `!Send`).
/// For all Send futures, use `tokio::spawn` directly.
///
/// The task is polled cooperatively on the next (and every subsequent)
/// [`block_on`] call on this thread.
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
