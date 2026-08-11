//! Parser, evaluator, type system, and builtins for the tinct language. (S-873)
//!
//! [`parse`] takes an input string and returns a `ParseOutput` containing the full `SurfaceProgram`.
//! [`parse_surface_expression`] parses a single expression and returns the native `Arc<SurfaceNode>` (no bridge).
//!
//! Additional public API:
//! - [`eval_surface_file`] / [`eval_surface_file_with_input`] -- evaluate a `SurfaceProgram` with optional stdin input (requires prior `desugar` + `resolve` passes)
//! - [`typecheck_source`] -- parse and typecheck only (no evaluation)
//! - [`materialize`] -- force thunks (shallow)
//! - [`build_core_env`] -- build a list of core Rust builtin names (starting point for `run_loader_pipeline`)
//! - [`EvalContext`] -- evaluation context with base directory and stdlib environment; include_cache memoizes `include` results (same file = same cached thunk)
//! - [`value_to_display_string`] -- render a materialized `Value` as a human-readable string

#![deny(clippy::disallowed_types, clippy::disallowed_methods)]
// Increased recursion limit for deep Send/Sync bound evaluation through hyper/reqwest/tower
// dependency chains (reqwest::Client -> hyper_util::Client -> Pool -> PoolInner chain).
#![recursion_limit = "256"]

pub(crate) mod arena;
// Shared async runtime for QUIC/HTTP3 builtins (block_on helper).
pub mod ast;
pub mod async_rt;
pub(crate) mod coverage;
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
// TypeValue constructor tag string constants — single authoritative source for all ctor strings.
pub mod type_tags;
// Type system modules (top-level for circular dependency avoidance)
pub(crate) mod type_class;
pub(crate) mod type_def;
pub(crate) mod type_infer;
pub(crate) mod type_normalize;
// Type system façade (re-exports from all type modules)
pub(crate) mod types;
pub(crate) mod value;
// Per-document bootstrap pipeline: resolve → typecheck → eval.
pub(crate) mod pipeline;
// Import resolution for type checker — seeds TypeEnv with prelude function types.
pub(crate) mod imports;
// Rust-native builtin functions (stdlib-1 sprint).
pub(crate) mod builtins;
// Dict/access builtins: keys, length, append, get, each, each-key, each-kv.
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
// Declaration expansion (TypeAlias → constructor dict) — pre-resolve AST transformation.
pub mod expand;
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
pub use ast::{Annotation, Param, Span, Spanned};
/// Surface AST types for the runtime-v2 pipeline.
pub use ast::{
    MacroProvenance, Provenance, Resolution, SurfaceEntry, SurfaceExpression, SurfaceNode,
    SurfaceProgram, TypeAnnotation, VarAddr,
};
/// Parser entry points.
pub use parser::{parse, parse_surface_expression, ParseOutput};
/// Per-document pipeline.
pub use pipeline::{process_pipeline_document, PipelineState};

/// Evaluation functions.
pub use eval::{
    eval_surface_file, eval_surface_file_with_input, invoke_function, materialize, CallContext,
    EvalConfig, EvalContext, TypeContextData,
};

pub use builtins::build_core_env;

/// Bootstrap type environment from builtin_core.llt.
pub use imports::{
    build_builtin_core_envs, get_builtin_core_tycon_env, get_builtin_core_type_stage_scope,
};

/// Error types with source spans and stack traces.
pub use error::{
    format_diagnostic, render_span_snippet, ArityBound, Diagnostic, DiagnosticLevel, SpanLabel,
};

/// TypeValue: the Arc<Value>-based type representation used throughout the type checker.
pub use type_class::TypeValue;

/// TypeValue construction helpers exposed for use in crate embedders (e.g., main.rs CLI
/// injected type env). These are the canonical single-path constructors — callers must
/// not duplicate construction logic locally (Axiom 2).
pub use type_infer::{make_typevalue_nominal, make_typevalue_open_dict, make_typevalue_unknown};
pub use value::{
    TYPE_ID_CLOCKCAP, TYPE_ID_DICT, TYPE_ID_DIRCAP, TYPE_ID_FILE, TYPE_ID_INT, TYPE_ID_NETCAP,
    TYPE_ID_PROGRAM_ITEM, TYPE_ID_PROXY,
};

/// Formatter: canonical source reformatter.
pub use formatter::format_source_tinct;

