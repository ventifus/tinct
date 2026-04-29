//! Parser, evaluator, type system, and builtins for the tinct language.
//!
//! [`parse`] takes an input string and returns a fully-spanned `File` AST (one or more documents).
//! [`parse_expression`] is a convenience wrapper that parses a single expression.
//! [`eval_source`] parses and evaluates LLT source with the standard library environment.
//!
//! Additional public API:
//! - [`eval_file`] / [`eval_file_with_input`] -- evaluate a parsed AST with optional stdin input
//! - [`typecheck_source`] -- parse and typecheck only (no evaluation)
//! - [`materialize`] / [`deep_materialize`] -- force thunks (shallow or recursive)
//! - [`create_stdlib_env`] -- create the standard library environment (Rust builtins + LLT prelude)
//! - [`EvalContext`] -- evaluation context with base directory and stdlib environment for `$include`
//! - [`json_to_value`] -- convert `serde_json::Value` to LLT `Value`
//! - [`value_to_json`] -- convert LLT `Value` to `serde_json::Value`
//! - [`value_to_display_string`] -- render a materialized `Value` as a human-readable string
//! - [`MAX_EVAL_DEPTH`] -- recursion limit for evaluation (256)
//! - [`MAX_FILE_SIZE`] -- file size limit for `$include` and stdin (10 MB)

pub mod ast;
pub(crate) mod error;
pub(crate) mod eval;
pub mod formatter;
pub mod lexer;
pub mod parser;
#[cfg(test)]
pub(crate) mod test_util;
pub mod typecheck;
pub(crate) mod types;
pub(crate) mod value;
// Rust-native builtin functions (stdlib-1 sprint).
pub(crate) mod builtins;
// $_ desugaring (pre-typecheck AST transformation).
pub mod desugar;
// REPL (Read-Eval-Print Loop).
pub mod repl;
// LSP (Language Server Protocol).
#[cfg(feature = "lsp")]
pub mod lsp;

use std::rc::Rc;

/// AST node types produced by the parser.
pub use ast::{Annotation, Document, Entry, Expr, File, NamedArg, Param, Position, Span, Spanned};
/// Parser entry points and error type.
pub use parser::{parse, parse_expression, ParseError};

/// Evaluation functions and depth limit.
pub use eval::{
    deep_materialize, eval_file, eval_file_with_input, materialize, EvalContext, MAX_EVAL_DEPTH,
};

/// Builtin infrastructure: stdlib creation, JSON conversion.
pub use builtins::{create_stdlib_env, json_to_value, MAX_FILE_SIZE};

/// Error types with source spans and stack traces.
pub use error::{EvalError, StackFrame};

/// Formatter: canonical source reformatter.
pub use formatter::format_source;

#[cfg(feature = "repl")]
pub use repl::run_repl;

#[cfg(feature = "lsp")]
pub use lsp::run_lsp;

/// Runtime value types: values, thunks, environments, and dict keys.
pub use value::{Environment, Key, Thunk, Value};

/// Parse and evaluate LLT source, returning the result in **LLT display format**
/// (e.g. `Int(42)`, `Dict({"x": Int(1)})`) -- not JSON.
///
/// Runs advisory type checking before evaluation (type errors are ignored).
/// The output format recursively materializes all values (including dict entries)
/// into a readable representation. Primarily used for testing and corpus validation.
/// For JSON output, use [`value_to_json`] after evaluation instead.
pub fn eval_source(input: &str) -> Result<String, String> {
    eval_source_with_config(input, false)
}

/// Parse and evaluate LLT source with configurable filesystem access.
///
/// This is a variant of [`eval_source`] that allows control over the `no_fs` flag.
/// When `no_fs` is `true`, filesystem operations (like `$include`) are disabled.
/// Primarily used for corpus tests that verify the `IncludeForbidden` error path.
pub fn eval_source_with_config(input: &str, no_fs: bool) -> Result<String, String> {
    let mut file = parse(input).map_err(|e| format!("{e}"))?;
    // PIPELINE INVARIANT: Desugar must run after parse and before typecheck.
    // See also: src/main.rs:234-240 (run_eval pipeline)
    // Desugar $_ implicit lambdas (pre-typecheck AST transformation).
    desugar::desugar_file(&mut file.node);
    // Type errors are advisory; evaluation proceeds regardless.
    let _ = typecheck::typecheck_file(&file.node);
    let env = builtins::create_stdlib_env().map_err(|e| format!("{e}"))?;
    // Create evaluation context (current directory, configurable sandbox)
    // Note: PathBuf::from(".") is a relative path. $include resolves relative to the test
    // runner's working directory, not to an absolute path. The CLI (main.rs) canonicalizes
    // to an absolute path before calling eval_file.
    let ctx = eval::EvalContext::new(std::path::PathBuf::from("."), Rc::clone(&env), no_fs);
    let thunk =
        eval::eval_file(&file.node, Rc::clone(&env), &ctx, 0).map_err(|e| format!("{e}"))?;
    let val = eval::materialize(&thunk, None, &ctx, 0).map_err(|e| format!("{e}"))?;
    let forced = eval::deep_materialize(&val, &ctx, 0).map_err(|e| format!("{e}"))?;
    value_to_display_string(&forced, &ctx, 0).map_err(|e| format!("{e}"))
}

