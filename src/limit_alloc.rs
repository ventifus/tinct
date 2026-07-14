//! Soft heap-limit allocator wrapper.
//!
//! When `--max-memory` is passed, `set_limit` activates a byte counter on the
//! global allocator.  When the heap crosses that threshold the process prints
//! a diagnostics summary (thunks created, environments created, heap bytes)
//! and exits cleanly via `process::exit`.  RLIMIT_AS (set to the same value)
//! acts as the hard backstop if something bypasses the allocator (e.g. a
//! direct `mmap` call from a native dependency).
//!
//! # Re-entrancy
//! `eprintln!` inside `oom_exit` may itself allocate.  A `OOM_FIRED` flag
//! ensures only the first over-limit allocation triggers diagnostics; any
//! subsequent allocation during the diagnostic print returns `null_mut()`,
//! causing `handle_alloc_error` to abort — acceptable since we already
//! started the human-readable message.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering::Relaxed};

/// Current net heap bytes (allocated − freed).  Relaxed: we want a cheap
/// approximation, not a sequentially-consistent snapshot.
static ALLOCATED: AtomicI64 = AtomicI64::new(0);

/// Highest value ALLOCATED has ever reached.
static PEAK: AtomicI64 = AtomicI64::new(0);

/// Active limit in bytes.  0 = disabled (default until `set_limit` is called).
static LIMIT: AtomicI64 = AtomicI64::new(0);

/// Set once the first time the limit is exceeded, to prevent recursive OOM
/// diagnostics if `eprintln!` itself triggers an allocation.
static OOM_FIRED: AtomicBool = AtomicBool::new(false);

/// Exit code used when the soft heap limit fires.
pub const EXIT_OOM: i32 = 3;

/// Activate the soft heap limit.  Call once from `main` after CLI parsing.
/// `bytes == 0` keeps the limit disabled.
pub fn set_limit(bytes: u64) {
    LIMIT.store(bytes as i64, Relaxed);
}

/// Snapshot of current net heap bytes (approximate).
pub fn allocated_bytes() -> i64 {
    ALLOCATED.load(Relaxed)
}

/// Snapshot of peak heap bytes since process start.
pub fn peak_bytes() -> i64 {
    PEAK.load(Relaxed)
}

pub struct LimitedAlloc;

#[global_allocator]
pub(crate) static GLOBAL: LimitedAlloc = LimitedAlloc;

unsafe impl GlobalAlloc for LimitedAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size() as i64;
        let now = ALLOCATED.fetch_add(size, Relaxed) + size;
        update_peak(now);
        if over_limit(now) {
            ALLOCATED.fetch_sub(size, Relaxed);
            return oom_or_null();
        }
        let ptr = System.alloc(layout);
        if ptr.is_null() {
            // RLIMIT_AS or kernel OOM fired before our counter reached the limit
            // (virtual address space includes stack, libs, mmaps — all uncounted).
            ALLOCATED.fetch_sub(size, Relaxed);
            return oom_or_null();
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        ALLOCATED.fetch_sub(layout.size() as i64, Relaxed);
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let delta = new_size as i64 - layout.size() as i64;
        let now = ALLOCATED.fetch_add(delta, Relaxed) + delta;
        if delta > 0 {
            update_peak(now);
            if over_limit(now) {
                ALLOCATED.fetch_sub(delta, Relaxed);
                return oom_or_null();
            }
        }
        let ptr = System.realloc(ptr, layout, new_size);
        if ptr.is_null() && delta > 0 {
            ALLOCATED.fetch_sub(delta, Relaxed);
            return oom_or_null();
        }
        ptr
    }
}

#[inline]
fn over_limit(now: i64) -> bool {
    let limit = LIMIT.load(Relaxed);
    limit > 0 && now > limit
}

fn update_peak(now: i64) {
    let mut peak = PEAK.load(Relaxed);
    while now > peak {
        match PEAK.compare_exchange_weak(peak, now, Relaxed, Relaxed) {
            Ok(_) => break,
            Err(p) => peak = p,
        }
    }
}

/// First call: print diagnostics and exit.
/// Re-entrant call (eprintln! itself hit the limit): return null so
/// `handle_alloc_error` aborts — we already started the human-readable output.
#[cold]
unsafe fn oom_or_null() -> *mut u8 {
    if OOM_FIRED.swap(true, Relaxed) {
        return std::ptr::null_mut();
    }
    oom_exit()
}

#[cold]
fn oom_exit() -> ! {
    let limit = LIMIT.load(Relaxed);
    let allocated = ALLOCATED.load(Relaxed);
    let peak = PEAK.load(Relaxed);

    eprintln!("tinct: out of memory (limit: {limit} bytes)");
    eprintln!("  heap allocated:       {allocated} bytes");
    eprintln!("  heap peak:            {peak} bytes");

    // Use _exit (not process::exit) to avoid running atexit/cleanup handlers.
    // process::exit triggers tokio's scheduler cleanup which panics if called
    // while a scheduler lock is held (which is the case inside async tasks).
    // _exit terminates immediately with the given exit code — correct for OOM
    // since any cleanup code would itself try to allocate and fail.
    #[cfg(unix)]
    unsafe {
        libc::_exit(EXIT_OOM)
    }
    #[cfg(not(unix))]
    std::process::exit(EXIT_OOM)
}
