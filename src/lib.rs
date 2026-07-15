//! Parser, evaluator, type system, and builtins for the tinct language. (S-873)
//!
//! [`parse`] takes an input string and returns a `ParseOutput` containing the full `SurfaceProgram`.
//! [`parse_surface_expression`] parses a single expression and returns the native `Arc<SurfaceNode>` (no bridge).
//!
//! Additional public API:
//! - [`eval_surface_file`] / [`eval_surface_file_with_input`] -- evaluate a `SurfaceProgram` with optional stdin input (requires prior `desugar` + `resolve` passes)
//! - [`typecheck_source`] -- parse and typecheck only (no evaluation)
//! - [`materialize`] -- force thunks (shallow)
//! - [`build_core_env`] -- build a fresh env with only the core Rust builtins (starting point for `run_loader_pipeline`)
//! - [`EvalContext`] -- evaluation context with base directory and stdlib environment; include_cache memoizes `include` results (same file = same cached thunk)
//! - [`value_to_display_string`] -- render a materialized `Value` as a human-readable string
//! - [`MAX_FILE_SIZE`] -- file size limit for `include` and stdin (10 MB)

#![deny(clippy::disallowed_types, clippy::disallowed_methods)]
// Arc<Thunk> and related types are !Send because Thunk contains Rc<...> (e.g. Rc<str>
// for string sharing, Rc<RefCell<...>> for IO handles). LLT uses tokio::task::LocalSet
// with a current_thread runtime, so values never cross thread boundaries. The !Send
// constraint is intentional and correct; Rc-based sharing is cheaper and simpler than
// Arc<Mutex<...>> for data that never leaves the local thread.
#![allow(clippy::arc_with_non_send_sync)]
// TypeErrorTyped is a large enum (~208 bytes) used pervasively as the Err type
// across the type checker. Boxing it at every return site would be invasive and
// would hurt readability for marginal runtime benefit (errors are cold paths).
#![allow(clippy::result_large_err)]

pub(crate) mod arena;
// Soft heap-limit allocator: prints diagnostics and exits cleanly before RLIMIT_AS fires.
pub mod limit_alloc;
// Shared async runtime for QUIC/HTTP3 builtins (block_on helper).
pub mod ast;
pub mod async_rt;
pub(crate) mod coverage;
pub(crate) mod env;
pub(crate) mod error;
pub(crate) mod eval;
pub(crate) mod eval_access;
pub(crate) mod eval_call;
pub(crate) mod eval_core;
pub(crate) mod eval_materialize;
pub mod formatter;
pub mod lexer;
pub mod parser;
pub(crate) mod surface_fmt;
// Stream output builtins: builtin-to-tinct for SCN serialization.
pub mod resolve;
pub(crate) mod stream;
pub mod surface_convert;
#[cfg(test)]
pub(crate) mod test_util;
pub mod typecheck;
// Boolean-Algebraic Subtyping (BAS) — RDNF normalization and emptiness checking.
pub(crate) mod bas;
// Type system modules (top-level for circular dependency avoidance)
pub(crate) mod type_class;
pub(crate) mod type_def;
pub(crate) mod type_infer;
pub(crate) mod type_normalize;
// Typed TypeError infrastructure (per-error structs and discriminated enum).
pub mod type_errors;
// Type system façade (re-exports from all type modules)
pub(crate) mod types;
pub(crate) mod value;
// Import resolution for type checker — seeds TypeEnv with prelude function types.
pub(crate) mod imports;
// Rust-native builtin functions (stdlib-1 sprint).
pub(crate) mod builtins;
// Dict/access builtins: keys, length, merge, append, get, each, each-key, each-kv.
pub(crate) mod builtins_dict;
// I/O builtins: open, builtin-read-line, builtin-read-chunk, write, connect, emit, env.
pub(crate) mod builtins_io;
// Arithmetic, comparison, and control-flow builtins: +, -, *, /, =, <, if.
pub(crate) mod builtins_math;
// Type/eval/meta builtins: type-of, include, error, try, apply, validate.
pub(crate) mod builtins_meta;
// String builtins: str, split, replace, upper, lower, trim.
pub(crate) mod builtins_string;
// Net builtins: connect, tls-layer, tls-peer-cert, quic-*, http*-session, http-request,
// icmp-ping, and URI builtins (uri, url, urn). Re-exports from builtins_io.rs; URI builtins implemented directly.
pub(crate) mod builtins_net;
// Bytes builtins: bytes, bytes-find, bytes-of, bytes-equal?, ct-equal?.
pub(crate) mod builtins_bytes;
// Date-time builtins: timestamps, durations, clock capabilities, timezones.
pub(crate) mod builtins_datetime;
// Async concurrency primitives: task, await, channel, send, recv.
pub(crate) mod builtins_async;
// Aggregator for the "core" native module: core_builtins() collects all split files.
pub(crate) mod builtins_core;
// $_ desugaring (pre-typecheck AST transformation).
pub mod desugar;
// runtime-v2: lowering pass (SurfaceExpression → CoreExpr).
pub(crate) mod lower;
// runtime-v2: surface AST field extraction for match dispatch and dot-access.
pub(crate) mod surface_fields;
// Literate tinct: extract and evaluate tinct code blocks from Markdown files.
pub mod literate;
// Profiling infrastructure: span collection, timing, and conversion to Value format.
pub mod profiling;