/// Parse and type-check LLT source code.
///
/// Returns `Ok(())` if type checking succeeds, or `Err(errors)` with a formatted
/// error message if type errors are found. Each error includes the error message
/// and the source span where it occurred.
///
/// **Note**: This function type-checks with an empty type environment. Stdlib
/// builtins (like `$+`, `$merge`, etc.) are not in scope and will produce
/// "undefined variable" errors. This is a known limitation — the type system
/// does not yet have type signatures for builtins.
///
/// This function is primarily used for testing and corpus validation to ensure
/// type checking regressions are caught. The main `eval_source` function treats
/// type errors as advisory warnings and continues evaluation regardless.
pub fn typecheck_source(input: &str) -> Result<(), String> {
    let mut file = parse(input).map_err(|e| format!("{e}"))?;
    // Desugar $_ implicit lambdas (pre-typecheck AST transformation).
    desugar::desugar_file(&mut file.node);
    // Type check the file
    typecheck::typecheck_file(&file.node).map_err(|type_errors| {
        let error_msgs: Vec<String> = type_errors.iter().map(|e| format!("{}", e)).collect();
        error_msgs.join("\n")
    })
}

// value_to_json and value_to_display_string are kept separate; their logic diverges too much for a shared visitor.

/// Convert a materialized [`Value`](value::Value) to a [`serde_json::Value`].
///
/// Dict values are materialized on demand via [`eval::materialize`]. If all keys
/// are sequential integers starting from 0 the dict is serialized as a JSON array;
/// otherwise it becomes a JSON object (integer keys are stringified).
///
/// Returns an error for:
/// - `Function` / `Builtin` values (no JSON representation)
/// - `Float` values that are NaN or infinite (not representable in JSON)
/// - Exceeding the maximum recursion depth ([`eval::MAX_EVAL_DEPTH`])
pub fn value_to_json(
    val: &value::Value,
    ctx: &std::rc::Rc<eval::EvalContext>,
    depth: usize,
) -> Result<serde_json::Value, Box<error::EvalError>> {
    use serde_json::Value as JV;
    use value::{Key, Value};

    if depth > eval::MAX_EVAL_DEPTH {
        return Err(
            error::EvalError::depth_exceeded(eval::MAX_EVAL_DEPTH, ast::Span::origin()).into(),
        );
    }

    match val {
        Value::Int(n) => Ok(JV::Number((*n).into())),
        Value::Float(f) => {
            let n = serde_json::Number::from_f64(*f).ok_or_else(|| {
                error::EvalError::float_not_finite("to-json".to_string(), *f, ast::Span::origin())
            })?;
            Ok(JV::Number(n))
        }
        Value::String(s) => Ok(JV::String(s.clone())),
        Value::Bool(b) => Ok(JV::Bool(*b)),
        Value::Dict(map) => {
            // Detect array-like dict: all keys are sequential ints 0..n
            let is_array = !map.is_empty()
                && map
                    .keys()
                    .enumerate()
                    .all(|(i, k)| matches!(k, Key::Int(n) if *n >= 0 && *n as usize == i));

            if is_array {
                let mut arr = Vec::with_capacity(map.len());
                for (_key, thunk) in map {
                    let v = eval::materialize(thunk, None, ctx, depth)?;
                    arr.push(value_to_json(&v, ctx, depth + 1)?);
                }
                Ok(JV::Array(arr))
            } else {
                let mut obj = serde_json::Map::with_capacity(map.len());
                for (key, thunk) in map {
                    let key_str = match key {
                        Key::Int(n) => n.to_string(),
                        Key::String(s) => s.clone(),
                    };
                    let v = eval::materialize(thunk, None, ctx, depth)?;
                    obj.insert(key_str, value_to_json(&v, ctx, depth + 1)?);
                }
                Ok(JV::Object(obj))
            }
        }
        Value::Function { .. } => Err(error::EvalError::value_not_serializable(
            "Function".to_string(),
            ast::Span::origin(),
        )
        .into()),
        Value::Builtin { name, .. } => Err(error::EvalError::value_not_serializable(
            format!("Builtin ({name})"),
            ast::Span::origin(),
        )
        .into()),
        Value::Seq { head, .. } => {
            Err(error::EvalError::value_not_serializable("Seq".to_string(), head.span).into())
        }
        Value::Proxy { .. } => Err(error::EvalError::value_not_serializable(
            "Proxy".to_string(),
            ast::Span::origin(),
        )
        .into()),
    }
}

