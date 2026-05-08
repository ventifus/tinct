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
pub mod ast_dict;
pub(crate) mod coverage;
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
// Import resolution for type checker — seeds TypeEnv with prelude function types.
pub(crate) mod imports;
// Rust-native builtin functions (stdlib-1 sprint).
pub(crate) mod builtins;
// Dict/access builtins: keys, length, merge, append, get, each, each-key, each-kv.
pub(crate) mod builtins_dict;
// I/O builtins: dir-cap, open, slurp, write, connect, lines, emit, env.
pub(crate) mod builtins_io;
// Arithmetic, comparison, and control-flow builtins: +, -, *, /, =, <, if.
pub(crate) mod builtins_math;
// Type/eval/meta builtins: type-of, eval, include, error, try, apply, validate.
pub(crate) mod builtins_meta;
// Seq primitive builtins: seq, head, tail, collect, seq?.
pub(crate) mod builtins_seq_prim;
// Sequence generator builtins: range, repeat, cycle, iterate, unfold.
pub(crate) mod builtins_seq_gen;
// Sequence reduction builtins: reduce, join, concat.
pub(crate) mod builtins_seq_reduce;
// Sequence transform builtins: map, filter, take, drop.
pub(crate) mod builtins_seq_xform;
// String builtins: str, split, replace, upper, lower, trim.
pub(crate) mod builtins_string;
// $_ desugaring (pre-typecheck AST transformation).
pub mod desugar;
// Macro expansion (pre-desugar AST transformation).
pub mod expand;
// Literate tinct: extract and evaluate tinct code blocks from Markdown files.
pub mod literate;
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

/// Builtin infrastructure: stdlib creation, JSON conversion, resource limits.
pub use builtins::{
    create_root_env, create_stdlib_env, json_to_value, MAX_COLLECT_SIZE, MAX_FILE_SIZE,
};

/// Import resolution for the type checker.
pub use imports::{build_prelude_env, build_type_env};

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
pub use formatter::{format_source, format_source_compact, format_source_tinct};

#[cfg(feature = "repl")]
pub use repl::run_repl;

#[cfg(feature = "lsp")]
pub use lsp::run_lsp;

/// Runtime value types: values, thunks, environments, and dict keys.
pub use value::{Environment, Key, NetCapEntry, Thunk, Value};

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
    let file = parse(input).map_err(|e| format!("{e}"))?;
    // PIPELINE INVARIANT: expand_macros -> desugar -> typecheck -> eval.
    // See also: src/main.rs:234-240 (run_eval pipeline)
    // Expand macros (pre-desugar AST transformation).
    let expand_result = expand::expand_macros(file, no_fs).map_err(|e| format!("{e}"))?;
    let mut file = expand_result.file;
    let provenance = expand_result.provenance;

    // Helper: attach macro expansion provenance to errors before formatting.
    // Checks if the error's definition span, materialization span, or any stack
    // frame span matches a provenance entry from macro expansion.
    let attach_provenance = |mut e: Box<error::EvalError>| -> String {
        if e.macro_expansion.is_none() {
            // Check definition span
            let mut found = provenance.get(&expand::SpanKey::from(e.definition_span));
            // Check materialization span
            if found.is_none() {
                if let Some(mat_span) = e.materialization_span {
                    found = provenance.get(&expand::SpanKey::from(mat_span));
                }
            }
            // Check stack frame spans
            if found.is_none() {
                for frame in &e.stack {
                    if let Some(prov) = provenance.get(&expand::SpanKey::from(frame.span)) {
                        found = Some(prov);
                        break;
                    }
                }
            }
            if let Some(prov) = found {
                e.macro_expansion = Some((prov.macro_name.clone(), prov.call_site_span));
            }
        }
        format!("{e}")
    };
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
        eval::eval_file(&file.node, Rc::clone(&env), &ctx, 0).map_err(&attach_provenance)?;
    let val = eval::materialize(&thunk, None, &ctx, 0).map_err(&attach_provenance)?;
    let forced = eval::deep_materialize(&val, &ctx, 0, None).map_err(&attach_provenance)?;
    value_to_display_string(&forced, &ctx, 0).map_err(&attach_provenance)
}