/// Runtime value types: values, thunks, environments, and dict keys.
pub use value::{
    string_val, ChannelInner, ClockCapInner, DirPerms, EvalFrame, HashableValue, NetCapEntry,
    Thunk, Value,
};

/// Run the loader.llt bootstrap pipeline with a pre-configured environment.
///
/// This is the shared evaluation core used by the CLI path
/// (`run_eval` in `src/main.rs`). Callers must:
///
/// - Create `eval_ctx` via [`eval::EvalContext::new_with_options`] with the correct options
///   (`require_integrity`, `env_allowed`, `profiling`, `libdir_dir`, etc.)
///
/// `libdir_dir` must be an open `cap_std::fs::Dir` for the stdlib directory. It is
/// used as the base for macro expansion of the init program (which is embedded via
/// `include_str!` by default, but its macros resolve paths against this dir).
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
    init_source: &str,
    init_path: &str,
    _injected_cap_types: Option<std::collections::HashMap<String, crate::type_infer::TypeValue>>,
) -> crate::error::DiagnosticResult<()> {
    // Start from builtins — consistent slots, single context.
    let mut state = crate::pipeline::PipelineState::from_builtins(eval_ctx);

    let mut diagnostics: Vec<crate::error::Diagnostic> = Vec::new();

    // Process builtin_core.llt inline — same pipeline, same context.
    let core_source = include_str!("../stdlib/builtin_core.llt");
    let core_sf: Arc<str> = Arc::from("stdlib/builtin_core.llt");
    let core_parsed = match parse(core_source, core_sf) {
        Ok(p) => p,
        Err(e) => {
            return crate::error::DiagnosticResult::err(crate::error::Diagnostic::error(
                "parse-error",
                format!("builtin_core.llt: {e}"),
                crate::rust_span!(),
            ))
            .with_diagnostics(diagnostics);
        }
    };
    let core_program = desugar::desugar_program_full(&core_parsed.program);
    let core_program = expand::expand_program(&core_program);
    for doc in &core_program.documents {
        if let Err(e) = crate::pipeline::process_pipeline_document(doc, &mut state, eval_ctx).await
        {
            return crate::error::DiagnosticResult::err(*e).with_diagnostics(diagnostics);
        }
    }

    // Initialize TypeContext from the pipeline state accumulated during builtin_core
    // processing. The loader calls builtin-get-type-context which needs this.
    eval_ctx.init_type_context(crate::eval::TypeContextData {
        tycon_env: state.tycon_env.clone(),
        type_stage_scope: state.type_stage_data.scope.clone(),
        type_stage_fns: state.type_stage_data.fns.clone(),
        type_stage_type_vars: state.type_stage_data.type_vars.clone(),
        type_diagnostics: Vec::new(),
        type_display_names: state.type_display_names.clone(),
        type_name_to_id: state.type_name_to_id.clone(),
    });

    // Process init program documents.
    let loader_sf: Arc<str> = Arc::from(init_path);
    let loader_parsed = match parse(init_source, Arc::clone(&loader_sf)) {
        Ok(p) => p,
        Err(e) => {
            return crate::error::DiagnosticResult::err(crate::error::Diagnostic::error(
                "parse-error",
                format!("{init_path}: {e}"),
                crate::rust_span!(),
            ))
            .with_diagnostics(diagnostics);
        }
    };
    diagnostics.extend(loader_parsed.diagnostics.into_iter().map(|d| d.into()));
    let loader_program = desugar::desugar_program_full(&loader_parsed.program);
    let loader_program = expand::expand_program(&loader_program);
    for doc in &loader_program.documents {
        if let Err(e) = crate::pipeline::process_pipeline_document(doc, &mut state, eval_ctx).await
        {
            return crate::error::DiagnosticResult::err(*e).with_diagnostics(diagnostics);
        }
    }

    crate::error::DiagnosticResult::ok(()).with_diagnostics(diagnostics)
}

