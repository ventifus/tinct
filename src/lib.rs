//! Parser and evaluator for the Lazy Lisp Transformer language.
//!
//! [`parse`] takes an input string and returns a fully-spanned `File` AST (one or more documents).
//! [`parse_expression`] is a convenience wrapper that parses a single expression.

pub mod ast;
// Phase 1a-1c evaluator modules: defined but not yet wired into public API (Phase 1d).
#[allow(dead_code)]
pub(crate) mod error;
#[allow(dead_code)]
pub(crate) mod eval;
pub mod parser;
#[cfg(test)]
pub(crate) mod test_util;
#[allow(dead_code)]
pub(crate) mod value;

pub use ast::{Annotation, Document, Entry, Expr, File, NamedArg, Param, Position, Span, Spanned};
pub use parser::{parse, parse_expression, ParseError};