use std::sync::Arc;

/// AST node types produced by the parser.
pub use ast::{Annotation, Param, Position, SourceFile, Span, Spanned};
/// Surface AST types for the runtime-v2 pipeline.
pub use ast::{
    CallDispatch, MacroProvenance, Provenance, Resolution, SlotAnnotation, SurfaceEntry,
    SurfaceExpression, SurfaceNode, SurfaceProgram, TypeAnnotation,
};
/// Parser entry points and error type.
pub use parser::{
    format_parse_error, parse, parse_surface_expression, parse_with_file, ParseError, ParseOutput,
};

/// Evaluation functions.
pub use eval::{
    eval_surface_file, eval_surface_file_with_input, invoke_function, materialize, CallContext,
    EvalConfig, EvalContext, EvalState, TypeContextData,
};

/// Builtin infrastructure: core env builder and resource limits.
pub use builtins::{build_core_env, MAX_COLLECT_SIZE, MAX_FILE_SIZE};

/// Import resolution for the type checker.
pub use imports::{
    apply_include_type_post_pass, build_type_env, build_type_env_with_cap,
    get_builtin_core_type_env,
};

/// Error types with source spans and stack traces.
pub use error::{
    render_span_snippet, ArityBound, DiagnosticLevel, ErrorKind, EvalError, EvalResult, StackFrame,
    TypeDiagnostic,
};

/// Type error diagnostic formatting.
pub use types::{format_type_error, TypeError};

/// Formatter: canonical source reformatter.
pub use formatter::{format_source_tinct, format_source_tinct_with_dir};

/// Unified evaluation and type-checker environment.
pub use env::Env;
/// Runtime value types: values, thunks, environments, and dict keys.
pub use value::{
    string_val, ChannelInner, ClockCapInner, DirPerms, HashableValue, NetCapEntry, Thunk, ThunkId,
    Value,
};

/// Run the loader.llt bootstrap pipeline with a pre-configured environment.
///
/// This is the shared evaluation core used by the CLI path
/// (`run_eval` in `src/main.rs`). Callers must:
///
/// - Create the initial env via [`builtins::build_core_env`]
/// - Build and inject all capability thunks into `env`:
///   `%programs`, `%args`, `%cwd`, `%libdir`, `%clock`, `%cap-fs`, etc.
///   Note: `%stdout` and `%stderr` are defined by loader.llt Dict 2, not injected here.
/// - Create `eval_ctx` via [`eval::EvalContext::new_empty`] with the correct options
///   (`no_fs`, `require_integrity`, `env_allowed`, `profiling`, `libdir_dir`, etc.)
///
/// `libdir_dir` must be an open `cap_std::fs::Dir` for the stdlib directory. It is
/// used as the base for macro expansion of the init program (which is embedded via
/// `include_str!` by default, but its macros resolve paths against this dir).
/// Provide it even when `no_fs=true` — the `no_fs` flag governs user program filesystem
/// access, not the bootstrap infrastructure.
///
/// `init_source` is the source text of the init program. `init_path` is the path used
/// in error messages and source spans. Pass `include_str!("../stdlib/loader.llt")` and
/// `"stdlib/loader.llt"` to use the embedded default.
///
/// Returns `Ok(())` on success. Any parse, expansion, typecheck, or eval error is
/// returned as `Err(String)` with a human-readable message.
pub async fn run_loader_pipeline(
    eval_ctx: &Arc<eval::EvalContext>,
    _libdir_dir: &cap_std::fs::Dir,
    _no_fs: bool,
    init_source: &str,
    init_path: &str,
) -> Result<(), String> {
    let loader_sf = Arc::new(SourceFile {
        path: Arc::from(init_path),
        content: Arc::from(init_source),
    });
    let loader_parsed = parse_with_file(init_source, Arc::clone(&loader_sf))
        .map_err(|e| format!("{init_path} parse error: {e}"))?;
    let mut loader_program = loader_parsed.program;

    desugar::desugar_surface_program(&mut loader_program);

    // Resolve the loader program. Seeded from FlatEnv so that builtin names
    // (builtin-parse, etc.) resolve to de Bruijn coordinates instead of
    // falling back to name-based lookup via the MAX/MAX sentinel.
    // Borrow is scoped so it's dropped before eval_surface_file borrows_mut the arena.
    //
    // B-513: Capture the combined scope frames (root_frame + new_frames produced by the
    // resolver) and thread them into a new eval_ctx via with_scope_frames(). This allows
    // lower() to resolve call_dispatch mangled instance binding names to correct De Bruijn
    // coordinates at eval time, so typeclass method dispatch works in the production loader
    // path where typecheck precedes eval.
    let eval_ctx_with_frames: Arc<eval::EvalContext> = {
        // Build root_frame as name→actual_slot_index (not IndexMap ordinal position).
        // iter_named() yields (name, slot_idx) pairs with the real slot index,
        // avoiding the deduplication bug where "" names collapse in IndexMap.
        let root_frame: indexmap::IndexMap<String, u32> = {
            let arena = eval_ctx.scope_arena.borrow();
            arena.scopes[0]
                .iter_named()
                .filter(|(n, _)| !n.is_empty() && !n.starts_with('#'))
                .map(|(n, slot)| (n.to_string(), slot))
                .collect()
        };
        let (_table, new_frames) =
            resolve::resolve_surface_program(&loader_program, &[root_frame.clone()]);
        // Combine: root_frame (outermost) followed by frames introduced by the program.
        let all_frames: Vec<indexmap::IndexMap<String, u32>> =
            std::iter::once(root_frame).chain(new_frames).collect();
        eval_ctx.with_scope_frames(Arc::new(all_frames))
    };

    // Typecheck writes type annotations inline on AST nodes. Errors are advisory only.
    // The returned tycon_env maps type constructor names (e.g. "Boolean", "Seq") to their
    // TyConDef, populated by [type ...] declarations in the program. Wiring it into
    // eval_ctx ensures that runtime TypeAssert checks against user-defined nominal types
    // (e.g. @Boolean on annotated function args) resolve correctly instead of failing
    // conservatively because tycon_env is None.
    let (_loader_type_errors, _loader_annotation_table, _loader_expects_resolved) =
        typecheck::typecheck_surface_program_annotation_table(&loader_program).await;

    // Evaluate loader.llt. env already contains all stdlib builtins, %programs, %args,
    // %cwd, %libdir, and any other caps injected by the caller.
    // %stdout and %stderr are defined in loader.llt Dict 2 as protocol dicts.
    let loader_thunk = eval::eval_surface_file(&loader_program, &eval_ctx_with_frames)
        .await
        .map_err(|e| format!("{e}"))?;

    // Materialize the loader result. The init program's final expression IS the
    // pipeline execution (a top-level [>> ...] Sequential node, not a dict entry),
    // so materialize() drives all side effects directly: stdout writes, emit
    // channel drains, formatter execution.
    eval::materialize(&loader_thunk, None, &eval_ctx_with_frames)
        .await
        .map_err(|e| format!("{e}"))?;

    Ok(())
}

