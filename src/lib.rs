//! Parser and evaluator for the Lazy Lisp Transformer language.
//!
//! [`parse`] takes an input string and returns a fully-spanned `File` AST (one or more documents).
//! [`parse_expression`] is a convenience wrapper that parses a single expression.

pub mod ast;
// Phase 1a-1d evaluator modules: pub(crate) until public API is designed (Phase 3b).
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

use std::cell::RefCell;
use std::rc::Rc;

/// Parse and evaluate LLT source, returning the materialized result as a displayable string.
///
/// The output format recursively materializes all values (including dict entries)
/// into a readable representation. This is intended for testing; the full public
/// eval API will be designed in Phase 3b.
pub fn eval_source(input: &str) -> Result<String, String> {
    let file = parse(input).map_err(|e| format!("{e}"))?;
    let env = Rc::new(RefCell::new(value::Environment::new()));
    let thunk = eval::eval_file(&file.node, env, 0).map_err(|e| format!("{e}"))?;
    let val = eval::materialize(&thunk, None, 0).map_err(|e| format!("{e}"))?;
    deep_materialize_to_string(&val).map_err(|e| format!("{e}"))
}

/// Recursively materialize a Value into a displayable string.
///
/// Unlike `Value::Debug`, this fully materializes dict values so the output
/// shows the complete structure, not just keys.
fn deep_materialize_to_string(val: &value::Value) -> Result<String, Box<error::EvalError>> {
    match val {
        value::Value::Int(n) => Ok(format!("Int({n})")),
        value::Value::Float(f) => Ok(format!("Float({f})")),
        value::Value::String(s) => Ok(format!("String({s:?})")),
        value::Value::Bool(b) => Ok(format!("Bool({b})")),
        value::Value::Dict(map) => {
            let mut parts = Vec::new();
            for (key, thunk) in map {
                let v = eval::materialize(thunk, None, 0)?;
                let key_str = match key {
                    value::Key::Int(n) => format!("{n}"),
                    value::Key::String(s) => format!("{s:?}"),
                };
                let val_str = deep_materialize_to_string(&v)?;
                parts.push(format!("{key_str}: {val_str}"));
            }
            Ok(format!("Dict({{{}}})", parts.join(", ")))
        }
        value::Value::Function { params, .. } => {
            let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
            Ok(format!("Function({})", names.join(", ")))
        }
        value::Value::Builtin { name, .. } => Ok(format!("Builtin({name})")),
    }
}
