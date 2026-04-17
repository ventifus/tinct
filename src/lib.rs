//! Parser, evaluator, type system, and builtins for the Lazy Lisp Transformer language.
//!
//! [`parse`] takes an input string and returns a fully-spanned `File` AST (one or more documents).
//! [`parse_expression`] is a convenience wrapper that parses a single expression.
//! [`eval_source`] parses and evaluates LLT source with the standard library environment.

pub mod ast;
pub(crate) mod error;
pub(crate) mod eval;
pub mod parser;
#[cfg(test)]
pub(crate) mod test_util;
// Type system modules: pub(crate) with dead_code allowed until public API is designed (Phase 3b).
#[allow(dead_code)]
pub(crate) mod typecheck;
#[allow(dead_code)]
pub(crate) mod types;
pub(crate) mod value;
// Phase 3a: Rust-native builtin functions.
pub(crate) mod builtins;

pub use ast::{Annotation, Document, Entry, Expr, File, NamedArg, Param, Position, Span, Spanned};
pub use parser::{parse, parse_expression, ParseError};

/// Parse and evaluate LLT source, returning the materialized result as a displayable string.
///
/// The output format recursively materializes all values (including dict entries)
/// into a readable representation. This is intended for testing; the full public
/// eval API will be designed in Phase 3b.
pub fn eval_source(input: &str) -> Result<String, String> {
    let file = parse(input).map_err(|e| format!("{e}"))?;
    let env = builtins::create_stdlib_env()?;
    let thunk = eval::eval_file(&file.node, env, 0).map_err(|e| format!("{e}"))?;
    let val = eval::materialize(&thunk, None, 0).map_err(|e| format!("{e}"))?;
    deep_materialize_to_string(&val, 0).map_err(|e| format!("{e}"))
}

/// Recursively materialize a Value into a displayable string.
///
/// Unlike `Value::Debug`, this fully materializes dict values so the output
/// shows the complete structure, not just keys.
///
/// `depth` tracks recursion depth to prevent stack overflow from deeply nested
/// dict-of-dicts structures. Uses the same limit as `eval::MAX_EVAL_DEPTH`.
fn deep_materialize_to_string(
    val: &value::Value,
    depth: usize,
) -> Result<String, Box<error::EvalError>> {
    if depth >= eval::MAX_EVAL_DEPTH {
        return Err(error::EvalError::new(
            format!("maximum display depth exceeded ({})", eval::MAX_EVAL_DEPTH),
            ast::Span::origin(),
        )
        .into());
    }
    match val {
        value::Value::Int(n) => Ok(format!("Int({n})")),
        value::Value::Float(f) => Ok(format!("Float({f})")),
        value::Value::String(s) => Ok(format!("String({s:?})")),
        value::Value::Bool(b) => Ok(format!("Bool({b})")),
        value::Value::Dict(map) => {
            let mut parts = Vec::new();
            for (key, thunk) in map {
                let v = eval::materialize(thunk, None, depth)?;
                let key_str = match key {
                    value::Key::Int(n) => format!("{n}"),
                    value::Key::String(s) => format!("{s:?}"),
                };
                let val_str = deep_materialize_to_string(&v, depth + 1)?;
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