/// Parse and type-check LLT source code.
///
/// Returns `Ok(())` if type checking succeeds, or `Err(errors)` with a formatted
/// error message if type errors are found. Each error includes the error message
/// and the source span where it occurred.
///
/// The type environment is pre-populated with builtin type signatures AND prelude
/// function types via `imports::build_prelude_env()`, so stdlib builtins (`+`, `merge`)
/// and prelude functions (`map`, `filter`, `any?`, etc.) are in scope for type checking.
///
/// This function is primarily used for testing and corpus validation to ensure
/// type checking regressions are caught. The main `eval_source` function treats
/// type errors as advisory warnings and continues evaluation regardless.
pub fn typecheck_source(input: &str) -> Result<(), String> {
    let file = parse(input).map_err(|e| format!("{e}"))?;
    // PIPELINE INVARIANT: expand_macros -> desugar -> typecheck.
    // Expand macros (pre-desugar AST transformation).
    let expand_result = expand::expand_macros(file, false).map_err(|e| format!("{e}"))?;
    let mut file = expand_result.file;
    // Desugar $_ implicit lambdas (pre-typecheck AST transformation).
    desugar::desugar_file(&mut file.node);
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    resolve::resolve_file(&file.node);
    // Type check the file with prelude-seeded environment
    let env = imports::build_prelude_env();
    let (type_errors, _type_map, _doc_map) =
        typecheck::typecheck_file_with_types_and_env(&file.node, env);
    if type_errors.is_empty() {
        Ok(())
    } else {
        let error_msgs: Vec<String> = type_errors.iter().map(|e| format!("{}", e)).collect();
        Err(error_msgs.join("\n"))
    }
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
    fn visit_variant(&self, tag: String, payload: Self::Output) -> Self::Output;
    fn visit_decimal(&self, v: rust_decimal::Decimal) -> Self::Output;
    fn visit_bigint(&self, v: &num_bigint::BigInt) -> Self::Output;
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
        value::Value::String {
            ref source,
            start,
            end,
        } => {
            let s = &source[*start..*end];
            Ok(visitor.visit_str(s))
        }
        value::Value::Bool(b) => Ok(visitor.visit_bool(*b)),
        value::Value::Dict(map) => {
            let mut entries = Vec::with_capacity(map.len());
            for (key, thunk_id) in map {
                let thunk = ctx.get_thunk(*thunk_id);
                let v = eval::materialize(&thunk, None, ctx, depth)?;
                entries.push((key.clone(), visit_value(&v, ctx, depth + 1, visitor)?));
            }
            Ok(visitor.visit_dict(entries))
        }
        value::Value::Overlay(l, r) => {
            // Flatten overlay to a concrete dict, then visit it.
            let map = builtins::flatten_overlay(
                l,
                r,
                "value serialization",
                ctx,
                depth,
                ast::Span::origin(),
            )?;
            visit_value(&value::Value::Dict(map), ctx, depth, visitor)
        }
        value::Value::Seq { head, .. } => {
            let head_thunk = ctx.get_thunk(*head);
            let head_val = eval::materialize(&head_thunk, None, ctx, depth)?;
            let head_out = visit_value(&head_val, ctx, depth + 1, visitor)?;
            visitor.visit_seq_head(head_out)
        }
        value::Value::Function { params, .. } => visitor.visit_function(&**params),
        value::Value::Builtin(def) => visitor.visit_builtin(def.name),
        value::Value::Proxy { .. } => visitor.visit_proxy(),
        value::Value::DirCap(_) => Err(Box::new(error::EvalError::value_not_serializable(
            "DirCap".to_string(),
            ast::Span::origin(),
        ))),
        value::Value::NetCap(_) => Err(Box::new(error::EvalError::value_not_serializable(
            "NetCap".to_string(),
            ast::Span::origin(),
        ))),
        value::Value::Handle(_) => Err(Box::new(error::EvalError::value_not_serializable(
            "Handle".to_string(),
            ast::Span::origin(),
        ))),
        value::Value::RevocableDirCap { .. } => Err(Box::new(
            error::EvalError::value_not_serializable("DirCap".to_string(), ast::Span::origin()),
        )),
        value::Value::Variant { tag, payload } => {
            let payload_output = match payload {
                Some(thunk_id) => {
                    let thunk = ctx.get_thunk(*thunk_id);
                    let v = eval::materialize(&thunk, None, ctx, depth)?;
                    visit_value(&v, ctx, depth + 1, visitor)?
                }
                None => visitor.visit_null(),
            };
            Ok(visitor.visit_variant(tag.clone(), payload_output))
        }
        value::Value::Decimal(d) => Ok(visitor.visit_decimal(*d)),
        value::Value::BigInt(n) => Ok(visitor.visit_bigint(n)),
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
    fn visit_variant(&self, tag: String, payload: serde_json::Value) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert(tag, payload);
        serde_json::Value::Object(obj)
    }
    fn visit_decimal(&self, v: rust_decimal::Decimal) -> serde_json::Value {
        // Serialize Decimal as JSON number string to preserve exact representation
        use std::str::FromStr;
        serde_json::Value::Number(
            serde_json::Number::from_str(&v.to_string())
                .unwrap_or_else(|_| serde_json::Number::from(0)),
        )
    }
    fn visit_bigint(&self, v: &num_bigint::BigInt) -> serde_json::Value {
        // BigInt serializes as JSON number string. May exceed JSON receiver's i64 range.
        use std::str::FromStr;
        serde_json::Value::Number(
            serde_json::Number::from_str(&v.to_string())
                .unwrap_or_else(|_| serde_json::Number::from(0)),
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
    fn visit_variant(&self, tag: String, payload: String) -> String {
        format!("Variant({tag}, {payload})")
    }
    fn visit_decimal(&self, v: rust_decimal::Decimal) -> String {
        format!("Decimal({v})")
    }
    fn visit_bigint(&self, v: &num_bigint::BigInt) -> String {
        format!("BigInt({v})")
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
        let head_thunk = ctx.get_thunk(*head);
        return Err(
            error::EvalError::value_not_serializable("Seq".to_string(), head_thunk.span).into(),
        );
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

/// Format a tinct value as a compact JSON string using `stdlib/out/json.llt`.
///
/// Reads and evaluates the json.llt file at `json_llt_path`, then calls its
/// `json` function with `result_thunk` as the argument in the same evaluation
/// context as the main program. This ensures all `ThunkId` references in the
/// result value are resolved from the correct arena.
///
/// # Returns
///
/// - `Ok(Some(json_string))` — json.llt produced a string.
/// - `Ok(None)` — `json_llt_path` does not exist; the caller should fall back
///   to [`value_to_json`].
/// - `Err(message)` — parse or evaluation error from json.llt.
///
/// # Laziness note
///
/// json.llt contains `[emit [json %]]` as a lazy auto-indexed dict entry. This
/// entry is **never forced** by this function — only the `json` key (a function)
/// is accessed, so no `emit` side-effect fires.
pub fn format_with_json_llt(
    result_thunk: Rc<value::Thunk>,
    eval_ctx: &Rc<eval::EvalContext>,
    env: Rc<std::cell::RefCell<value::Environment>>,
    json_llt_path: &std::path::Path,
) -> Result<Option<String>, String> {
    use eval_call::{invoke_function, CallContext};
    use value::Key;

    // Bail early if json.llt does not exist — caller will fall back.
    if !json_llt_path.is_file() {
        return Ok(None);
    }

    // Read json.llt source.
    // Use Ok(None) on any read failure (e.g. Landlock blocks access when --allow-path is set).
    // The caller falls back to value_to_json() in that case.
    let json_llt_source = match std::fs::read_to_string(json_llt_path) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };

    // Parse json.llt (it's a single dict expression — one document).
    let mut ast = parse(&json_llt_source).map_err(|e| format!("json.llt: parse error: {e}"))?;
    desugar::desugar_file(&mut ast.node);
    resolve::resolve_file(&ast.node);
    let _ = typecheck::typecheck_file(&ast.node);

    // Evaluate json.llt in the SAME eval_ctx as the main program so all ThunkIds
    // from the result_thunk are resolvable when the json functions access dict entries.
    // The initial `%` = result_thunk; json.llt's `[emit [json %]]` is a lazy dict
    // entry (auto-index 0) that is never forced here.
    let module_thunk = eval::eval_file_with_input(
        &ast.node,
        Rc::clone(&env),
        eval_ctx,
        Some(Rc::clone(&result_thunk)),
        0,
    )
    .map_err(|e| format!("json.llt: eval error: {e}"))?;

    // Materialize the json module dict (forces the outer dict, not its entries).
    let module_val = eval::materialize(&module_thunk, None, eval_ctx, 0)
        .map_err(|e| format!("json.llt: materialize module error: {e}"))?;

    // Look up the `json` key from the module dict.
    let json_key = Key::String("json".into());
    let json_fn_thunk = match &module_val {
        value::Value::Dict(map) => {
            let thunk_id = map
                .get(&json_key)
                .ok_or_else(|| "json.llt: missing 'json' function in module dict".to_string())?;
            eval_ctx.get_thunk(*thunk_id)
        }
        other => {
            return Err(format!(
                "json.llt: expected Dict from module, got {}",
                other.type_name()
            ))
        }
    };

    // Materialize the `json` function thunk.
    let json_fn_val = eval::materialize(&json_fn_thunk, None, eval_ctx, 0)
        .map_err(|e| format!("json.llt: materialize json function error: {e}"))?;

    // Call `json(result_thunk)` via invoke_function.
    let call_span = ast::Span::origin();
    // Bind the positional argument in an explicit local so the slice reference is valid
    // for the entire `call_ctx` lifetime (required by CallContext<'a>).
    let positional_args = [Rc::clone(&result_thunk)];
    let result_call_thunk = match &json_fn_val {
        value::Value::Function {
            params,
            body,
            env: closure_env,
        } => {
            let call_ctx = CallContext {
                params,
                body,
                closure_env,
                positional: &positional_args,
                named: None,
                default_env: closure_env,
                call_span,
                depth: 0,
                origin: None,
                ctx: eval_ctx,
            };
            invoke_function(&call_ctx).map_err(|e| format!("json.llt: call error: {e}"))?
        }
        other => {
            return Err(format!(
                "json.llt: 'json' key is {}, expected Function",
                other.type_name()
            ))
        }
    };

    // Materialize the result — should be a String.
    let result_val = eval::materialize(&result_call_thunk, None, eval_ctx, 0)
        .map_err(|e| format!("json.llt: serialize error: {e}"))?;

    match result_val {
        value::Value::String {
            ref source,
            start,
            end,
        } => {
            let s = source[start..end].to_string();
            Ok(Some(s))
        }
        other => Err(format!(
            "json.llt: expected String result, got {}",
            other.type_name()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use std::cell::RefCell;
    use std::rc::Rc;
    use test_util::test_span;
    use value::{string_val, Environment, Key, Thunk, Value};

    /// Helper: wrap a Value in a materialized thunk.
    fn thunk(val: Value) -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(val, test_span(1, 1, 1, 1)))
    }

    /// Build a materialized dict thunk with entries allocated into `ctx`'s arena.
    #[allow(dead_code)]
    fn thunk_dict(map: IndexMap<Key, Rc<Thunk>>, ctx: &Rc<eval::EvalContext>) -> Rc<Thunk> {
        let mut id_map: IndexMap<Key, value::ThunkId> = IndexMap::with_capacity(map.len());
        for (k, v) in map {
            id_map.insert(k, ctx.alloc_thunk(v));
        }
        Rc::new(Thunk::new_materialized(
            Value::Dict(id_map),
            test_span(1, 1, 1, 1),
        ))
    }

    /// Build a materialized Seq thunk with head and tail allocated into `ctx`'s arena.
    #[allow(dead_code)]
    fn seq_thunk(head: Rc<Thunk>, tail: Rc<Thunk>, ctx: &Rc<eval::EvalContext>) -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(
            Value::Seq {
                head: ctx.alloc_thunk(head),
                tail: ctx.alloc_thunk(tail),
            },
            test_span(1, 1, 1, 1),
        ))
    }

    /// Build a Proxy thunk with the handler allocated into `ctx`'s arena.
    #[allow(dead_code)]
    fn proxy_thunk(handler: Rc<Thunk>, ctx: &Rc<eval::EvalContext>) -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(
            Value::Proxy {
                handler: ctx.alloc_thunk(handler),
            },
            test_span(1, 1, 1, 1),
        ))
    }

    /// Build a `Value::Dict` with entries allocated into `ctx`'s arena.
    fn make_dict(map: IndexMap<Key, Rc<Thunk>>, ctx: &Rc<eval::EvalContext>) -> Value {
        let mut id_map: IndexMap<Key, value::ThunkId> = IndexMap::with_capacity(map.len());
        for (k, v) in map {
            id_map.insert(k, ctx.alloc_thunk(v));
        }
        Value::Dict(id_map)
    }

    fn test_ctx() -> Rc<eval::EvalContext> {
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        eval::EvalContext::new(
            base_dir,
            builtins::create_stdlib_env().expect("stdlib failed"),
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
        let result = value_to_json(&string_val("hello"), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!("hello"));
    }

    #[test]
    fn test_json_string_empty() {
        let result = value_to_json(&string_val(""), &test_ctx(), 0).unwrap();
        assert_eq!(result, serde_json::json!(""));
    }

    #[test]
    fn test_json_string_with_special_chars() {
        let result = value_to_json(&string_val("line\nnewline"), &test_ctx(), 0).unwrap();
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
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("name".into()), thunk(string_val("Alice")));
        map.insert(Key::String("age".into()), thunk(Value::Int(30)));
        let val = make_dict(map, &ctx);
        let result = value_to_json(&val, &ctx, 0).unwrap();
        assert_eq!(result, serde_json::json!({"name": "Alice", "age": 30}));
    }

    #[test]
    fn test_json_dict_int_keys_non_sequential() {
        // Int keys that are NOT sequential from 0 -> object with stringified keys
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(5), thunk(string_val("five")));
        map.insert(Key::Int(10), thunk(string_val("ten")));
        let val = make_dict(map, &ctx);
        let result = value_to_json(&val, &ctx, 0).unwrap();
        assert_eq!(result, serde_json::json!({"5": "five", "10": "ten"}));
    }

    #[test]
    fn test_json_dict_mixed_keys() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(string_val("zero")));
        map.insert(Key::String("x".into()), thunk(Value::Int(1)));
        let val = make_dict(map, &ctx);
        let result = value_to_json(&val, &ctx, 0).unwrap();
        assert_eq!(result, serde_json::json!({"0": "zero", "x": 1}));
    }

    #[test]
    fn test_json_dict_array_like() {
        // Sequential int keys 0, 1, 2 -> JSON array
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(string_val("a")));
        map.insert(Key::Int(1), thunk(string_val("b")));
        map.insert(Key::Int(2), thunk(string_val("c")));
        let val = make_dict(map, &ctx);
        let result = value_to_json(&val, &ctx, 0).unwrap();
        assert_eq!(result, serde_json::json!(["a", "b", "c"]));
    }

    #[test]
    fn test_json_dict_array_single_element() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::Bool(true)));
        let val = make_dict(map, &ctx);
        let result = value_to_json(&val, &ctx, 0).unwrap();
        assert_eq!(result, serde_json::json!([true]));
    }

    #[test]
    fn test_json_dict_array_wrong_order() {
        // Keys are 0 and 1, but inserted in wrong order in IndexMap -> not sequential
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(1), thunk(string_val("b")));
        map.insert(Key::Int(0), thunk(string_val("a")));
        // First key is 1 at index 0 -> not array-like
        let val = make_dict(map, &ctx);
        let result = value_to_json(&val, &ctx, 0).unwrap();
        assert_eq!(result, serde_json::json!({"1": "b", "0": "a"}));
    }

    #[test]
    fn test_json_dict_array_starting_at_one() {
        // Keys 1, 2, 3 -- not starting from 0, so object
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(1), thunk(Value::Int(10)));
        map.insert(Key::Int(2), thunk(Value::Int(20)));
        let val = make_dict(map, &ctx);
        let result = value_to_json(&val, &ctx, 0).unwrap();
        assert_eq!(result, serde_json::json!({"1": 10, "2": 20}));
    }

    #[test]
    fn test_json_nested_dict() {
        let ctx = test_ctx();
        let mut inner = IndexMap::new();
        inner.insert(Key::String("x".into()), thunk(Value::Int(1)));
        let inner_val = make_dict(inner, &ctx);
        let mut outer = IndexMap::new();
        outer.insert(Key::String("inner".into()), thunk(inner_val));
        outer.insert(Key::String("y".into()), thunk(Value::Int(2)));
        let val = make_dict(outer, &ctx);
        let result = value_to_json(&val, &ctx, 0).unwrap();
        assert_eq!(result, serde_json::json!({"inner": {"x": 1}, "y": 2}));
    }

    #[test]
    fn test_json_array_of_objects() {
        let ctx = test_ctx();
        let mut obj1 = IndexMap::new();
        obj1.insert(Key::String("name".into()), thunk(string_val("Alice")));
        let obj1_val = make_dict(obj1, &ctx);
        let mut obj2 = IndexMap::new();
        obj2.insert(Key::String("name".into()), thunk(string_val("Bob")));
        let obj2_val = make_dict(obj2, &ctx);

        let mut arr = IndexMap::new();
        arr.insert(Key::Int(0), thunk(obj1_val));
        arr.insert(Key::Int(1), thunk(obj2_val));
        let val = make_dict(arr, &ctx);
        let result = value_to_json(&val, &ctx, 0).unwrap();
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
        let ctx = test_ctx();
        let seq = {
            let head_thunk = Rc::new(Thunk::new_materialized(
                Value::Int(1),
                test_span(1, 1, 1, 1),
            ));
            let tail_thunk = Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                test_span(1, 1, 1, 1),
            ));
            Value::Seq {
                head: ctx.alloc_thunk(head_thunk),
                tail: ctx.alloc_thunk(tail_thunk),
            }
        };
        let err = value_to_json(&seq, &ctx, 0).unwrap_err();
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
        let ctx = test_ctx();
        let handler_thunk = Rc::new(Thunk::new_materialized(Value::Int(0), ast::Span::origin()));
        let proxy = Value::Proxy {
            handler: ctx.alloc_thunk(handler_thunk),
        };
        let err = value_to_json(&proxy, &ctx, 0).unwrap_err();
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
            builtins::json_to_value(&json, 0, ast::Span::origin(), &ctx)
                .expect("json_to_value failed")
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
        // Access the 0th element of the pipeline array via get builtin
        // Bracket access removed — use [get 0 %] instead of %[0]
        let input_json = serde_json::json!([1, 2, 3]);
        let result = eval_to_json_with_input("[first: [get 0 %]]", Some(input_json));
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
        let ctx = test_ctx();
        let seq = {
            let head_thunk = Rc::new(Thunk::new_materialized(
                Value::Int(1),
                test_span(1, 1, 1, 1),
            ));
            let tail_thunk = Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                test_span(1, 1, 1, 1),
            ));
            Value::Seq {
                head: ctx.alloc_thunk(head_thunk),
                tail: ctx.alloc_thunk(tail_thunk),
            }
        };
        let display = value_to_display_string(&seq, &ctx, 0).expect("display failed");
        assert_eq!(display, "Seq(Int(1), ...)");
    }

    #[test]
    fn test_display_proxy() {
        let ctx = test_ctx();
        let handler_thunk = Rc::new(Thunk::new_materialized(
            Value::Int(42),
            test_span(1, 1, 1, 1),
        ));
        let proxy = Value::Proxy {
            handler: ctx.alloc_thunk(handler_thunk),
        };
        let display = value_to_display_string(&proxy, &ctx, 0).expect("display failed");
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

    /// Integration test: `typecheck_source` resolves the prelude `map` function.
    ///
    /// Calling `[call $map [fn [x] x] [1 2 3]]` should type-check without any
    /// "undefined variable" error for `map`, proving that `build_prelude_env()`
    /// is wired into `typecheck_source` and that prelude functions are in scope.
    #[test]
    fn typecheck_source_resolves_prelude_map() {
        let result = typecheck_source("[call $map [fn [x] x] [1 2 3]]");
        assert!(
            result.is_ok(),
            "expected typecheck_source to succeed (no undefined-variable error for map), got: {:?}",
            result
        );
    }

    /// Integration test: an error whose `definition_span` covers multiple lines renders
    /// ALL spanned lines in the snippet (no truncation with "...").
    ///
    /// We construct a `Span` directly over a known multi-line source string and call
    /// `render_span_snippet` — the same call that `main.rs` and the REPL make. This
    /// verifies the full pipeline from span → snippet without needing an eval error
    /// that naturally produces a multi-line span (which requires syntactic constructs
    /// the evaluator doesn't currently tag that way).
    #[test]
    fn test_multiline_span_snippet_shows_all_lines() {
        use crate::ast::{Position, Span};

        // A three-line expression:
        //   line 1: "let x = ["
        //   line 2: "  missing_key"
        //   line 3: "]"
        let source = "let x = [\n  missing_key\n]";

        // Span covering the entire expression: line 1 col 1 → line 3 col 2.
        let span = Span {
            start: Position {
                offset: 0,
                line: 1,
                column: 1,
            },
            end: Position {
                offset: 23,
                line: 3,
                column: 2,
            },
        };

        let snippet = error::render_span_snippet(source, span)
            .expect("render_span_snippet should return Some for a real multi-line span");

        // All three lines must appear.
        assert!(
            snippet.contains("let x = ["),
            "snippet must contain first line, got:\n{snippet}"
        );
        assert!(
            snippet.contains("missing_key"),
            "snippet must contain middle line, got:\n{snippet}"
        );
        assert!(
            snippet.contains(']'),
            "snippet must contain last line, got:\n{snippet}"
        );

        // Caret underline must be present.
        assert!(
            snippet.contains('^'),
            "snippet must contain caret, got:\n{snippet}"
        );

        // The old "..." truncation marker must NOT appear.
        assert!(
            !snippet.contains("..."),
            "snippet must NOT contain '...' truncation; all lines should be shown, got:\n{snippet}"
        );
    }
}