/// Parse and type-check LLT source code.
///
/// Returns `Ok(())` if type checking succeeds with no type errors AND no quality
/// diagnostics (emitted inline by the CEK type checker), or `Err(messages)` with a formatted
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
    let file: Arc<str> = Arc::from("<typecheck>");
    let parsed = parse(input, file).map_err(|e| format!("{e}"))?;
    let program = desugar::desugar_program_full(&parsed.program);
    let program = expand::expand_program(&program);
    let ctx = eval::EvalContext::new_empty();
    let mut state = crate::pipeline::PipelineState::from_builtins(&ctx);
    // Process builtin_core.llt inline.
    let core_parsed = parse(
        include_str!("../stdlib/builtin_core.llt"),
        Arc::from("stdlib/builtin_core.llt"),
    )
    .map_err(|e| format!("builtin_core.llt parse error: {e}"))?;
    let core_program = desugar::desugar_program_full(&core_parsed.program);
    let core_program = expand::expand_program(&core_program);
    for doc in &core_program.documents {
        crate::pipeline::process_pipeline_document(doc, &mut state, &ctx)
            .await
            .map_err(|e| format!("builtin_core.llt: {e}"))?;
    }
    let mut diagnostics = Vec::new();
    for doc in &program.documents {
        let mut doc_diags = crate::pipeline::typecheck_pipeline_document(doc, &mut state).await;
        diagnostics.append(&mut doc_diags);
    }
    // Include state-level diagnostics (e.g. unknown-type warnings from unification).
    diagnostics.append(&mut state.infer_state.diagnostics);
    if diagnostics.is_empty() {
        Ok(())
    } else {
        let mut msgs: Vec<String> = diagnostics.iter().map(|d| d.message.clone()).collect();
        msgs.sort();
        Err(msgs.join("\n"))
    }
}

/// Parse and type-check LLT source code (errors only, no quality diagnostics).
///
/// Like [`typecheck_source`] but only fails on actual type errors, ignoring advisory
/// quality diagnostics emitted inline by the CEK type checker.
///
/// Used by the typecheck corpus (`tests/corpus/eval/typecheck/`) which validates that
/// programs type-check without errors but may legitimately contain polymorphic or
/// open-record patterns that produce `Unknown` in intermediate type-map entries.
pub async fn typecheck_source_errors_only(input: &str) -> Result<(), String> {
    let file: Arc<str> = Arc::from("<typecheck>");
    let parsed = parse(input, file).map_err(|e| format!("{e}"))?;
    let program = desugar::desugar_program_full(&parsed.program);
    let program = expand::expand_program(&program);
    let ctx = eval::EvalContext::new_empty();
    let mut state = crate::pipeline::PipelineState::from_builtins(&ctx);
    // Process builtin_core.llt inline.
    let core_parsed = parse(
        include_str!("../stdlib/builtin_core.llt"),
        Arc::from("stdlib/builtin_core.llt"),
    )
    .map_err(|e| format!("builtin_core.llt parse error: {e}"))?;
    let core_program = desugar::desugar_program_full(&core_parsed.program);
    let core_program = expand::expand_program(&core_program);
    for doc in &core_program.documents {
        crate::pipeline::process_pipeline_document(doc, &mut state, &ctx)
            .await
            .map_err(|e| format!("builtin_core.llt: {e}"))?;
    }
    let mut diagnostics = Vec::new();
    for doc in &program.documents {
        let mut doc_diags = crate::pipeline::typecheck_pipeline_document(doc, &mut state).await;
        diagnostics.append(&mut doc_diags);
    }
    // Include state-level diagnostics (e.g. unknown-type warnings from unification).
    diagnostics.append(&mut state.infer_state.diagnostics);
    if !crate::error::has_type_errors(&diagnostics) {
        Ok(())
    } else {
        let msgs: Vec<String> = diagnostics
            .iter()
            .filter(|d| d.level == crate::error::DiagnosticLevel::Err)
            .map(|e| e.message.clone())
            .collect();
        Err(msgs.join("\n"))
    }
}

// --- Value Serializer Visitor Pattern ---
//
// `visit_value` and `value_to_display_string` share the same structural traversal
// (depth guard, Dict/Seq entry materialization) but diverge at
// leaf rendering. A `ValueVisitor` trait captures the shared traversal in `visit_value`
// while each visitor impl handles the format-specific leaf rendering.

/// Visitor trait for materialised [`Value`](value::Value) trees.
///
/// Implement this trait to produce a format-specific output from a `Value`.
/// The shared `visit_value` function handles structural traversal (depth limit,
/// Dict/Seq entry materialization); visitor methods handle
/// leaf rendering and container assembly.
///
/// Dict entries are pre-converted to `Self::Output` before `visit_dict` is called,
/// so the visitor need not recurse manually.
pub trait ValueVisitor {
    type Output;

