//! Shared test helpers: `test_span()`, `sp()`, and `rsp()` for constructing test fixtures.

use std::rc::Rc;

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