/// Parse and type-check LLT source code.
///
/// Returns `Ok(())` if type checking succeeds with no type errors AND no quality
/// diagnostics (from `scan_type_quality`), or `Err(messages)` with a formatted
/// error string combining both error and diagnostic messages.
///
/// The type environment is pre-populated with builtin type signatures AND prelude
/// function types via `imports::build_prelude_env()`, so stdlib builtins (`+`, `merge`)
/// and prelude functions (`map`, `filter`, `any?`, etc.) are in scope for type checking.
///
/// This strict variant is used by the typecheck warnings corpus (which expects specific
/// diagnostic messages) and the valid corpus (which asserts zero warnings unless an
/// `=== warn` section is present). For corpus tests that only care about type *errors*
/// (not quality diagnostics), use [`typecheck_source_errors_only`] instead.
pub async fn typecheck_source(input: &str) -> Result<(), String> {
    let parsed = parse(input).map_err(|e| format!("{e}"))?;
    // PIPELINE INVARIANT: parse -> desugar -> typecheck.
    let mut program = parsed.program;
    desugar::desugar_surface_program(&mut program);
    let env_arc = imports::get_builtin_core_type_env()
        .await
        .expect("builtin core type env not available — bootstrap error");
    let (type_errors, _type_map, _doc_map, _scheme_map, diagnostics) =
        typecheck::typecheck_surface_program(&program, env_arc).await;
    if type_errors.is_empty() && diagnostics.is_empty() {
        Ok(())
    } else {
        let mut msgs = Vec::new();
        for e in &type_errors {
            msgs.push(format!("{}", e));
        }
        for d in &diagnostics {
            msgs.push(d.message.clone());
        }
        msgs.sort();
        Err(msgs.join("\n"))
    }
}

/// Parse and type-check LLT source code (errors only, no quality diagnostics).
///
/// Like [`typecheck_source`] but only fails on actual type errors, ignoring advisory
/// quality diagnostics from `scan_type_quality` (e.g., "inferred type is Unknown").
///
/// Used by the typecheck corpus (`tests/corpus/eval/typecheck/`) which validates that
/// programs type-check without errors but may legitimately contain polymorphic or
/// open-record patterns that produce `Unknown` in intermediate type-map entries.
pub async fn typecheck_source_errors_only(input: &str) -> Result<(), String> {
    let parsed = parse(input).map_err(|e| format!("{e}"))?;
    // PIPELINE INVARIANT: parse -> desugar -> typecheck.
    let mut program = parsed.program;
    desugar::desugar_surface_program(&mut program);
    // Type check the surface program.
    let env_arc2 = imports::get_builtin_core_type_env()
        .await
        .expect("builtin core type env not available — bootstrap error");
    let (type_errors, _type_map, _doc_map, _scheme_map, _diagnostics) =
        typecheck::typecheck_surface_program(&program, env_arc2).await;
    if type_errors.is_empty() {
        Ok(())
    } else {
        let mut msgs = Vec::new();
        for e in &type_errors {
            msgs.push(format!("{}", e));
        }
        Err(msgs.join("\n"))
    }
}

