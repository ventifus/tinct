//! Shared test helpers: `test_span()`, `sp()`, and `rsp()` for constructing test fixtures.

use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use crate::ast::{Position, Span, Spanned};

pub fn test_span(start_line: usize, start_col: usize, end_line: usize, end_col: usize) -> Span {
    Span::new(
        Position {
            offset: 0,
            line: start_line,
            column: start_col,
        },
        Position {
            offset: 0,
            line: end_line,
            column: end_col,
        },
    )
}

pub fn sp<T>(node: T) -> Spanned<T> {
    Spanned::new(node, test_span(1, 1, 1, 10))
}

/// Like `sp()` but wrapped in `Rc` — convenience for `Entry.value`, `NamedArg.value`,
/// `Call.args` elements, and `Fn.body` which are all `Rc<Spanned<Expr>>`.
pub fn rsp<T>(node: T) -> Rc<Spanned<T>> {
    Rc::new(sp(node))
}

/// Centralized directory capabilities for test infrastructure.
pub struct TestCaps {
    pub root: Arc<cap_std::fs::Dir>,
    /// Opened stdlib/ directory — available for tests that need it, not currently used.
    #[allow(dead_code)]
    pub stdlib: Arc<cap_std::fs::Dir>,
}

static TEST_CAPS: OnceLock<TestCaps> = OnceLock::new();

/// Returns shared test directory capabilities. Thread-safe via `OnceLock`.
///
/// Centralizes the single `open_ambient_dir` call for the entire test suite.
/// All tests should use `test_caps().root` or `test_caps().stdlib` instead of
/// calling `open_ambient_dir` directly.
pub fn test_caps() -> &'static TestCaps {
    TEST_CAPS.get_or_init(|| {
        // AMBIENT-OK: single initialization for entire test suite.
        #[allow(clippy::disallowed_methods)]
        let root = cap_std::fs::Dir::open_ambient_dir(
            env!("CARGO_MANIFEST_DIR"),
            cap_std::ambient_authority(),
        )
        .expect("cannot open project root for tests");

        let stdlib = root
            .open_dir("stdlib")
            .expect("cannot open stdlib/ for tests");

        TestCaps {
            root: Arc::new(root),
            stdlib: Arc::new(stdlib),
        }
    })
}
