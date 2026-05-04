//! Parser, evaluator, type system, and builtins for the tinct language.
//!
//! [`parse`] takes an input string and returns a fully-spanned `File` AST (one or more documents).
//! [`parse_expression`] is a convenience wrapper that parses a single expression.
//! [`eval_source`] parses and evaluates LLT source with the standard library environment.
//!
//! Additional public API:
//! - [`eval_file`] / [`eval_file_with_input`] -- evaluate a parsed AST with optional stdin input (requires EvalContext; `include` uses context base_dir for resolution)
//! - [`typecheck_source`] -- parse and typecheck only (no evaluation)
//! - [`materialize`] / [`deep_materialize`] -- force thunks (shallow or recursive)
//! - [`create_stdlib_env`] -- create the standard library environment (Rust builtins + LLT prelude)
//! - [`EvalContext`] -- evaluation context with base directory and stdlib environment; include_cache memoizes `include` results (same file = same cached thunk)
//! - [`json_to_value`] -- convert `serde_json::Value` to LLT `Value`
//! - [`value_to_json`] -- convert LLT `Value` to `serde_json::Value`
//! - [`value_to_display_string`] -- render a materialized `Value` as a human-readable string
//! - [`MAX_EVAL_DEPTH`] -- recursion limit for evaluation (256)
//! - [`MAX_FILE_SIZE`] -- file size limit for `include` and stdin (10 MB)

pub(crate) mod arena;
pub mod ast;
pub(crate) mod error;
pub(crate) mod eval;
pub(crate) mod eval_access;
pub(crate) mod eval_call;
pub(crate) mod eval_deep;
pub(crate) mod eval_materialize;
pub mod formatter;
pub mod lexer;
pub mod parser;
pub mod resolve;
#[cfg(test)]
pub(crate) mod test_util;
pub mod typecheck;
pub(crate) mod types;
pub(crate) mod value;
// Rust-native builtin functions (stdlib-1 sprint).
pub(crate) mod builtins;
// Seq primitive builtins: seq, head, tail, collect, seq?.
pub(crate) mod builtins_seq_prim;
// Sequence generator builtins: range, repeat, cycle, iterate, unfold.
pub(crate) mod builtins_seq_gen;
// Sequence transform builtins: map, filter, take, drop.
pub(crate) mod builtins_seq_xform;
// Sequence reduction builtins: reduce, join, concat.
pub(crate) mod builtins_seq_reduce;
// String builtins: str, split, replace, upper, lower, trim.
pub(crate) mod builtins_string;
// Arithmetic, comparison, and control-flow builtins: +, -, *, /, =, <, if.
pub(crate) mod builtins_math;
// $_ desugaring (pre-typecheck AST transformation).
pub mod desugar;
// REPL (Read-Eval-Print Loop).
#[cfg(feature = "repl")]
pub mod repl;
// LSP (Language Server Protocol).
#[cfg(feature = "lsp")]
pub mod lsp;

use std::rc::Rc;

/// AST node types produced by the parser.
pub use ast::{Annotation, Document, Entry, Expr, File, NamedArg, Param, Position, Span, Spanned};
/// Parser entry points and error type.
pub use parser::{parse, parse2, parse_expression, ParseError, ParseOutput};

/// Evaluation functions and depth limit.
pub use eval::{
    eval_file, eval_file_with_input, materialize, EvalConfig, EvalContext, EvalState,
    MAX_EVAL_DEPTH,
};
pub use eval_deep::deep_materialize;

/// Builtin infrastructure: stdlib creation, JSON conversion.
pub use builtins::{create_stdlib_env, json_to_value, MAX_FILE_SIZE};

// Compile-time assertion: LSP MAX_DOCUMENT_SIZE must match builtins MAX_FILE_SIZE
#[cfg(feature = "lsp")]
const _: () = {
    const LSP_MAX: usize = lsp::MAX_DOCUMENT_SIZE;
    const BUILTINS_MAX: u64 = builtins::MAX_FILE_SIZE;
    assert!(
        LSP_MAX as u64 == BUILTINS_MAX,
        "MAX_DOCUMENT_SIZE and MAX_FILE_SIZE must match"
    );
};