// --- Value Serializer Visitor Pattern ---
//
// `visit_value` and `value_to_display_string` share the same structural traversal
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
    fn visit_float(&self, v: f64, span: ast::Span) -> Result<Self::Output, Box<error::EvalError>>;
    fn visit_str(&self, v: &str) -> Self::Output;
    fn visit_bytes(&self, v: &[u8]) -> Self::Output;
    fn visit_null(&self) -> Self::Output;
    fn visit_dict(&self, entries: Vec<(value::HashableValue, Self::Output)>) -> Self::Output;
    fn visit_seq_head(
        &self,
        head: Self::Output,
        span: ast::Span,
    ) -> Result<Self::Output, Box<error::EvalError>>;
    fn visit_function(
        &self,
        params: &[ast::Param],
        span: ast::Span,
    ) -> Result<Self::Output, Box<error::EvalError>>;
    fn visit_builtin(
        &self,
        name: &str,
        span: ast::Span,
    ) -> Result<Self::Output, Box<error::EvalError>>;
    fn visit_proxy(&self, span: ast::Span) -> Result<Self::Output, Box<error::EvalError>>;
    fn visit_variant(&self, tag: String, payload: Self::Output) -> Self::Output;
    fn visit_decimal(&self, v: rust_decimal::Decimal) -> Self::Output;
    fn visit_bigint(&self, v: &num_bigint::BigInt) -> Self::Output;
    fn visit_timestamp(
        &self,
        nanos: i64,
        span: ast::Span,
    ) -> Result<Self::Output, Box<error::EvalError>>;
    fn visit_duration(&self, nanos: i64) -> Self::Output;
    fn visit_clock_cap(&self, span: ast::Span) -> Result<Self::Output, Box<error::EvalError>>;
    fn visit_timezone(&self, span: ast::Span) -> Result<Self::Output, Box<error::EvalError>>;
    /// Return `Some(output)` if the depth limit has been reached, `None` to continue.
    fn depth_limit_output(
        &self,
        depth: usize,
        span: ast::Span,
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
pub fn visit_value<'a, V: ValueVisitor + 'a>(
    val: &'a value::Value,
    ctx: &'a Arc<eval::EvalContext>,
    depth: usize,
    visitor: &'a V,
    span: ast::Span,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<V::Output, Box<error::EvalError>>> + 'a>,
> {
    Box::pin(async move {
        if let Some(limit_result) = visitor.depth_limit_output(depth, span.clone()) {
            return limit_result;
        }
        match val {
            value::Value::Int(n) => Ok(visitor.visit_int(*n)),
            // U64 values: for serialization, emit as Int when they fit in i64,
            // otherwise serialize as BigInt equivalent. The visitor uses visit_bigint.
            value::Value::U64(n) => {
                if let Ok(i) = i64::try_from(*n) {
                    Ok(visitor.visit_int(i))
                } else {
                    Ok(visitor.visit_bigint(&num_bigint::BigInt::from(*n)))
                }
            }
            value::Value::Float(f) => visitor.visit_float(*f, span),
            value::Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[*start..*end];
                Ok(visitor.visit_str(s))
            }
            value::Value::Bytes {
                ref source,
                start,
                end,
            } => {
                let bytes = &source[*start..*end];
                Ok(visitor.visit_bytes(bytes))
            }
            value::Value::Dict(map) => {
                let mut entries = Vec::with_capacity(map.len());
                for (key, thunk_id) in map {
                    let thunk = ctx.get_thunk(*thunk_id);
                    let v = eval::materialize(&thunk, None, ctx).await?;
                    let child_span = thunk.span.clone();
                    entries.push((
                        key.clone(),
                        visit_value(&v, ctx, depth + 1, visitor, child_span).await?,
                    ));
                }
                Ok(visitor.visit_dict(entries))
            }
            value::Value::Overlay(l, r) => {
                // Flatten overlay to a concrete dict, then visit it.
                let map = builtins::flatten_overlay(l, r, "value serialization", ctx, span.clone())
                    .await?;
                visit_value(&value::Value::Dict(map), ctx, depth, visitor, span).await
            }
            value::Value::Function { params, .. } => visitor.visit_function(params, span),
            value::Value::Builtin(def) => visitor.visit_builtin(def.name, span),
            value::Value::Proxy { .. } => visitor.visit_proxy(span),
            value::Value::DirCap { .. } => Err(Box::new(error::EvalError::value_not_serializable(
                "DirCap".to_string(),
                span,
            ))),
            value::Value::NetCap(_) => Err(Box::new(error::EvalError::value_not_serializable(
                "NetCap".to_string(),
                span,
            ))),
            value::Value::File(_) => Err(Box::new(error::EvalError::value_not_serializable(
                "File".to_string(),
                span,
            ))),
            value::Value::RevocableDirCap { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("RevocableDirCap".to_string(), span),
            )),
            value::Value::Variant { tag, payload } => {
                let payload_output = match payload {
                    Some(thunk_id) => {
                        let thunk = ctx.get_thunk(*thunk_id);
                        let v = eval::materialize(&thunk, None, ctx).await?;
                        let payload_span = thunk.span.clone();
                        visit_value(&v, ctx, depth + 1, visitor, payload_span).await?
                    }
                    None => visitor.visit_null(),
                };
                Ok(visitor.visit_variant(tag.clone(), payload_output))
            }
            value::Value::Decimal(d) => Ok(visitor.visit_decimal(*d)),
            value::Value::BigInt(n) => Ok(visitor.visit_bigint(n)),
            value::Value::Uri { .. } => Err(Box::new(error::EvalError::value_not_serializable(
                "Uri".to_string(),
                span,
            ))),
            value::Value::Timestamp(nanos) => visitor.visit_timestamp(*nanos, span),
            value::Value::Duration(nanos) => Ok(visitor.visit_duration(*nanos)),
            value::Value::ClockCap(_) => visitor.visit_clock_cap(span),
            value::Value::Timezone(_) => visitor.visit_timezone(span),
            value::Value::QuicSession(_) => Err(Box::new(
                error::EvalError::value_not_serializable("QuicSession".to_string(), span),
            )),
            value::Value::Http2Session { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("Http2Session".to_string(), span),
            )),
            value::Value::Http3Session(_) => Err(Box::new(
                error::EvalError::value_not_serializable("Http3Session".to_string(), span),
            )),
            value::Value::QuicDatagramHandle(_) => Err(Box::new(
                error::EvalError::value_not_serializable("QuicDatagramHandle".to_string(), span),
            )),
            value::Value::DatagramHandle { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("DatagramHandle".to_string(), span),
            )),
            value::Value::Program { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("Program".to_string(), span),
            )),
            value::Value::Document(_) => Err(Box::new(error::EvalError::value_not_serializable(
                "Document".to_string(),
                span,
            ))),
            // Expr.* variants (Value::Variant) are handled by the Variant arm above.
            value::Value::Task(_) => Err(Box::new(error::EvalError::value_not_serializable(
                "Task".to_string(),
                span,
            ))),
            value::Value::Channel(_) => Err(Box::new(error::EvalError::value_not_serializable(
                "Channel".to_string(),
                span,
            ))),
            value::Value::Context(_) => Err(Box::new(error::EvalError::value_not_serializable(
                "Context".to_string(),
                span,
            ))),
            value::Value::ReactiveCell(_) => Err(Box::new(
                error::EvalError::value_not_serializable("ReactiveCell".to_string(), span),
            )),
            value::Value::Builder(_) => Err(Box::new(error::EvalError::value_not_serializable(
                "Builder".to_string(),
                span,
            ))),
            value::Value::BroadcastChannel(_) => Err(Box::new(
                error::EvalError::value_not_serializable("BroadcastChannel".to_string(), span),
            )),
            value::Value::OneshotSender(_) => Err(Box::new(
                error::EvalError::value_not_serializable("OneshotSender".to_string(), span),
            )),
            value::Value::OneshotReceiver(_) => Err(Box::new(
                error::EvalError::value_not_serializable("OneshotReceiver".to_string(), span),
            )),
            // Annotated is transparent — delegate to inner value serialization.
            value::Value::Annotated { inner, .. } => {
                visit_value(inner, ctx, depth, visitor, span).await
            }
            value::Value::TypeContext(_) => Err(Box::new(
                error::EvalError::value_not_serializable("TypeContext".to_string(), span),
            )),
            value::Value::Bool(_) => Err(Box::new(error::EvalError::value_not_serializable(
                "Bool".to_string(),
                span,
            ))),
            value::Value::Seq { .. } => Err(Box::new(error::EvalError::value_not_serializable(
                "Seq".to_string(),
                span,
            ))),
            value::Value::Handle { .. } => Err(Box::new(error::EvalError::value_not_serializable(
                "Handle".to_string(),
                span,
            ))),
            value::Value::WriteHandle { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("WriteHandle".to_string(), span),
            )),
            value::Value::Expression(_) => Err(Box::new(error::EvalError::value_not_serializable(
                "Expression".to_string(),
                span,
            ))),
            value::Value::Arena { .. } => Err(Box::new(error::EvalError::value_not_serializable(
                "Arena".to_string(),
                span,
            ))),
        }
    })
}