    fn visit_int(&self, v: i64) -> Self::Output;
    fn visit_float(&self, v: f64, span: ast::Span) -> Result<Self::Output, Box<error::Diagnostic>>;
    fn visit_str(&self, v: &str) -> Self::Output;
    fn visit_bytes(&self, v: &[u8]) -> Self::Output;
    fn visit_null(&self) -> Self::Output;
    fn visit_dict(&self, entries: Vec<(value::HashableValue, Self::Output)>) -> Self::Output;
    fn visit_seq_head(
        &self,
        head: Self::Output,
        span: ast::Span,
    ) -> Result<Self::Output, Box<error::Diagnostic>>;
    fn visit_function(
        &self,
        params: &[ast::Param],
        span: ast::Span,
    ) -> Result<Self::Output, Box<error::Diagnostic>>;
    fn visit_builtin(
        &self,
        name: &str,
        span: ast::Span,
    ) -> Result<Self::Output, Box<error::Diagnostic>>;
    fn visit_proxy(&self, span: ast::Span) -> Result<Self::Output, Box<error::Diagnostic>>;
    fn visit_variant(&self, tag: String, payload: Self::Output) -> Self::Output;
    fn visit_decimal(&self, v: rust_decimal::Decimal) -> Self::Output;
    fn visit_bigint(&self, v: &num_bigint::BigInt) -> Self::Output;
    fn visit_timestamp(
        &self,
        nanos: i64,
        span: ast::Span,
    ) -> Result<Self::Output, Box<error::Diagnostic>>;
    fn visit_duration(&self, nanos: i64) -> Self::Output;
    fn visit_clock_cap(&self, span: ast::Span) -> Result<Self::Output, Box<error::Diagnostic>>;
    fn visit_timezone(&self, span: ast::Span) -> Result<Self::Output, Box<error::Diagnostic>>;
    /// Return `Some(output)` if the depth limit has been reached, `None` to continue.
    fn depth_limit_output(
        &self,
        depth: usize,
        span: ast::Span,
    ) -> Option<Result<Self::Output, Box<error::Diagnostic>>>;
}

/// Type alias for the pinned future returned by `visit_value`.
type VisitValueFuture<'a, Output> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Output, Box<error::Diagnostic>>> + Send + 'a>,
>;