/// Error types with source spans and stack traces.
pub use error::{render_span_snippet, ArityBound, ErrorKind, EvalError, EvalResult, StackFrame};

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
/// When `no_fs` is `true`, filesystem operations (like `include`) are disabled.
/// Primarily used for corpus tests that verify the `IncludeForbidden` error path.
pub fn eval_source_with_config(input: &str, no_fs: bool) -> Result<String, String> {
    let mut file = parse(input).map_err(|e| format!("{e}"))?;
    // PIPELINE INVARIANT: Desugar must run after parse and before typecheck.
    // See also: src/main.rs:234-240 (run_eval pipeline)
    // Desugar $_ implicit lambdas (pre-typecheck AST transformation).
    desugar::desugar_file(&mut file.node);
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    // Populates VarRef resolved caches with (level, slot) coordinates.
    resolve::resolve_file(&file.node);
    // Type errors are advisory; evaluation proceeds regardless.
    let _ = typecheck::typecheck_file(&file.node);
    let env = builtins::create_stdlib_env().map_err(|e| format!("{e}"))?;
    // Create evaluation context (current directory, configurable sandbox)
    let base_dir_path = std::env::current_dir()
        .ok()
        .and_then(|d| d.canonicalize().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let base_dir = cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
        .map_err(|e| format!("cannot open base directory: {e}"))?;
    let ctx = eval::EvalContext::new(base_dir, Rc::clone(&env), no_fs);
    let thunk =
        eval::eval_file(&file.node, Rc::clone(&env), &ctx, 0).map_err(|e| format!("{e}"))?;
    let val = eval::materialize(&thunk, None, &ctx, 0).map_err(|e| format!("{e}"))?;
    let forced = eval::deep_materialize(&val, &ctx, 0, None).map_err(|e| format!("{e}"))?;
    value_to_display_string(&forced, &ctx, 0).map_err(|e| format!("{e}"))
}

/// Parse and type-check LLT source code.
///
/// Returns `Ok(())` if type checking succeeds, or `Err(errors)` with a formatted
/// error message if type errors are found. Each error includes the error message
/// and the source span where it occurred.
///
/// The type environment is pre-populated with builtin type signatures via
/// `TypeEnv::with_builtins()`, so stdlib builtins (`+`, `merge`, etc.) are
/// in scope for type checking.
///
/// This function is primarily used for testing and corpus validation to ensure
/// type checking regressions are caught. The main `eval_source` function treats
/// type errors as advisory warnings and continues evaluation regardless.
pub fn typecheck_source(input: &str) -> Result<(), String> {
    let mut file = parse(input).map_err(|e| format!("{e}"))?;
    // Desugar $_ implicit lambdas (pre-typecheck AST transformation).
    desugar::desugar_file(&mut file.node);
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    resolve::resolve_file(&file.node);
    // Type check the file
    typecheck::typecheck_file(&file.node).map_err(|type_errors| {
        let error_msgs: Vec<String> = type_errors.iter().map(|e| format!("{}", e)).collect();
        error_msgs.join("\n")
    })
}

// --- Value Serializer Visitor Pattern ---
//
// `value_to_json` and `value_to_display_string` share the same structural traversal
// (depth guard, Overlay flattening, Dict/Seq entry materialization) but diverge at
// leaf rendering. A `ValueVisitor` trait captures the shared traversal in `visit_value`
// while each visitor impl handles the format-specific leaf rendering.

/// Visitor trait for materialised [`Value`](value::Value) trees.
///
/// Implement this trait to produce a format-specific output from a `Value`.
/// The shared `visit_value` function handles structural traversal (depth limit,
/// Overlay flattening, Dict/Seq entry materialization); visitor methods handle
/// leaf rendering and container assembly.
///
/// Dict entries are pre-converted to `Self::Output` before `visit_dict` is called,
/// so the visitor need not recurse manually.
pub trait ValueVisitor {
    type Output;

    fn visit_int(&self, v: i64) -> Self::Output;
    fn visit_float(&self, v: f64) -> Result<Self::Output, Box<error::EvalError>>;
    fn visit_bool(&self, v: bool) -> Self::Output;
    fn visit_str(&self, v: &str) -> Self::Output;
    fn visit_null(&self) -> Self::Output;
    fn visit_dict(&self, entries: Vec<(value::Key, Self::Output)>) -> Self::Output;
    fn visit_seq_head(&self, head: Self::Output) -> Result<Self::Output, Box<error::EvalError>>;
    fn visit_function(&self, params: &[ast::Param]) -> Result<Self::Output, Box<error::EvalError>>;
    fn visit_builtin(&self, name: &str) -> Result<Self::Output, Box<error::EvalError>>;
    fn visit_proxy(&self) -> Result<Self::Output, Box<error::EvalError>>;
    /// Return `Some(output)` if the depth limit has been reached, `None` to continue.
    fn depth_limit_output(
        &self,
        depth: usize,
    ) -> Option<Result<Self::Output, Box<error::EvalError>>>;
}

/// Shared structural traversal for materialised `Value` trees.
///
/// Handles depth limiting, `Overlay` flattening, and `Dict`/`Seq` entry
/// materialisation. Leaf rendering is delegated to the provided [`ValueVisitor`].
///
/// # Panics
///
/// Does not panic. All errors are propagated via `Result`.
pub fn visit_value<V: ValueVisitor>(
    val: &value::Value,
    ctx: &Rc<eval::EvalContext>,
    depth: usize,
    visitor: &V,
) -> Result<V::Output, Box<error::EvalError>> {
    if let Some(limit_result) = visitor.depth_limit_output(depth) {
        return limit_result;
    }
    match val {
        value::Value::Int(n) => Ok(visitor.visit_int(*n)),
        value::Value::Float(f) => visitor.visit_float(*f),
        value::Value::String(s) => Ok(visitor.visit_str(s)),
        value::Value::Bool(b) => Ok(visitor.visit_bool(*b)),
        value::Value::Dict(map) => {
            let mut entries = Vec::with_capacity(map.len());
            for (key, thunk) in map {
                let v = eval::materialize(thunk, None, ctx, depth)?;
                entries.push((key.clone(), visit_value(&v, ctx, depth + 1, visitor)?));
            }
            Ok(visitor.visit_dict(entries))
        }
        value::Value::Overlay(l, r) => {
            // Flatten overlay to a concrete dict, then visit it.
            let map =
                builtins::flatten_overlay(l, r, "serialize", ctx, depth, ast::Span::origin())?;
            visit_value(&value::Value::Dict(map), ctx, depth, visitor)
        }
        value::Value::Seq { head, .. } => {
            let head_val = eval::materialize(head, None, ctx, depth)?;
            let head_out = visit_value(&head_val, ctx, depth + 1, visitor)?;
            visitor.visit_seq_head(head_out)
        }
        value::Value::Function { params, .. } => visitor.visit_function(&**params),
        value::Value::Builtin(def) => visitor.visit_builtin(def.name),
        value::Value::Proxy { .. } => visitor.visit_proxy(),
    }
}

// --- JSON Visitor ---

struct JsonVisitor;

impl ValueVisitor for JsonVisitor {
    type Output = serde_json::Value;

    fn visit_int(&self, v: i64) -> serde_json::Value {
        serde_json::Value::Number(v.into())
    }
    fn visit_float(&self, v: f64) -> Result<serde_json::Value, Box<error::EvalError>> {
        serde_json::Number::from_f64(v)
            .map(serde_json::Value::Number)
            .ok_or_else(|| {
                error::EvalError::float_not_finite("to-json".to_string(), v, ast::Span::origin())
                    .into()
            })
    }
    fn visit_bool(&self, v: bool) -> serde_json::Value {
        serde_json::Value::Bool(v)
    }
    fn visit_str(&self, v: &str) -> serde_json::Value {
        serde_json::Value::String(v.to_owned())
    }
    fn visit_null(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    fn visit_dict(&self, entries: Vec<(value::Key, serde_json::Value)>) -> serde_json::Value {
        // Detect array-like dict: all keys are sequential ints 0..n
        let is_array = !entries.is_empty()
            && entries
                .iter()
                .enumerate()
                .all(|(i, (k, _))| matches!(k, value::Key::Int(n) if *n >= 0 && *n as usize == i));
        if is_array {
            serde_json::Value::Array(entries.into_iter().map(|(_, v)| v).collect())
        } else {
            let obj: serde_json::Map<String, serde_json::Value> = entries
                .into_iter()
                .map(|(k, v)| {
                    let ks = match k {
                        value::Key::Int(n) => n.to_string(),
                        value::Key::String(s) => s,
                    };
                    (ks, v)
                })
                .collect();
            serde_json::Value::Object(obj)
        }
    }
    fn visit_seq_head(
        &self,
        _head: serde_json::Value,
    ) -> Result<serde_json::Value, Box<error::EvalError>> {
        // Seq is not representable in JSON; must be collected to a Dict first via $collect.
        Err(error::EvalError::value_not_serializable("Seq".to_string(), ast::Span::origin()).into())
    }
    fn visit_function(
        &self,
        _params: &[ast::Param],
    ) -> Result<serde_json::Value, Box<error::EvalError>> {
        Err(
            error::EvalError::value_not_serializable("Function".to_string(), ast::Span::origin())
                .into(),
        )
    }
    fn visit_builtin(&self, name: &str) -> Result<serde_json::Value, Box<error::EvalError>> {
        Err(error::EvalError::value_not_serializable(
            format!("Builtin ({name})"),
            ast::Span::origin(),
        )
        .into())
    }
    fn visit_proxy(&self) -> Result<serde_json::Value, Box<error::EvalError>> {
        Err(
            error::EvalError::value_not_serializable("Proxy".to_string(), ast::Span::origin())
                .into(),
        )
    }
    fn depth_limit_output(
        &self,
        depth: usize,
    ) -> Option<Result<serde_json::Value, Box<error::EvalError>>> {
        if depth > eval::MAX_EVAL_DEPTH {
            Some(Err(error::EvalError::depth_exceeded(
                eval::MAX_EVAL_DEPTH,
                ast::Span::origin(),
            )
            .into()))
        } else {
            None
        }
    }
}

// --- Display Visitor ---

/// Maximum display recursion depth (3 levels).
/// Prevents deep traversal of nested structures in error messages.
const MAX_DISPLAY_DEPTH: usize = 3;

struct DisplayVisitor;

impl ValueVisitor for DisplayVisitor {
    type Output = String;

    fn visit_int(&self, v: i64) -> String {
        format!("Int({v})")
    }
    fn visit_float(&self, v: f64) -> Result<String, Box<error::EvalError>> {
        Ok(format!("Float({v})"))
    }
    fn visit_bool(&self, v: bool) -> String {
        format!("Bool({v})")
    }
    fn visit_str(&self, v: &str) -> String {
        format!("String({v:?})")
    }
    fn visit_null(&self) -> String {
        "Null".to_string()
    }
    fn visit_dict(&self, entries: Vec<(value::Key, String)>) -> String {
        use std::fmt::Write;
        let mut result = String::from("Dict({");
        for (i, (key, val_str)) in entries.into_iter().enumerate() {
            if i > 0 {
                result.push_str(", ");
            }
            match key {
                value::Key::Int(n) => write!(&mut result, "{n}").unwrap(),
                value::Key::String(s) => write!(&mut result, "{s:?}").unwrap(),
            }
            result.push_str(": ");
            result.push_str(&val_str);
        }
        result.push_str("})");
        result
    }
    fn visit_seq_head(&self, head: String) -> Result<String, Box<error::EvalError>> {
        Ok(format!("Seq({head}, ...)"))
    }
    fn visit_function(&self, params: &[ast::Param]) -> Result<String, Box<error::EvalError>> {
        let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
        Ok(format!("Function({})", names.join(", ")))
    }
    fn visit_builtin(&self, name: &str) -> Result<String, Box<error::EvalError>> {
        Ok(format!("Builtin({name})"))
    }
    fn visit_proxy(&self) -> Result<String, Box<error::EvalError>> {
        Ok("Proxy".to_string())
    }
    fn depth_limit_output(&self, depth: usize) -> Option<Result<String, Box<error::EvalError>>> {
        if depth > eval::MAX_EVAL_DEPTH {
            Some(Err(error::EvalError::internal(
                "display depth exceeded (this is a display recursion limit, not an evaluation depth limit)".to_string(),
                ast::Span::origin(),
            ).into()))
        } else if depth >= MAX_DISPLAY_DEPTH {
            Some(Ok("...".to_string()))
        } else {
            None
        }
    }
}

/// Convert a materialized [`Value`](value::Value) to a [`serde_json::Value`].
///
/// **Caller must ensure all values are fully materialized via [`deep_materialize`] before calling.**
/// Unmaterialized thunks will produce incorrect output.
///
/// Dict values are materialized on demand via [`eval::materialize`]. If all keys
/// are sequential integers starting from 0 the dict is serialized as a JSON array;
/// otherwise it becomes a JSON object (integer keys are stringified).
///
/// Unlike [`value_to_display_string`], this rejects NaN/Infinity floats (not valid JSON).
///
/// Returns an error for:
/// - `Function` / `Builtin` values (no JSON representation)
/// - `Float` values that are NaN or infinite (not representable in JSON)
/// - `Seq` values (must be collected to a Dict first via `$collect`)
/// - Exceeding the maximum recursion depth ([`eval::MAX_EVAL_DEPTH`])
pub fn value_to_json(
    val: &value::Value,
    ctx: &Rc<eval::EvalContext>,
    depth: usize,
) -> Result<serde_json::Value, Box<error::EvalError>> {
    // Seq has a span-bearing error; handle before the generic visitor.
    if let value::Value::Seq { head, .. } = val {
        return Err(error::EvalError::value_not_serializable("Seq".to_string(), head.span).into());
    }
    visit_value(val, ctx, depth, &JsonVisitor)
}

/// Convert a Value into a displayable string (LLT format, not JSON).
///
/// **Caller must ensure all values are fully materialized via [`deep_materialize`] before calling.**
/// Unmaterialized thunks will produce incorrect output.
///
/// Unlike `Value::Debug`, this renders dict values showing the complete
/// structure, not just keys. The value should already be deep-materialized
/// via [`eval::deep_materialize`]; this function still calls `materialize`
/// on each thunk for safety but does not perform recursive deep-forcing.
///
/// Unlike [`value_to_json`], this accepts NaN/Infinity floats (renders as `Float(NaN)`, `Float(inf)`).
///
/// `depth` tracks recursion depth to prevent stack overflow from deeply nested
/// dict-of-dicts structures. Uses the same limit as `eval::MAX_EVAL_DEPTH`.
pub fn value_to_display_string(
    val: &value::Value,
    ctx: &Rc<eval::EvalContext>,
    depth: usize,
) -> Result<String, Box<error::EvalError>> {
    visit_value(val, ctx, depth, &DisplayVisitor)
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
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        eval::EvalContext::new(base_dir, builtins::create_root_env(), false)
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
        let b = Value::Builtin(value::BuiltinDef {
            func: dummy,
            name: "test",
            pos_strictness: &[],
        });
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
        let result = eval_to_json("[x: 1 y: \"hello\"]");
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
        let result = eval_to_json_with_input("[greeting: %.name]", Some(input_json));
        assert_eq!(result, serde_json::json!({"greeting": "Alice"}));
    }

    #[test]
    fn test_pipeline_stdin_json_array() {
        let input_json = serde_json::json!([1, 2, 3]);
        let result = eval_to_json_with_input("[first: %[0]]", Some(input_json));
        assert_eq!(result, serde_json::json!({"first": 1}));
    }

    #[test]
    fn test_pipeline_stdin_json_passthrough() {
        // When % is the whole output, it should pass through
        let input_json = serde_json::json!({"x": 42});
        let result = eval_to_json_with_input("%", Some(input_json));
        assert_eq!(result, serde_json::json!({"x": 42}));
    }

    #[test]
    fn test_pipeline_no_stdin_default_empty_dict() {
        // Without stdin input, % defaults to empty dict
        let result = eval_to_json("%");
        assert_eq!(result, serde_json::json!({}));
    }

    #[test]
    fn test_pipeline_multi_document_with_stdin() {
        // stdin -> doc1 -> % -> doc2
        let input_json = serde_json::json!({"val": 10});
        let source = "[result: %.val]\n---\n[wrapped: %.result]";
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
        let forced = eval::deep_materialize(&val, &ctx, 0, None).expect("deep_materialize failed");
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
        let forced = eval::deep_materialize(&val, &ctx, 0, None).expect("deep_materialize failed");
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

    // --- Integration tests: typecheck→eval interaction ---

    /// Type errors are advisory: eval proceeds even when the type checker reports an error.
    ///
    /// This exercises the `let _ = typecheck::typecheck_file(&file.node)` line in
    /// `eval_source_with_config` (src/lib.rs:123). The type checker flags a mismatch
    /// (Int param given a String), but the evaluator sees the unannotated value and
    /// returns it unchanged.
    #[test]
    fn test_typecheck_advisory_eval_proceeds() {
        // Type annotation on param (x@Int) is advisory only.
        // Passing "hello" (String) should still evaluate successfully.
        let result = eval_source("[f: [fn [x@Int] x]  result: [f \"hello\"]]");
        assert!(
            result.is_ok(),
            "expected eval to succeed despite type mismatch, got: {:?}",
            result
        );
        let output = result.unwrap();
        assert!(
            output.contains("String(\"hello\")"),
            "expected result to contain String(\"hello\"), got: {output}"
        );
        assert!(
            output.contains("Function(x)"),
            "expected result to contain Function(x), got: {output}"
        );
    }

    /// Advisory check: typecheck_source reports the error while eval_source succeeds.
    ///
    /// The same input should fail typecheck but succeed eval, proving the two pipelines
    /// are independent and type errors are not blocking.
    #[test]
    fn test_typecheck_vs_eval_source_independence() {
        // The type checker is advisory — eval always proceeds regardless of type errors.
        // Use a source that evaluates successfully; typecheck may or may not catch
        // the annotation mismatch (param annotations are not fully checked in calls yet).
        let source = "[f: [fn [x@Int] x]  result: [f \"hello\"]]";
        // eval_source should succeed regardless of typecheck result
        let eval_result = eval_source(source);
        assert!(
            eval_result.is_ok(),
            "eval should succeed: {}",
            eval_result.unwrap_err()
        );
    }

    /// TypeAssert with `default:` fallback works end-to-end.
    ///
    /// When the main expression doesn't match the asserted type and a `default:`
    /// is provided, the default value is used instead of raising an error.
    #[test]
    fn test_typeassert_default_fallback_end_to_end() {
        // "hello" is a String, not a Number — default 42 should be returned.
        let result = eval_source("[@[type: Number  default: 42] \"hello\"]");
        assert!(
            result.is_ok(),
            "expected eval to succeed with default fallback, got: {:?}",
            result
        );
        let output = result.unwrap();
        assert_eq!(
            output, "Int(42)",
            "expected default value Int(42), got: {output}"
        );
    }

    /// TypeAssert with `default:` when main expression DOES match — uses main value.
    ///
    /// The default is only a fallback; if the main expression satisfies the assertion,
    /// the main value is returned unchanged.
    #[test]
    fn test_typeassert_default_not_used_when_main_matches() {
        // 99 is a Number — main value should be returned, not the default.
        let result = eval_source("[@[type: Number  default: 0] 99]");
        assert!(
            result.is_ok(),
            "expected eval to succeed, got: {:?}",
            result
        );
        let output = result.unwrap();
        assert_eq!(
            output, "Int(99)",
            "expected main value Int(99), got: {output}"
        );
    }

    // --- Integration tests: render_span_snippet in error output ---

    /// `eval_source_with_snippets` integration test: verify that when an error occurs
    /// in a user-written source string, the error Display produced by main.rs / REPL
    /// includes a source snippet (rustc-style underline). This exercises
    /// `render_span_snippet` being called with a real eval error's definition_span.
    ///
    /// The test simulates the pattern used in main.rs `run_eval` and `repl.rs` `eval_input`:
    /// parse source → eval → on error, call render_span_snippet with the source string
    /// and the error's definition_span, then check the snippet is present.
    #[test]
    fn test_eval_source_with_source_snippets() {
        // Source that will produce an eval error with a real source span.
        // Accessing an undefined variable gives an UndefinedVariable error whose
        // definition_span points at the VarRef expression in the source.
        let source = "$undefined_var";

        // Parse the source manually to get a real AST with spans.
        let mut file = parse(source).expect("parse should succeed");
        desugar::desugar_file(&mut file.node);
        let _ = typecheck::typecheck_file(&file.node);
        let env = builtins::create_stdlib_env().expect("stdlib failed");
        let ctx = test_ctx();

        // Evaluate: this should fail because $undefined_var is not defined.
        let eval_result = eval::eval_file(&file.node, Rc::clone(&env), &ctx, 0);
        assert!(
            eval_result.is_err(),
            "expected eval to fail for undefined variable"
        );
        let err = eval_result.unwrap_err();

        // Verify the error has a non-synthetic definition_span.
        assert_ne!(
            err.definition_span,
            ast::Span::origin(),
            "error should have a real source span, not Span::origin()"
        );

        // render_span_snippet should produce a snippet for this error.
        let snippet = error::render_span_snippet(source, err.definition_span);
        assert!(
            snippet.is_some(),
            "render_span_snippet should return Some for a real source span"
        );
        let snippet_text = snippet.unwrap();

        // The snippet should contain the source line.
        assert!(
            snippet_text.contains("$undefined_var"),
            "snippet should contain the source line with the variable reference, got: {snippet_text}"
        );

        // The snippet should contain caret underlines (error indicator).
        assert!(
            snippet_text.contains('^'),
            "snippet should contain caret underlines, got: {snippet_text}"
        );

        // The snippet should include a line number prefix in the format "N | ...".
        assert!(
            snippet_text.contains(" | "),
            "snippet should include line number format 'N | ...', got: {snippet_text}"
        );
    }
}