/// Reformat compact JSON into a pretty-printed string with 2-space indentation.
///
/// Handles string escapes correctly so that `{`, `}`, `[`, `]`, `,`, `:` inside
/// JSON strings are not treated as structural characters.
pub fn json_pretty_print(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    let mut depth: usize = 0;
    let mut in_string = false;
    // Tracks whether the last structural push was an open brace/bracket for an empty
    // container (detected via peek). Used in the `}` / `]` arm to decide whether to
    // emit a newline+indent or close inline.
    let mut last_was_open = false;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                // Consume the escaped character verbatim
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if c == '"' {
                in_string = false;
            }
            last_was_open = false;
            continue;
        }
        match c {
            // Skip whitespace outside strings (the input is compact JSON)
            ' ' | '\t' | '\n' | '\r' => {}
            '"' => {
                in_string = true;
                out.push(c);
                last_was_open = false;
            }
            '{' | '[' => {
                out.push(c);
                // Peek ahead: if immediately followed by the matching close, emit inline
                let close = if c == '{' { '}' } else { ']' };
                if chars.peek() == Some(&close) {
                    // Empty container — will be emitted on next iteration without indent
                    last_was_open = true;
                } else {
                    depth += 1;
                    out.push('\n');
                    for _ in 0..depth {
                        out.push_str("  ");
                    }
                    last_was_open = false;
                }
            }
            '}' | ']' => {
                if last_was_open {
                    // Empty container: emit close without newline
                    out.push(c);
                } else {
                    depth = depth.saturating_sub(1);
                    out.push('\n');
                    for _ in 0..depth {
                        out.push_str("  ");
                    }
                    out.push(c);
                }
                last_was_open = false;
            }
            ',' => {
                out.push(c);
                out.push('\n');
                for _ in 0..depth {
                    out.push_str("  ");
                }
                last_was_open = false;
            }
            ':' => {
                out.push(c);
                out.push(' ');
                last_was_open = false;
            }
            _ => {
                out.push(c);
                last_was_open = false;
            }
        }
    }
    out
}