/// Convert a Value into a displayable string.
///
/// Unlike `Value::Debug`, this renders dict values showing the complete
/// structure, not just keys. The value should already be deep-materialized
/// via [`eval::deep_materialize`]; this function still calls `materialize`
/// on each thunk for safety but does not perform recursive deep-forcing.
///
/// Unlike [`value_to_json`], this function accepts `Float` values that are
/// NaN or Infinity (they render as `Float(NaN)`, `Float(inf)`, etc.).
///
/// `depth` tracks recursion depth to prevent stack overflow from deeply nested
/// dict-of-dicts structures. Uses the same limit as `eval::MAX_EVAL_DEPTH`.
pub fn value_to_display_string(
    val: &value::Value,
    ctx: &std::rc::Rc<eval::EvalContext>,
    depth: usize,
) -> Result<String, Box<error::EvalError>> {
    if depth > eval::MAX_EVAL_DEPTH {
        return Err(error::EvalError::resource_limit_exceeded(
            format!(
                "display depth exceeded: maximum depth is {}",
                eval::MAX_EVAL_DEPTH
            ),
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
                let v = eval::materialize(thunk, None, ctx, depth)?;
                let key_str = match key {
                    value::Key::Int(n) => format!("{n}"),
                    value::Key::String(s) => format!("{s:?}"),
                };
                let val_str = value_to_display_string(&v, ctx, depth + 1)?;
                parts.push(format!("{key_str}: {val_str}"));
            }
            Ok(format!("Dict({{{}}})", parts.join(", ")))
        }
        value::Value::Function { params, .. } => {
            let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
            Ok(format!("Function({})", names.join(", ")))
        }
        value::Value::Builtin { name, .. } => Ok(format!("Builtin({name})")),
        value::Value::Seq { head, .. } => {
            // Materialize and display head element
            let head_val = eval::materialize(head, None, ctx, depth)?;
            let head_str = value_to_display_string(&head_val, ctx, depth + 1)?;
            Ok(format!("Seq({}, ...)", head_str))
        }
        value::Value::Proxy { .. } => Ok("Proxy".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use std::cell::RefCell;
    use std::rc::Rc;
    use test_util::test_span;
    use value::{Environment, Key, Thunk, Value};

    /// Helper: wrap a Value in a materialized thunk.
    fn thunk(val: Value) -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(val, test_span(1, 1, 1, 1)))
    }

    fn test_ctx() -> Rc<eval::EvalContext> {
        eval::EvalContext::new(
            std::path::PathBuf::from("."),
            builtins::create_root_env(),
            false,
        )
    }

    #[test]
    fn test_json_int() {
        let result = value_to_json(&Value::Int(42), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(42));
    }

    #[test]
    fn test_json_int_negative() {
        let result = value_to_json(&Value::Int(-100), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(-100));
    }

    #[test]
    fn test_json_int_zero() {
        let result = value_to_json(&Value::Int(0), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(0));
    }

    #[test]
    fn test_json_float() {
        let result = value_to_json(&Value::Float(3.14), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(3.14));
    }

    #[test]
    fn test_json_float_negative() {
        let result = value_to_json(&Value::Float(-2.5), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(-2.5));
    }

    #[test]
    fn test_json_float_zero() {
        let result = value_to_json(&Value::Float(0.0), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(0.0));
    }

    #[test]
    fn test_json_float_nan_error() {
        let err = value_to_json(&Value::Float(f64::NAN), &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("NaN"));
    }

    #[test]
    fn test_json_float_infinity_error() {
        let err = value_to_json(&Value::Float(f64::INFINITY), &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("is not a finite number"));
    }

    #[test]
    fn test_json_float_neg_infinity_error() {
        let err = value_to_json(&Value::Float(f64::NEG_INFINITY), &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("is not a finite number"));
    }

    #[test]
    fn test_json_string() {
        let result = value_to_json(&Value::String("hello".into()), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!("hello"));
    }

    #[test]
    fn test_json_string_empty() {
        let result = value_to_json(&Value::String("".into()), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(""));
    }

    #[test]
    fn test_json_string_with_special_chars() {
        let result = value_to_json(&Value::String("line\nnewline".into()), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!("line\nnewline"));
    }

    #[test]
    fn test_json_bool_true() {
        let result = value_to_json(&Value::Bool(true), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn test_json_bool_false() {
        let result = value_to_json(&Value::Bool(false), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(false));
    }

    #[test]
    fn test_json_dict_empty() {
        let dict = Value::Dict(IndexMap::new());
        let result = value_to_json(&dict, &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!({}));
    }

    #[test]
    fn test_json_dict_string_keys() {
        let mut map = IndexMap::new();
        map.insert(
            Key::String("name".into()),
            thunk(Value::String("Alice".into())),
        );
        map.insert(Key::String("age".into()), thunk(Value::Int(30)));
        let result = value_to_json(&Value::Dict(map), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!({"name": "Alice", "age": 30}));
    }

    #[test]
    fn test_json_dict_int_keys_non_sequential() {
        // Int keys that are NOT sequential from 0 -> object with stringified keys
        let mut map = IndexMap::new();
        map.insert(Key::Int(5), thunk(Value::String("five".into())));
        map.insert(Key::Int(10), thunk(Value::String("ten".into())));
        let result = value_to_json(&Value::Dict(map), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!({"5": "five", "10": "ten"}));
    }

    #[test]
    fn test_json_dict_mixed_keys() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("zero".into())));
        map.insert(Key::String("x".into()), thunk(Value::Int(1)));
        let result = value_to_json(&Value::Dict(map), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!({"0": "zero", "x": 1}));
    }

    #[test]
    fn test_json_dict_array_like() {
        // Sequential int keys 0, 1, 2 -> JSON array
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("a".into())));
        map.insert(Key::Int(1), thunk(Value::String("b".into())));
        map.insert(Key::Int(2), thunk(Value::String("c".into())));
        let result = value_to_json(&Value::Dict(map), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(["a", "b", "c"]));
    }

    #[test]
    fn test_json_dict_array_single_element() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::Bool(true)));
        let result = value_to_json(&Value::Dict(map), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!([true]));
    }

    #[test]
    fn test_json_dict_array_wrong_order() {
        // Keys are 0 and 1, but inserted in wrong order in IndexMap -> not sequential
        let mut map = IndexMap::new();
        map.insert(Key::Int(1), thunk(Value::String("b".into())));
        map.insert(Key::Int(0), thunk(Value::String("a".into())));
        // First key is 1 at index 0 -> not array-like
        let result = value_to_json(&Value::Dict(map), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!({"1": "b", "0": "a"}));
    }

    #[test]
    fn test_json_dict_array_starting_at_one() {
        // Keys 1, 2, 3 -- not starting from 0, so object
        let mut map = IndexMap::new();
        map.insert(Key::Int(1), thunk(Value::Int(10)));
        map.insert(Key::Int(2), thunk(Value::Int(20)));
        let result = value_to_json(&Value::Dict(map), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!({"1": 10, "2": 20}));
    }

    #[test]
    fn test_json_nested_dict() {
        let mut inner = IndexMap::new();
        inner.insert(Key::String("x".into()), thunk(Value::Int(1)));
        let mut outer = IndexMap::new();
        outer.insert(Key::String("inner".into()), thunk(Value::Dict(inner)));
        outer.insert(Key::String("y".into()), thunk(Value::Int(2)));
        let result = value_to_json(&Value::Dict(outer), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!({"inner": {"x": 1}, "y": 2}));
    }

    #[test]
    fn test_json_array_of_objects() {
        let mut obj1 = IndexMap::new();
        obj1.insert(
            Key::String("name".into()),
            thunk(Value::String("Alice".into())),
        );
        let mut obj2 = IndexMap::new();
        obj2.insert(
            Key::String("name".into()),
            thunk(Value::String("Bob".into())),
        );

        let mut arr = IndexMap::new();
        arr.insert(Key::Int(0), thunk(Value::Dict(obj1)));
        arr.insert(Key::Int(1), thunk(Value::Dict(obj2)));
        let result = value_to_json(&Value::Dict(arr), &test_ctx(), 0).unwrap();
        assert_eq!(
            result,
            serde_json::json!([{"name": "Alice"}, {"name": "Bob"}])
        );
    }

    #[test]
    fn test_json_function_error() {
        let f = Value::Function {
            params: Rc::new(vec![]),
            body: Rc::new(ast::Spanned::new(Expr::Int(0), test_span(1, 1, 1, 1))),
            env: Rc::new(RefCell::new(Environment::new())),
        };
        let err = value_to_json(&f, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("cannot serialize Function to JSON"),
            "got: {}",
            err.message()
        );
        assert_eq!(err.kind.code(), "E035");
    }

    #[test]
    fn test_json_seq_error() {
        let seq = Value::Seq {
            head: Rc::new(Thunk::new_materialized(
                Value::Int(1),
                test_span(1, 1, 1, 1),
            )),
            tail: Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                test_span(1, 1, 1, 1),
            )),
        };
        let err = value_to_json(&seq, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("cannot serialize Seq to JSON"),
            "got: {}",
            err.message()
        );
        assert_eq!(err.kind.code(), "E035");
    }

    #[test]
    fn test_json_builtin_error() {
        fn dummy(_ctx: value::BuiltinArgs) -> Result<Rc<Thunk>, Box<error::EvalError>> {
            Ok(Rc::new(Thunk::new_materialized(
                Value::Int(0),
                ast::Span::origin(),
            )))
        }
        let b = Value::Builtin {
            name: "test",
            func: dummy,
        };
        let err = value_to_json(&b, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("cannot serialize Builtin (test) to JSON"),
            "got: {}",
            err.message()
        );
        assert_eq!(err.kind.code(), "E035");
    }

    #[test]
    fn test_json_proxy_error() {
        let handler = Rc::new(Thunk::new_materialized(Value::Int(0), ast::Span::origin()));
        let proxy = Value::Proxy { handler };
        let err = value_to_json(&proxy, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("cannot serialize Proxy to JSON"),
            "got: {}",
            err.message()
        );
        assert_eq!(err.kind.code(), "E035");
    }

    #[test]
    fn test_json_depth_limit() {
        let err = value_to_json(&Value::Int(1), &test_ctx(), eval::MAX_EVAL_DEPTH + 1).unwrap_err();
        assert!(err.message().contains("maximum evaluation depth exceeded"));
    }

    #[test]
    fn test_json_depth_limit_just_under() {
        // One below the limit should still succeed for a leaf value
        let result = value_to_json(&Value::Int(1), &test_ctx(), eval::MAX_EVAL_DEPTH);
        assert!(result.is_ok());
    }

    #[test]
    fn test_json_int_max() {
        let result = value_to_json(&Value::Int(i64::MAX), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(i64::MAX));
    }

    #[test]
    fn test_json_int_min() {
        let result = value_to_json(&Value::Int(i64::MIN), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(i64::MIN));
    }

    /// Helper: run the full eval pipeline (parse, eval, materialize, to JSON).
    fn eval_to_json(source: &str) -> serde_json::Value {
        eval_to_json_with_input(source, None)
    }

    /// Helper: run the full eval pipeline with optional stdin JSON injection.
    fn eval_to_json_with_input(
        source: &str,
        stdin_json: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let mut file = parse(source).expect("parse failed");
        desugar::desugar_file(&mut file.node);
        let env = builtins::create_stdlib_env().expect("stdlib failed");
        let ctx = test_ctx();

        let initial_input = stdin_json.map(|json| {
            builtins::json_to_value(&json, 0, ast::Span::origin()).expect("json_to_value failed")
        });

        let thunk = eval::eval_file_with_input(&file.node, env, &ctx, initial_input, 0)
            .expect("eval failed");
        let val = eval::materialize(&thunk, None, &ctx, 0).expect("materialize failed");
        value_to_json(&val, &ctx, 0).expect("value_to_json failed")
    }

    #[test]
    fn test_pipeline_simple_dict() {
        let result = eval_to_json("[x: 1 y: hello]");
        assert_eq!(result, serde_json::json!({"x": 1, "y": "hello"}));
    }

    #[test]
    fn test_pipeline_array_like() {
        let result = eval_to_json("[10 20 30]");
        assert_eq!(result, serde_json::json!([10, 20, 30]));
    }

    #[test]
    fn test_pipeline_nested() {
        let result = eval_to_json("[a: [b: [c: 42]]]");
        assert_eq!(result, serde_json::json!({"a": {"b": {"c": 42}}}));
    }

    #[test]
    fn test_pipeline_stdin_json_injection() {
        let input_json = serde_json::json!({"name": "Alice", "age": 30});
        let result = eval_to_json_with_input("[greeting: $$.name]", Some(input_json));
        assert_eq!(result, serde_json::json!({"greeting": "Alice"}));
    }

    #[test]
    fn test_pipeline_stdin_json_array() {
        let input_json = serde_json::json!([1, 2, 3]);
        let result = eval_to_json_with_input("[first: $$[0]]", Some(input_json));
        assert_eq!(result, serde_json::json!({"first": 1}));
    }

    #[test]
    fn test_pipeline_stdin_json_passthrough() {
        // When $$ is the whole output, it should pass through
        let input_json = serde_json::json!({"x": 42});
        let result = eval_to_json_with_input("$$", Some(input_json));
        assert_eq!(result, serde_json::json!({"x": 42}));
    }

    #[test]
    fn test_pipeline_no_stdin_default_empty_dict() {
        // Without stdin input, $$ defaults to empty dict
        let result = eval_to_json("$$");
        assert_eq!(result, serde_json::json!({}));
    }

    #[test]
    fn test_pipeline_multi_document_with_stdin() {
        // stdin -> doc1 -> $$ -> doc2
        let input_json = serde_json::json!({"val": 10});
        let source = "[result: $$.val]\n---\n[wrapped: $$.result]";
        let result = eval_to_json_with_input(source, Some(input_json));
        assert_eq!(result, serde_json::json!({"wrapped": 10}));
    }

    #[test]
    fn test_pipeline_deep_materialize() {
        let source = "[a: [b: [c: 42]]]";
        let mut file = parse(source).expect("parse failed");
        desugar::desugar_file(&mut file.node);
        let env = builtins::create_stdlib_env().expect("stdlib failed");
        let ctx = test_ctx();
        let thunk = eval::eval_file(&file.node, env, &ctx, 0).expect("eval failed");
        let val = eval::materialize(&thunk, None, &ctx, 0).expect("materialize failed");
        let forced = eval::deep_materialize(&val, &ctx, 0).expect("deep_materialize failed");
        let json = value_to_json(&forced, &ctx, 0).expect("value_to_json failed");
        assert_eq!(json, serde_json::json!({"a": {"b": {"c": 42}}}));
    }

    #[test]
    fn test_pipeline_display_format() {
        let source = "[x: 42]";
        let mut file = parse(source).expect("parse failed");
        desugar::desugar_file(&mut file.node);
        let env = builtins::create_stdlib_env().expect("stdlib failed");
        let ctx = test_ctx();
        let thunk = eval::eval_file(&file.node, env, &ctx, 0).expect("eval failed");
        let val = eval::materialize(&thunk, None, &ctx, 0).expect("materialize failed");
        let forced = eval::deep_materialize(&val, &ctx, 0).expect("deep_materialize failed");
        let display = value_to_display_string(&forced, &ctx, 0).expect("display failed");
        assert_eq!(display, "Dict({\"x\": Int(42)})");
    }

    #[test]
    fn test_display_seq() {
        let seq = Value::Seq {
            head: Rc::new(Thunk::new_materialized(
                Value::Int(1),
                test_span(1, 1, 1, 1),
            )),
            tail: Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                test_span(1, 1, 1, 1),
            )),
        };
        let display = value_to_display_string(&seq, &test_ctx(), 0).expect("display failed");
        assert_eq!(display, "Seq(Int(1), ...)");
    }

    #[test]
    fn test_display_proxy() {
        let proxy = Value::Proxy {
            handler: Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 1),
            )),
        };
        let display = value_to_display_string(&proxy, &test_ctx(), 0).expect("display failed");
        assert_eq!(display, "Proxy");
    }

    #[test]
    fn test_pipeline_scalar_output() {
        let result = eval_to_json("42");
        assert_eq!(result, serde_json::json!(42));
    }

    #[test]
    fn test_pipeline_string_output() {
        let result = eval_to_json("\"hello world\"");
        assert_eq!(result, serde_json::json!("hello world"));
    }

    #[test]
    fn test_pipeline_bool_output() {
        let result = eval_to_json("true");
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn test_pipeline_float_output() {
        let result = eval_to_json("3.14");
        assert_eq!(result, serde_json::json!(3.14));
    }
}
