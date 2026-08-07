//! Shared test helpers: `test_span()` and `sp()` for constructing test fixtures.

use crate::ast::{Span, Spanned};
use crate::rust_span;

pub fn test_span(start_line: u32, start_col: u32, end_line: u32, end_col: u32) -> Span {
    Span::new(start_line, start_col, end_line, end_col, rust_span!().file)
}

pub fn sp<T>(node: T) -> Spanned<T> {
    Spanned::new(node, test_span(1, 1, 1, 10))
}