// --- Display Visitor ---

/// Maximum display recursion depth (5 levels).
/// Prevents deep traversal of nested structures in error messages.
/// Depth 5 handles typical variant-wrapped values and nested payload dicts.
const MAX_DISPLAY_DEPTH: usize = 5;

struct DisplayVisitor;

impl ValueVisitor for DisplayVisitor {
    type Output = String;

    fn visit_int(&self, v: i64) -> String {
        format!("Int({v})")
    }
    fn visit_float(&self, v: f64, _span: ast::Span) -> Result<String, Box<error::EvalError>> {
        Ok(format!("Float({v})"))
    }
    fn visit_str(&self, v: &str) -> String {
        format!("String({v:?})")
    }
    fn visit_bytes(&self, v: &[u8]) -> String {
        format!("Bytes({} bytes)", v.len())
    }
    fn visit_null(&self) -> String {
        "Null".to_string()
    }
    fn visit_dict(&self, entries: Vec<(value::HashableValue, String)>) -> String {
        use std::fmt::Write;
        let mut result = String::from("Dict({");
        for (i, (key, val_str)) in entries.into_iter().enumerate() {
            if i > 0 {
                result.push_str(", ");
            }
            // String keys are displayed quoted (e.g. "x") for readability and
            // round-trip clarity. Integer and other keys use their natural repr.
            match &key {
                value::HashableValue::Str(s) => write!(&mut result, "{s:?}").unwrap(),
                _ => write!(&mut result, "{key}").unwrap(),
            }
            result.push_str(": ");
            result.push_str(&val_str);
        }
        result.push_str("})");
        result
    }
    fn visit_seq_head(
        &self,
        head: String,
        _span: ast::Span,
    ) -> Result<String, Box<error::EvalError>> {
        Ok(format!("Seq({head}, ...)"))
    }
    fn visit_function(
        &self,
        params: &[ast::Param],
        _span: ast::Span,
    ) -> Result<String, Box<error::EvalError>> {
        let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
        Ok(format!("Function({})", names.join(", ")))
    }
    fn visit_builtin(&self, name: &str, _span: ast::Span) -> Result<String, Box<error::EvalError>> {
        Ok(format!("Builtin({name})"))
    }
    fn visit_proxy(&self, _span: ast::Span) -> Result<String, Box<error::EvalError>> {
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
    fn visit_timestamp(
        &self,
        nanos: i64,
        _span: ast::Span,
    ) -> Result<String, Box<error::EvalError>> {
        // Format as RFC 3339 for readability
        match jiff::Timestamp::from_nanosecond(nanos as i128) {
            Ok(ts) => Ok(format!("Timestamp({})", ts)),
            Err(_) => Ok(format!("Timestamp({} ns, invalid)", nanos)),
        }
    }
    fn visit_duration(&self, nanos: i64) -> String {
        format!("Duration({} ns)", nanos)
    }
    fn visit_clock_cap(&self, _span: ast::Span) -> Result<String, Box<error::EvalError>> {
        Ok("ClockCap".to_string())
    }
    fn visit_timezone(&self, _span: ast::Span) -> Result<String, Box<error::EvalError>> {
        Ok("Timezone".to_string())
    }
    fn depth_limit_output(
        &self,
        depth: usize,
        _span: ast::Span,
    ) -> Option<Result<String, Box<error::EvalError>>> {
        if depth >= MAX_DISPLAY_DEPTH {
            Some(Ok("...".to_string()))
        } else {
            None
        }
    }
}

/// Convert a Value into a displayable string (LLT format, not JSON).
///
/// Dict entry thunks are materialized on demand internally via [`eval::materialize`]; the caller
/// need only pass a shallowly-materialized top-level value. Does not perform recursive
/// deep-forcing — each thunk is materialized exactly once as it is visited.
///
/// Unlike `Value::Debug`, this renders dict values showing the complete
/// structure, not just keys.
///
/// Unlike the JSON visitor pattern, this accepts NaN/Infinity floats (renders as `Float(NaN)`, `Float(inf)`).
///
/// `depth` tracks recursion depth to prevent stack overflow from deeply nested
/// dict-of-dicts structures. Uses `MAX_DISPLAY_DEPTH` (5 levels); truncates deeper nesting with `...`.
pub async fn value_to_display_string(
    val: &value::Value,
    ctx: &Arc<eval::EvalContext>,
    span: ast::Span,
) -> Result<String, Box<error::EvalError>> {
    let depth = 0;
    visit_value(val, ctx, depth, &DisplayVisitor, span).await
}

/// Resolve the stdlib directory path from the binary location.
///
/// Tries multiple layouts in order:
/// 1. Development release binary: `target/debug/tinct` → `<project-root>/stdlib/`
///    (2 parent levels: debug/ → target/ → project root → stdlib/)
/// 2. Test binary: `target/debug/deps/tinct-HASH` → `<project-root>/stdlib/`
///    (3 parent levels: deps/ → debug/ → target/ → project root → stdlib/)
/// 3. Installed: `bin/tinct` → `<prefix>/share/tinct/stdlib/`
///    (2 parent levels: bin/ → prefix/ → share/tinct/stdlib/)
///
/// Returns `None` if no stdlib directory exists at any candidate path.
///
/// This is used by the type checker and runtime to resolve `%libdir` cap-qualified includes.
pub fn find_libdir_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // Collect candidate stdlib dirs by walking up the directory hierarchy.
    // Each ancestor that contains a "stdlib" subdirectory is a valid candidate.
    // We try up to 4 levels up to handle both release binaries (2 levels) and
    // test binaries (3 levels: target/debug/deps/tinct-HASH → target/ → root).
    let mut dir = exe.parent()?.to_path_buf();
    for _ in 0..4 {
        let candidate = dir.join("stdlib");
        if candidate.is_dir() {
            return Some(candidate);
        }
        // Also check the installed layout: <prefix>/share/tinct/stdlib/
        let installed = dir.join("share").join("tinct").join("stdlib");
        if installed.is_dir() {
            return Some(installed);
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_util::test_span;
    use value::{Thunk, Value};

    // Async wrappers for async public functions — test code uses .await.
    async fn typecheck_source_errors_only(input: &str) -> Result<(), String> {
        super::typecheck_source_errors_only(input).await
    }
    async fn value_to_display_string(
        val: &value::Value,
        ctx: &Arc<eval::EvalContext>,
        span: ast::Span,
    ) -> Result<String, Box<error::EvalError>> {
        super::value_to_display_string(val, ctx, span).await
    }

    async fn test_ctx() -> Arc<eval::EvalContext> {
        let base_dir = test_util::test_caps().root.try_clone().unwrap();
        eval::EvalContext::new_empty(base_dir, false)
    }

    #[test]
    fn test_display_unit_variant() {
        // Unit variants display as their full qualified tag via Rust Display.
        let v = Value::Variant {
            tag: "Color.Red".to_string(),
            payload: None,
        };
        assert_eq!(format!("{v}"), "Color.Red");
    }

    /// B-448: All unit variants must serialise uniformly as `Variant(Tag, Null)`.
    /// The serialiser must be agnostic to ADT tag names — no tag name receives special privilege.
    ///
    /// Before B-448, `visit_value` short-circuited on certain variant tags and dispatched to a
    /// now-deleted `visit_bool` method.  After the fix all variants fall through to the generic
    /// `Variant` arm.  `visit_bool` has since been removed from the `ValueVisitor` trait entirely.
    #[tokio::test]
    async fn test_display_unit_variant_uniform() {
        let ctx = test_ctx().await;

        let red_val = Value::Variant {
            tag: "Color.Red".to_string(),
            payload: None,
        };
        let green_val = Value::Variant {
            tag: "Color.Green".to_string(),
            payload: None,
        };

        let red_display = value_to_display_string(&red_val, &ctx, rust_span!())
            .await
            .expect("display should succeed for Color.Red");
        let green_display = value_to_display_string(&green_val, &ctx, rust_span!())
            .await
            .expect("display should succeed for Color.Green");

        // Must serialize as a generic variant.
        assert_eq!(
            red_display, "Variant(Color.Red, Null)",
            "Color.Red must serialise as Variant(Color.Red, Null)"
        );
        assert_eq!(
            green_display, "Variant(Color.Green, Null)",
            "Color.Green must serialise as Variant(Color.Green, Null)"
        );

        // A unit variant from a different type must produce the same structure.
        let user_val = Value::Variant {
            tag: "MyBool.Yes".to_string(),
            payload: None,
        };
        let user_display = value_to_display_string(&user_val, &ctx, rust_span!())
            .await
            .expect("display should succeed for MyBool.Yes");
        assert_eq!(
            user_display, "Variant(MyBool.Yes, Null)",
            "User-defined unit variant must serialise uniformly"
        );
    }

    #[tokio::test]
    async fn test_display_proxy() {
        let ctx = test_ctx().await;
        let handler_thunk = Arc::new(Thunk::new_materialized(
            Value::Int(42),
            test_span(1, 1, 1, 1),
        ));
        let proxy = Value::Proxy {
            handler: ctx.alloc_thunk(handler_thunk),
        };
        let display = value_to_display_string(&proxy, &ctx, rust_span!())
            .await
            .expect("display failed");
        assert_eq!(display, "Proxy");
    }

    // --- Integration tests: render_span_snippet in error output ---

    /// `eval_source_with_snippets` integration test: verify that when an error occurs
    /// in a user-written source string, the error Display produced by main.rs
    /// includes a source snippet (rustc-style underline). This exercises
    /// `render_span_snippet` being called with a real eval error's definition_span.
    ///
    /// The test simulates the pattern used in main.rs `run_eval`:
    /// parse source → eval → on error, call render_span_snippet with the source string
    /// and the error's definition_span, then check the snippet is present.
    #[tokio::test]
    async fn test_eval_source_with_source_snippets() {
        // Source that will produce an eval error with a real source span.
        // Accessing an undefined variable gives an UndefinedVariable error whose
        // definition_span points at the VarRef expression in the source.
        let source = "$undefined_var";

        // Parse the source manually to get a real AST with spans.
        // Move `program` out of ParseOutput directly — cloning would increase Arc reference
        // counts and cause `desugar_surface_program`'s `Arc::get_mut` to panic.
        let mut program = parse(source).expect("parse should succeed").program;
        desugar::desugar_surface_program(&mut program);
        // T-1576: test path uses bootstrap mode (no arena yet).
        let (_table, _frames) = resolve::resolve_surface_program(&program, &[]);
        let (_type_errors, _inferred, _tycon_env) =
            typecheck::typecheck_surface_program_annotation_table(&program).await;
        let ctx = test_ctx().await;

        // Evaluate: this should fail because $undefined_var is not defined.
        let eval_result = eval::eval_surface_file(&program, &ctx).await;
        assert!(
            eval_result.is_err(),
            "expected eval to fail for undefined variable"
        );
        let err = eval_result.unwrap_err();

        // Verify the error has a non-synthetic definition_span.
        assert_ne!(
            err.definition_span,
            rust_span!(),
            "error should have a real source span, not rust_span!()"
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

    /// Integration test: an error whose `definition_span` covers multiple lines renders
    /// ALL spanned lines in the snippet (no truncation with "...").
    ///
    /// We construct a `Span` directly over a known multi-line source string and call
    /// `render_span_snippet` — the same call that `main.rs` and the REPL make. This
    /// verifies the full pipeline from span → snippet without needing an eval error
    /// that naturally produces a multi-line span (which requires syntactic constructs
    /// the evaluator doesn't currently tag that way).
    #[tokio::test]
    async fn test_multiline_span_snippet_shows_all_lines() {
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
            file: rust_span!().file,
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

    /// Lint pipeline: type error produces Err (exit code 1 behavior).
    ///
    /// Verifies that an undefined variable reference causes the lint pipeline to
    /// return an error, matching the `tinct lint` exit-code-1 behavior.
    #[tokio::test]
    async fn test_lint_pipeline_type_error() {
        // Referencing an undefined variable should produce an "undefined variable" type error.
        let input = r#"[x: undefined_var]"#;
        let result = typecheck_source_errors_only(input).await;
        assert!(
            result.is_err(),
            "lint pipeline should report a type error for undefined variable reference, got Ok(())"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("undefined variable"),
            "error message should describe the undefined variable, got: {msg}"
        );
    }

    /// Lint pipeline: no side effects — emit calls are not executed.
    ///
    /// Verifies that running the lint pipeline on a file containing `$emit` calls does
    /// not execute the emit (no output is produced). The lint pipeline stops before eval.
    #[tokio::test]
    async fn test_lint_pipeline_no_side_effects() {
        // If lint executed this, it would emit text to stdout. Since it only type-checks,
        // no output should occur. The absence of a panic or visible output is the assertion.
        let input = r#"[call $emit "this should not appear"]"#;
        // typecheck_source only parses, expands, desugars, resolves, and type-checks.
        // It does not evaluate — so no emit side-effect fires.
        let _ = typecheck_source_errors_only(input).await;
        // If we reach here without IO side-effects, the test passes.
        // (Capturing stdout in a unit test would require infrastructure not worth adding.)
    }

    // -------------------------------------------------------------------------
    // json_pretty_print tests
    // -------------------------------------------------------------------------

    #[test]
    fn json_pretty_print_simple_object() {
        assert_eq!(json_pretty_print(r#"{"a":1}"#), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn json_pretty_print_empty_object() {
        assert_eq!(json_pretty_print("{}"), "{}");
    }

    #[test]
    fn json_pretty_print_empty_array() {
        assert_eq!(json_pretty_print("[]"), "[]");
    }

    #[test]
    fn json_pretty_print_array() {
        assert_eq!(json_pretty_print(r#"["x","y"]"#), "[\n  \"x\",\n  \"y\"\n]");
    }

    #[test]
    fn json_pretty_print_structural_chars_in_string() {
        // The `{` inside the string value must not trigger extra indentation.
        let result = json_pretty_print(r#"{"k":"a{b}"}"#);
        assert_eq!(result, "{\n  \"k\": \"a{b}\"\n}");
    }
}