/// Shared structural traversal for materialised `Value` trees.
///
/// Handles depth limiting and `Dict`/`Seq` entry
/// materialisation. Leaf rendering is delegated to the provided [`ValueVisitor`].
///
/// # Panics
///
/// Does not panic. All errors are propagated via `Result`.
pub fn visit_value<'a, V: ValueVisitor + Sync + 'a>(
    val: &'a value::Value,
    ctx: &'a Arc<eval::EvalContext>,
    depth: usize,
    visitor: &'a V,
    span: ast::Span,
) -> VisitValueFuture<'a, V::Output>
where
    V::Output: Send,
{
    Box::pin(async move {
        if let Some(limit_result) = visitor.depth_limit_output(depth, span.clone()) {
            return limit_result;
        }
        match val {
            value::Value::Int { n, .. } => Ok(visitor.visit_int(*n)),
            // U64 values: for serialization, emit as Int when they fit in i64,
            // otherwise serialize as BigInt equivalent. The visitor uses visit_bigint.
            value::Value::U64 { n, .. } => {
                if let Ok(i) = i64::try_from(*n) {
                    Ok(visitor.visit_int(i))
                } else {
                    Ok(visitor.visit_bigint(&num_bigint::BigInt::from(*n)))
                }
            }
            value::Value::Float { n, .. } => visitor.visit_float(*n, span),
            value::Value::String {
                ref source,
                start,
                end,
                ..
            } => {
                let s = &source[*start..*end];
                Ok(visitor.visit_str(s))
            }
            value::Value::Bytes {
                ref source,
                start,
                end,
                ..
            } => {
                let bytes = &source[*start..*end];
                Ok(visitor.visit_bytes(bytes))
            }
            value::Value::Dict { entries: map, .. } => {
                let mut entries = Vec::with_capacity(map.len());
                for (key, thunk) in map {
                    let v = eval::materialize(thunk, None, ctx).await?;
                    let child_span = thunk.span.clone();
                    entries.push((
                        key.clone(),
                        visit_value(&v, ctx, depth + 1, visitor, child_span).await?,
                    ));
                }
                Ok(visitor.visit_dict(entries))
            }
            value::Value::Function { clauses, .. } => {
                // Extract params from clause 0 (single-clause common case; T-2133 for multi-clause).
                let clause = clauses
                    .first()
                    .expect("Value::Function must have at least one clause");
                let params: Vec<ast::Param> = clause
                    .params
                    .iter()
                    .map(|p| ast::Param {
                        name: p.node.name.clone(),
                        annotation: p.node.annotation.clone(),
                        variadic: p.node.variadic,
                        slot: p.node.slot,
                        resolved_type: p.node.resolved_type.clone(),
                    })
                    .collect();
                visitor.visit_function(&params, span)
            }
            value::Value::Builtin { def, .. } => visitor.visit_builtin(def.name, span),
            value::Value::Proxy { .. } => visitor.visit_proxy(span),
            value::Value::DirCap { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("DirCap".to_string(), span),
            )),
            value::Value::NetCap { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("NetCap".to_string(), span),
            )),
            value::Value::File { .. } => Err(Box::new(error::Diagnostic::value_not_serializable(
                "File".to_string(),
                span,
            ))),
            value::Value::RevocableDirCap { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("RevocableDirCap".to_string(), span),
            )),
            value::Value::Variant {
                type_decl_id,
                ctor_index,
                payload,
                ..
            } => {
                let payload_output = match payload {
                    Some(thunk_id) => {
                        let thunk = Arc::clone(thunk_id);
                        let v = eval::materialize(&thunk, None, ctx).await?;
                        let payload_span = thunk.span.clone();
                        visit_value(&v, ctx, depth + 1, visitor, payload_span).await?
                    }
                    None => visitor.visit_null(),
                };
                Ok(visitor.visit_variant(
                    {
                        let empty = std::collections::HashMap::new();
                        let tycon_env = ctx.tycon_env().unwrap_or(&empty);
                        crate::type_tags::ctor_display_name(*type_decl_id, *ctor_index, tycon_env)
                    },
                    payload_output,
                ))
            }
            value::Value::Decimal { n: d, .. } => Ok(visitor.visit_decimal(*d)),
            value::Value::BigInt { n, .. } => Ok(visitor.visit_bigint(n)),
            value::Value::Uri { .. } => Err(Box::new(error::Diagnostic::value_not_serializable(
                "Uri".to_string(),
                span,
            ))),
            value::Value::Timestamp { ts, .. } => {
                visitor.visit_timestamp(ts.as_nanosecond() as i64, span)
            }
            value::Value::Duration { nanos, .. } => Ok(visitor.visit_duration(*nanos)),
            value::Value::ClockCap { .. } => visitor.visit_clock_cap(span),
            value::Value::Timezone { .. } => visitor.visit_timezone(span),
            value::Value::QuicSession { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("QuicSession".to_string(), span),
            )),
            value::Value::Http2Session { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("Http2Session".to_string(), span),
            )),
            value::Value::Http3Session { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("Http3Session".to_string(), span),
            )),
            value::Value::QuicDatagramHandle { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("QuicDatagramHandle".to_string(), span),
            )),
            value::Value::Program { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("Program".to_string(), span),
            )),
            value::Value::Document { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("Document".to_string(), span),
            )),
            // Expr.* variants (Value::Variant) are handled by the Variant arm above.
            value::Value::Task { .. } => Err(Box::new(error::Diagnostic::value_not_serializable(
                "Task".to_string(),
                span,
            ))),
            value::Value::Channel { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("Channel".to_string(), span),
            )),
            value::Value::Context { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("Context".to_string(), span),
            )),
            value::Value::ReactiveCell { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("ReactiveCell".to_string(), span),
            )),
            value::Value::Builder(_) => Err(Box::new(error::Diagnostic::value_not_serializable(
                "Builder".to_string(),
                span,
            ))),
            value::Value::BroadcastChannel { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("BroadcastChannel".to_string(), span),
            )),
            value::Value::OneshotSender { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("OneshotSender".to_string(), span),
            )),
            value::Value::OneshotReceiver { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("OneshotReceiver".to_string(), span),
            )),
            // Annotated is transparent — delegate to inner value serialization.
            value::Value::Annotated { inner, .. } => {
                visit_value(inner, ctx, depth, visitor, span).await
            }
            value::Value::TypeContext { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("TypeContext".to_string(), span),
            )),
            value::Value::Expression { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("Expression".to_string(), span),
            )),
            value::Value::Arena { .. } => Err(Box::new(error::Diagnostic::value_not_serializable(
                "Arena".to_string(),
                span,
            ))),
            value::Value::CoreDocument { .. } => Err(Box::new(
                error::Diagnostic::value_not_serializable("CoreDocument".to_string(), span),
            )),
            value::Value::Scope { .. } => Err(Box::new(error::Diagnostic::value_not_serializable(
                "Scope".to_string(),
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

/// Visitor that produces tinct source-format repr: `["key": "value"  42  ...]`.
/// This is the format used by `builtin-llt-repr` for test output comparison.
struct TinctReprVisitor;

impl ValueVisitor for TinctReprVisitor {
    type Output = String;

    fn visit_int(&self, v: i64) -> String {
        v.to_string()
    }
    fn visit_float(&self, v: f64, _span: ast::Span) -> Result<String, Box<error::Diagnostic>> {
        if v.fract() == 0.0 && v.is_finite() {
            Ok(format!("{v:.1}"))
        } else {
            Ok(v.to_string())
        }
    }
    fn visit_str(&self, v: &str) -> String {
        // Produce a quoted string: escape internal quotes and backslashes.
        let mut result = String::from('"');
        for c in v.chars() {
            match c {
                '"' => result.push_str("\\\""),
                '\\' => result.push_str("\\\\"),
                '\n' => result.push_str("\\n"),
                '\t' => result.push_str("\\t"),
                '\r' => result.push_str("\\r"),
                c => result.push(c),
            }
        }
        result.push('"');
        result
    }
    fn visit_bytes(&self, v: &[u8]) -> String {
        format!("<{} bytes>", v.len())
    }
    fn visit_null(&self) -> String {
        "[]".to_string()
    }
    fn visit_dict(&self, entries: Vec<(value::HashableValue, String)>) -> String {
        if entries.is_empty() {
            return "[]".to_string();
        }
        let mut parts = Vec::with_capacity(entries.len());
        for (key, val_str) in entries {
            match key {
                value::HashableValue::Int(_) => {
                    // Positional (auto-indexed) entry — show just the value
                    parts.push(val_str);
                }
                value::HashableValue::Str(s) => {
                    // String-keyed entry — show "key": value
                    let mut entry = String::new();
                    entry.push('"');
                    for c in s.chars() {
                        match c {
                            '"' => entry.push_str("\\\""),
                            '\\' => entry.push_str("\\\\"),
                            c => entry.push(c),
                        }
                    }
                    entry.push('"');
                    entry.push_str(": ");
                    entry.push_str(&val_str);
                    parts.push(entry);
                }
                other => {
                    // Non-string/non-int key (Bool, Dict, Variant) — show as "Display: value"
                    let mut entry = format!("{}", other);
                    entry.push_str(": ");
                    entry.push_str(&val_str);
                    parts.push(entry);
                }
            }
        }
        format!("[{}]", parts.join("  "))
    }
    fn visit_seq_head(
        &self,
        head: String,
        _span: ast::Span,
    ) -> Result<String, Box<error::Diagnostic>> {
        Ok(format!("[{head}  ...]"))
    }
    fn visit_function(
        &self,
        params: &[ast::Param],
        _span: ast::Span,
    ) -> Result<String, Box<error::Diagnostic>> {
        let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
        Ok(format!("[fn [let {}] ...]", names.join(" ")))
    }
    fn visit_builtin(
        &self,
        name: &str,
        _span: ast::Span,
    ) -> Result<String, Box<error::Diagnostic>> {
        Ok(format!("<builtin:{name}>"))
    }
    fn visit_proxy(&self, _span: ast::Span) -> Result<String, Box<error::Diagnostic>> {
        Ok("<proxy>".to_string())
    }
    fn visit_variant(&self, tag: String, payload: String) -> String {
        if payload == "[]" {
            // Unit variant
            tag
        } else {
            format!("[{tag} {payload}]")
        }
    }
    fn visit_decimal(&self, v: rust_decimal::Decimal) -> String {
        v.to_string()
    }
    fn visit_bigint(&self, v: &num_bigint::BigInt) -> String {
        v.to_string()
    }
    fn visit_timestamp(
        &self,
        nanos: i64,
        span: ast::Span,
    ) -> Result<String, Box<error::Diagnostic>> {
        match jiff::Timestamp::from_nanosecond(nanos as i128) {
            Ok(ts) => Ok(format!("{ts}")),
            Err(e) => Err(Box::new(error::Diagnostic::internal(
                format!("invalid timestamp: {e}"),
                span,
            ))),
        }
    }
    fn visit_duration(&self, nanos: i64) -> String {
        format!("<duration:{nanos}ns>")
    }
    fn visit_clock_cap(&self, _span: ast::Span) -> Result<String, Box<error::Diagnostic>> {
        Ok("<clock>".to_string())
    }
    fn visit_timezone(&self, _span: ast::Span) -> Result<String, Box<error::Diagnostic>> {
        Ok("<timezone>".to_string())
    }
    fn depth_limit_output(
        &self,
        depth: usize,
        _span: ast::Span,
    ) -> Option<Result<String, Box<error::Diagnostic>>> {
        if depth >= MAX_DISPLAY_DEPTH {
            Some(Ok("...".to_string()))
        } else {
            None
        }
    }
}

struct DisplayVisitor;

impl ValueVisitor for DisplayVisitor {
    type Output = String;

    fn visit_int(&self, v: i64) -> String {
        format!("Int({v})")
    }
    fn visit_float(&self, v: f64, _span: ast::Span) -> Result<String, Box<error::Diagnostic>> {
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
    ) -> Result<String, Box<error::Diagnostic>> {
        Ok(format!("Seq({head}, ...)"))
    }
    fn visit_function(
        &self,
        params: &[ast::Param],
        _span: ast::Span,
    ) -> Result<String, Box<error::Diagnostic>> {
        let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
        Ok(format!("Function({})", names.join(", ")))
    }
    fn visit_builtin(
        &self,
        name: &str,
        _span: ast::Span,
    ) -> Result<String, Box<error::Diagnostic>> {
        Ok(format!("Builtin({name})"))
    }
    fn visit_proxy(&self, _span: ast::Span) -> Result<String, Box<error::Diagnostic>> {
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
        span: ast::Span,
    ) -> Result<String, Box<error::Diagnostic>> {
        // Format as RFC 3339 for readability
        match jiff::Timestamp::from_nanosecond(nanos as i128) {
            Ok(ts) => Ok(format!("Timestamp({})", ts)),
            Err(e) => Err(Box::new(error::Diagnostic::internal(
                format!("invalid timestamp: {e}"),
                span,
            ))),
        }
    }
    fn visit_duration(&self, nanos: i64) -> String {
        format!("Duration({} ns)", nanos)
    }
    fn visit_clock_cap(&self, _span: ast::Span) -> Result<String, Box<error::Diagnostic>> {
        Ok("ClockCap".to_string())
    }
    fn visit_timezone(&self, _span: ast::Span) -> Result<String, Box<error::Diagnostic>> {
        Ok("Timezone".to_string())
    }
    fn depth_limit_output(
        &self,
        depth: usize,
        _span: ast::Span,
    ) -> Option<Result<String, Box<error::Diagnostic>>> {
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
/// Convert a Value into tinct source-format repr (e.g. `["hello": "world"]`).
///
/// Used by `builtin-llt-repr` to format test output for corpus test comparison.
pub async fn value_to_tinct_repr_string(
    val: &value::Value,
    ctx: &Arc<eval::EvalContext>,
    span: ast::Span,
) -> Result<String, Box<error::Diagnostic>> {
    visit_value(val, ctx, 0, &TinctReprVisitor, span).await
}

pub async fn value_to_display_string(
    val: &value::Value,
    ctx: &Arc<eval::EvalContext>,
    span: ast::Span,
) -> Result<String, Box<error::Diagnostic>> {
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
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            eprintln!(
                "tinct: warning: could not determine executable path for libdir discovery: {e}"
            );
            return None;
        }
    };
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
    use type_infer::make_typevalue_nominal;
    use value::{Thunk, Value, TYPE_ID_INT, TYPE_ID_PROXY};

    const TEST_ID_COLOR: u64 = 500;
    const TEST_ID_MY_BOOL: u64 = 501;

    // Async wrappers for async public functions — test code uses .await.
    async fn value_to_display_string(
        val: &value::Value,
        ctx: &Arc<eval::EvalContext>,
        span: ast::Span,
    ) -> Result<String, Box<error::Diagnostic>> {
        super::value_to_display_string(val, ctx, span).await
    }

    async fn test_ctx() -> Arc<eval::EvalContext> {
        eval::EvalContext::new_empty()
    }

    #[test]
    fn test_display_unit_variant() {
        // Unit variants display as their full qualified tag via Rust Display.
        let v = Value::Variant {
            type_decl_id: TEST_ID_COLOR,
            ctor_index: 0,
            payload: None,
            type_val: Some(make_typevalue_nominal(TEST_ID_COLOR)),
        };
        let empty_env = std::collections::HashMap::new();
        assert_eq!(v.render(&empty_env), "Variant(500.0)");
    }

    /// Regression: all unit variants must serialise uniformly as `Variant(Tag, Null)`.
    /// The serialiser is agnostic to ADT tag names — no tag name receives special privilege.
    ///
    /// Before the fix, `visit_value` short-circuited on certain variant tags and dispatched to a
    /// now-deleted `visit_bool` method.  After the fix all variants fall through to the generic
    /// `Variant` arm.  `visit_bool` has since been removed from the `ValueVisitor` trait entirely.
    #[tokio::test]
    async fn test_display_unit_variant_uniform() {
        let ctx = test_ctx().await;

        let red_val = Value::Variant {
            type_decl_id: TEST_ID_COLOR,
            ctor_index: 0,
            payload: None,
            type_val: Some(make_typevalue_nominal(TEST_ID_COLOR)),
        };
        let green_val = Value::Variant {
            type_decl_id: TEST_ID_COLOR,
            ctor_index: 1,
            payload: None,
            type_val: Some(make_typevalue_nominal(TEST_ID_COLOR)),
        };

        let red_display = value_to_display_string(&red_val, &ctx, rust_span!())
            .await
            .expect("display should succeed for Color.Red");
        let green_display = value_to_display_string(&green_val, &ctx, rust_span!())
            .await
            .expect("display should succeed for Color.Green");

        // Must serialize as a generic variant.
        assert_eq!(
            red_display, "Variant(Ctor(500.0), Null)",
            "Color.Red must serialise as Variant(Ctor(type_decl_id.ctor_index), Null)"
        );
        assert_eq!(
            green_display, "Variant(Ctor(500.1), Null)",
            "Color.Green must serialise as Variant(Ctor(type_decl_id.ctor_index), Null)"
        );

        // A unit variant from a different type must produce the same structure.
        let user_val = Value::Variant {
            type_decl_id: TEST_ID_MY_BOOL,
            ctor_index: 0,
            payload: None,
            type_val: Some(make_typevalue_nominal(TEST_ID_MY_BOOL)),
        };
        let user_display = value_to_display_string(&user_val, &ctx, rust_span!())
            .await
            .expect("display should succeed for MyBool.Yes");
        assert_eq!(
            user_display, "Variant(Ctor(501.0), Null)",
            "User-defined unit variant must serialise uniformly"
        );
    }

    #[tokio::test]
    async fn test_display_proxy() {
        let ctx = test_ctx().await;
        let handler_thunk = Arc::new(Thunk::value(
            Value::Int {
                n: 42,
                type_val: Some(make_typevalue_nominal(TYPE_ID_INT)),
            },
            test_span(1, 1, 1, 1),
        ));
        let proxy = Value::Proxy {
            handler: handler_thunk,
            type_val: Some(make_typevalue_nominal(TYPE_ID_PROXY)),
        };
        let display = value_to_display_string(&proxy, &ctx, rust_span!())
            .await
            .expect("display failed");
        assert_eq!(display, "Proxy");
    }

    // --- Integration tests: render_span_snippet in error output ---

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
        use crate::ast::Span;

        // A three-line expression:
        //   line 1: "let x = ["
        //   line 2: "  missing_key"
        //   line 3: "]"
        let source = "let x = [\n  missing_key\n]";

        // Span covering the entire expression: line 1 col 1 → line 3 col 2.
        let span = Span {
            file: rust_span!().file,
            start_line: 1,
            start_col: 1,
            end_line: 3,
            end_col: 2,
            name: None,
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
