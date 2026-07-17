//! Memory budget tracking for arena-based allocation.
//!
//! Replaces the unsafe GlobalAlloc-based limit_alloc module with safe atomic counters.
//! Arena-level tracking records thunk and scope allocations, checks against a configured
//! limit, and sets an OOM flag when the limit is exceeded. The evaluator checks the OOM
//! flag at safe points and returns a ResourceLimitExceeded error.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering::Relaxed};

static THUNK_BYTES: AtomicI64 = AtomicI64::new(0);
static THUNK_TOTAL: AtomicI64 = AtomicI64::new(0);
static SCOPE_BYTES: AtomicI64 = AtomicI64::new(0);
static LIMIT: AtomicI64 = AtomicI64::new(0);
static OOM_FIRED: AtomicBool = AtomicBool::new(false);

pub const EXIT_OOM: i32 = 3;

/// Set the memory limit in bytes. Zero means no limit.
pub fn set_limit(bytes: u64) {
    LIMIT.store(bytes as i64, Relaxed);
}

/// Record a thunk allocation and check if the limit has been exceeded.
/// Sets the OOM flag if the total allocated bytes exceed the configured limit.
pub fn record_and_check(bytes: usize) {
    THUNK_BYTES.fetch_add(bytes as i64, Relaxed);
    THUNK_TOTAL.fetch_add(1, Relaxed);
    let limit = LIMIT.load(Relaxed);
    if limit > 0 && (THUNK_BYTES.load(Relaxed) + SCOPE_BYTES.load(Relaxed)) > limit {
        OOM_FIRED.swap(true, Relaxed);
    }
}

/// Record thunk deallocation when a scope is dropped.
pub fn record_thunk_free(bytes: usize, _count: usize) {
    THUNK_BYTES.fetch_sub(bytes as i64, Relaxed);
}

/// Record scope allocation (the Scope struct and its Vec<Option<Arc<Thunk>>> backing).
pub fn record_scope_alloc(bytes: usize) {
    SCOPE_BYTES.fetch_add(bytes as i64, Relaxed);
}

/// Check if the OOM flag has been set.
pub fn is_oom_flagged() -> bool {
    OOM_FIRED.load(Relaxed)
}

/// Get the total allocated bytes (thunks + scopes).
pub fn allocated_bytes() -> i64 {
    THUNK_BYTES.load(Relaxed) + SCOPE_BYTES.load(Relaxed)
}

/// Get the total number of thunks allocated (monotonic counter).
pub fn thunk_total() -> i64 {
    THUNK_TOTAL.load(Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Global statics are process-wide; tests must be serialized to avoid interleaving
    // reset() calls from parallel test threads.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset() {
        THUNK_BYTES.store(0, Relaxed);
        THUNK_TOTAL.store(0, Relaxed);
        SCOPE_BYTES.store(0, Relaxed);
        LIMIT.store(0, Relaxed);
        OOM_FIRED.store(false, Relaxed);
    }

    #[test]
    fn test_record_and_check_no_limit() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        record_and_check(100);
        assert_eq!(allocated_bytes(), 100);
        assert_eq!(thunk_total(), 1);
        assert!(!is_oom_flagged());
    }

    #[test]
    fn test_record_and_check_under_limit() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        set_limit(1000);
        record_and_check(100);
        assert_eq!(allocated_bytes(), 100);
        assert!(!is_oom_flagged());
    }

    #[test]
    fn test_record_and_check_exceeds_limit() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        set_limit(100);
        record_and_check(200);
        assert_eq!(allocated_bytes(), 200);
        assert!(is_oom_flagged());
    }

    #[test]
    fn test_thunk_free() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        record_and_check(100);
        assert_eq!(allocated_bytes(), 100);
        record_thunk_free(100, 1);
        assert_eq!(allocated_bytes(), 0);
        // thunk_total is monotonic — never decrements
        assert_eq!(thunk_total(), 1);
    }

    #[test]
    fn test_scope_alloc() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        record_scope_alloc(50);
        assert_eq!(allocated_bytes(), 50);
        record_and_check(100);
        assert_eq!(allocated_bytes(), 150);
    }

    #[test]
    fn test_scope_alloc_contributes_to_limit() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        set_limit(100);
        record_scope_alloc(60);
        assert!(!is_oom_flagged());
        record_and_check(50);
        // Total is now 110 > 100 → OOM
        assert!(is_oom_flagged());
    }

    #[test]
    fn test_thunk_total_monotonic() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        record_and_check(10);
        record_and_check(10);
        record_and_check(10);
        assert_eq!(thunk_total(), 3);
        record_thunk_free(30, 3);
        assert_eq!(thunk_total(), 3); // Does not decrement
    }
}
