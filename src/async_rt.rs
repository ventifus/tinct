//! Shared async runtime for QUIC/HTTP3 builtins.
//! Single-threaded current_thread runtime initialized once per thread.
//! reqwest's blocking client has its own internal async runtime — no conflict.

use std::future::Future;

std::thread_local! {
    static TOKIO_RT: tokio::runtime::Runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("async_rt: failed to create tokio runtime");
}

pub fn block_on<F: Future>(fut: F) -> F::Output {
    TOKIO_RT.with(|rt| rt.block_on(fut))
}
