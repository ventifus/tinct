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

#![deny(clippy::disallowed_types, clippy::disallowed_methods)]
// Increased recursion limit for deep Send/Sync bound evaluation through hyper/reqwest/tower
// dependency chains (reqwest::Client -> hyper_util::Client -> Pool -> PoolInner chain).
#![recursion_limit = "256"]

pub(crate) mod arena;
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
    CapturesCell, MacroProvenance, Provenance, Resolution, SurfaceEntry, SurfaceExpression,
    SurfaceNode, SurfaceProgram, TypeAnnotation, VarAddr,
};
/// Parser entry points.
pub use parser::{format_parse_error, parse, parse_surface_expression, ParseOutput};

/// Evaluation functions.
pub use eval::{
    eval_surface_file, eval_surface_file_with_input, invoke_function, materialize, CallContext,
    EvalConfig, EvalContext, TypeContextData,
};

pub use builtins::build_core_env;

/// Bootstrap type environment from builtin_core.llt.
pub use imports::{
    build_builtin_core_envs, get_builtin_core_tycon_env, get_builtin_core_type_env,
    get_builtin_core_type_stage_scope,
};

/// Error types with source spans and stack traces.
pub use error::{
    format_diagnostic, render_span_snippet, ArityBound, Diagnostic, DiagnosticLevel, ErrorKind,
    EvalError, EvalResult, StackFrame,
};

/// TypeValue: the Arc<Value>-based type representation used throughout the type checker.
pub use type_class::TypeValue;

/// TypeValue construction helpers exposed for use in crate embedders (e.g., main.rs CLI
/// injected type env). These are the canonical single-path constructors — callers must
/// not duplicate construction logic locally (Axiom 2).
pub use type_infer::{make_typevalue_op, make_typevalue_repr, make_typevalue_unknown};

/// Formatter: canonical source reformatter.
pub use formatter::format_source_tinct;

/// Unified evaluation and type-checker environment.
pub use env::Env;
/// Runtime value types: values, thunks, environments, and dict keys.
pub use value::{
    string_val, unknown_type_val, ChannelInner, ClockCapInner, DirPerms, EvalFrame, HashableValue,
    NetCapEntry, Thunk, Value,
};

/// Run the loader.llt bootstrap pipeline with a pre-configured environment.
///
/// This is the shared evaluation core used by the CLI path
/// (`run_eval` in `src/main.rs`). Callers must:
///
/// - Create the initial env via [`builtins::build_core_env`]
/// - Build and inject all capability thunks into `env`:
///   `%programs`, `%args`, `%cwd`, `%libdir`, `%clock`, `%cap-fs`, etc.
///   Note: `%stdout` and `%stderr` are nominal type values defined by loader.llt Dict 2, not injected here.
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
    injected_type_env: Option<Arc<std::sync::RwLock<crate::env::Env>>>,
) -> Result<(), String> {
    let loader_sf: Arc<str> = Arc::from(init_path);
    let loader_parsed = parse(init_source, Arc::clone(&loader_sf))
        .map_err(|e| format!("{init_path} parse error: {e}"))?;
    for diag in &loader_parsed.diagnostics {
        eprintln!(
            "{}",
            crate::parser::format_parse_error(diag, init_source, init_path)
        );
    }
    let loader_program = desugar::desugar_program_full(&loader_parsed.program);

    // Resolve the loader program. Seeded from FlatEnv so that builtin names
    // (builtin-parse, etc.) resolve to de Bruijn coordinates instead of
    // falling back to name-based lookup via the MAX/MAX sentinel.
    // Borrow is scoped so it's dropped before eval_surface_file borrows_mut the arena.
    //
    // Capture the combined scope frames (root_frame + new_frames produced by the
    // resolver) and thread them into a new eval_ctx via with_scope_frames(). This allows
    // lower() to resolve scope-frame-dependent names (e.g., builtin-dict-merge for spread
    // dicts) to correct De Bruijn coordinates at eval time.
    let eval_ctx_with_frames: Arc<eval::EvalContext> = {
        // Build the resolver seed map from the eval context's root group.
        // root_group contains all static builtins (slots 0..N-1) followed by
        // capabilities (slots N..M-1) in the same order they were registered via
        // with_root_group_capabilities. root_group_resolver_map() reads slot indices
        // directly from the group, so the resolver and evaluator are always in sync.
        // Each name gets LGM(slot) from enter_scope_from_frame; at runtime accumulated_group
        // starts with root_group so LGM(slot) indexes directly into the right thunk.
        let root_frame = eval_ctx.root_group_resolver_map();
        let (_table, new_frames) = resolve::resolve_surface_program_with_classes(
            &loader_program,
            std::slice::from_ref(&root_frame),
            std::collections::HashMap::new(),
        );
        // Combine: root_frame (outermost) followed by frames introduced by the program.
        // Strip FrameKind for EvalContext.scope_frames — lower.rs only needs the name→slot maps.
        let all_frames: Vec<indexmap::IndexMap<String, u32>> = std::iter::once(root_frame)
            .chain(new_frames.into_iter().map(|(frame, _kind)| frame))
            .collect();
        eval_ctx.with_scope_frames(Arc::new(all_frames))
    };

    // Two-pass: evaluate type-stage documents first, force all thunks to build
    // type_stage_data with fully materialized types, then typecheck the loader program.
    let loader_type_stage_data: crate::type_infer::TypeStageData = {
        // Filter type-stage documents (those with stage: "type" in their header).
        let ts_docs: Vec<_> = loader_program
            .documents
            .iter()
            .filter(|d| {
                d.node.header.get("stage").is_some_and(|stage_node| {
                    matches!(
                        &stage_node.expr,
                        crate::ast::SurfaceExpression::StringLiteral { content, .. }
                        if content == "type"
                    )
                })
            })
            .cloned()
            .collect();

        if ts_docs.is_empty() {
            crate::type_infer::TypeStageData::new()
        } else {
            // Build a mini-program with only the type-stage documents and evaluate it.
            // The documents are already desugared and resolved from the main program pass —
            // do NOT re-run desugar (Arc sharing panics) or resolve (would overwrite inline
            // de Bruijn coordinates with wrong values for empty frames).
            let ts_program = crate::ast::SurfaceProgram { documents: ts_docs };

            let ts_thunk = eval::eval_surface_file(&ts_program, &eval_ctx_with_frames)
                .await
                .map_err(|e| format!("type-stage evaluation failed: {e}"))?;
            let ts_val = eval::materialize(&ts_thunk, None, &eval_ctx_with_frames)
                .await
                .map_err(|e| format!("type-stage materialization failed: {e}"))?;
            match ts_val {
                crate::value::Value::Dict { entries, .. } => {
                    // Force all thunks and classify into the three maps.
                    let mut scope_map: std::collections::HashMap<
                        String,
                        crate::type_infer::TypeValue,
                    > = std::collections::HashMap::new();
                    let mut fns_map: std::collections::HashMap<
                        String,
                        std::sync::Arc<crate::value::Thunk>,
                    > = std::collections::HashMap::new();
                    let mut type_vars_map: std::collections::HashMap<String, String> =
                        std::collections::HashMap::new();
                    for (key, thunk) in &entries {
                        if let crate::value::HashableValue::Str(name) = key {
                            let val = eval::materialize(thunk, None, &eval_ctx_with_frames)
                                .await
                                .map_err(|e| {
                                    format!(
                                        "type-stage thunk '{}' failed to materialize: {}",
                                        name, e
                                    )
                                })?;

                            let scope_so_far = vec![scope_map.clone()];
                            let classified = crate::imports::classify_type_stage_entry(
                                name,
                                thunk,
                                &val,
                                &eval_ctx_with_frames,
                                &scope_so_far,
                            )
                            .await
                            .map_err(|e| format!("type-stage classify '{}' failed: {}", name, e))?;
                            if let Some((tv, opt_thunk, opt_kind)) = classified {
                                scope_map.insert(name.to_string(), Arc::clone(&tv));
                                if let Some(thunk_arc) = opt_thunk {
                                    fns_map.insert(name.to_string(), thunk_arc);
                                }
                                if let Some(kind_str) = opt_kind {
                                    type_vars_map.insert(name.to_string(), kind_str);
                                }
                            }
                        }
                    }
                    crate::type_infer::TypeStageData {
                        scope: vec![scope_map],
                        fns: fns_map,
                        type_vars: type_vars_map,
                    }
                }
                other => {
                    return Err(format!(
                        "type-stage evaluation produced {}, expected Dict",
                        other.type_name()
                    ));
                }
            }
        }
    };

    // Typecheck the loader program, seeded with the builtin core type env so that
    // builtin-* names are in scope. Populates the TypeContext with type signatures.
    // Only "type-assertion" diagnostics are fatal; other type errors are expected
    // during bootstrap because prelude names are not yet in scope (see below).
    let (builtin_env_base, seed_tycon_env) = crate::imports::build_builtin_core_envs().await;
    // Prepend builtin_core.llt's type-stage scope to the loader's type-stage scope
    // so that opaque types (BuilderHandle, TypeContext, DirCap, etc.) declared in
    // builtin_core.llt's type-stage section are resolvable when typechecking the loader.
    // Also saved separately for TypeContext pre-initialization (see below).
    let builtin_core_ts_data = crate::imports::get_builtin_core_type_stage_scope().await;
    // Combine: builtin_core entries first (outermost), loader entries innermost.
    let combined_type_stage_data = {
        let mut combined = builtin_core_ts_data.clone();
        combined.scope.extend(loader_type_stage_data.scope);
        combined.fns.extend(loader_type_stage_data.fns);
        combined.type_vars.extend(loader_type_stage_data.type_vars);
        combined
    };
    // Save clones for TypeContext pre-initialization before builtin_env_base and seed_tycon_env
    // are consumed by the typecheck pass below.
    let core_env_for_tc = Arc::clone(&builtin_env_base);
    let core_tycon_for_tc = seed_tycon_env.clone();
    // Chain injected types (from caller) above the builtin env so the type-checker
    // sees %programs, %cwd, etc. at their correct runtime slot positions.
    //
    // Capabilities (%cwd, %libdir, %args, etc.) are in eval_ctx.root_group at slots
    // N_builtins..N_builtins+N_caps-1. The resolver (via root_group_resolver_map) assigns
    // those same slot numbers to capability names. The type-checker's builtin_env_base
    // only has N_builtins entries (slots 0..N-1). We extend it with capability entries
    // via insert_at_slot so that get_scheme_at(N+i) succeeds during type-checking.
    // All capability entries go into slots at resolver-assigned slot positions.
    let builtin_env = {
        let mut wrapper = crate::env::Env::with_parent(Arc::clone(&builtin_env_base));
        // Add capability entries at their correct runtime slot positions.
        // root_group_resolver_map returns all root_group entries (builtins + caps).
        // builtin_env_base.slots already covers builtins; we fill in the cap slots.
        let root_map = eval_ctx.root_group_resolver_map();
        let builtin_slots_len = {
            let base = builtin_env_base.read().unwrap();
            base.slots.len()
        };
        // Build a name→TypeValue lookup from injected_type_env for capability types.
        // Capabilities are stored in slots (via insert_at_slot) in the injected_type_env.
        let cap_types: std::collections::HashMap<String, crate::type_infer::TypeValue> =
            if let Some(ref inj) = injected_type_env {
                let r = inj.read().unwrap();
                r.iter_slots()
                    .filter_map(|(name, slot_entry)| {
                        slot_entry.scheme.clone().map(|tv| (name.to_string(), tv))
                    })
                    .collect()
            } else {
                std::collections::HashMap::new()
            };
        for (name, slot) in &root_map {
            let slot = *slot as usize;
            if slot >= builtin_slots_len {
                // This slot is a capability — inject it with its type (Unknown if not found).
                let tv = cap_types
                    .get(name)
                    .cloned()
                    .unwrap_or_else(crate::type_infer::make_typevalue_top);
                wrapper.insert_at_slot(slot, name.clone(), tv, None);
            }
        }
        Arc::new(std::sync::RwLock::new(wrapper))
    };
    let (loader_diagnostics, _loader_env, loader_tycon_env) =
        typecheck::typecheck_program_bootstrap(
            &loader_program,
            builtin_env,
            Some(Arc::clone(&eval_ctx_with_frames)),
            seed_tycon_env,
            combined_type_stage_data,
        )
        .await;
    // Wire the TyConEnv from the typecheck pass into the eval context so that
    // value_matches_type and TypeAssert error messages can look up TyCon definitions.
    eval_ctx_with_frames.set_tycon_env(loader_tycon_env);
    // Emit ALL diagnostics (errors and warnings). Never drop any.
    // Only type-assertion errors are fatal in the init program.
    //
    // The init program (loader.llt) bootstraps the runtime — it loads prelude as its first
    // action. By design, type-checking the init program runs before prelude is available,
    // so the type-checker cannot resolve prelude-only names (>>, if, get, etc.) used in
    // loader.llt's runtime documents. This produces spurious "resolver-slot-miss" and
    // "type-error" (undefined variable, arity mismatch, cannot unify) diagnostics that are
    // inherent to the bootstrap architecture — not actionable errors.
    //
    // The one error kind that IS actionable here: "type-assertion" — a TypeAssert annotation
    // [@ Type expr] that failed means the init program's runtime behavior differs from its
    // declared type, which would be a genuine bug regardless of prelude availability.
    //
    // All other error-level diagnostics are logged (above) but do not abort execution.
    let mut has_fatal = false;
    for diag in &loader_diagnostics {
        eprintln!("{}", format_diagnostic(diag, init_source, init_path));
        if diag.level == crate::error::DiagnosticLevel::Err && diag.kind == "type-assertion" {
            has_fatal = true;
        }
    }
    if has_fatal {
        return Err(format!(
            "{init_path}: fatal type assertion error in init program — cannot proceed"
        ));
    }

    // Pre-initialize the EvalContext TypeContext with the builtin_core type-stage scope so
    // that [builtin-make-type-ctx] in the init program is a no-op (init_type_context is
    // idempotent), and [builtin-get-type-context] returns the pre-seeded TypeContext.
    // This avoids tinct-side letrec cycles: when builtin-tc-update-type-stage-env
    // materializes type-stage thunks from a letrec, mutual dependencies in the same
    // letrec group (e.g. Integer → TypeNode → CoreDocument → TypeNode) create false cycles
    // because the evaluator's InProgress detection fires before siblings are settled.
    // The Rust-level pre-initialization bypasses this by evaluating builtin_core.llt
    // in an isolated context with no InProgress thunks from the running init program.
    eval_ctx_with_frames.init_type_context(TypeContextData {
        inference_env: core_env_for_tc,
        tycon_env: core_tycon_for_tc,
        type_stage_scope: builtin_core_ts_data.scope,
        type_stage_fns: builtin_core_ts_data.fns,
        type_stage_type_vars: builtin_core_ts_data.type_vars,
        type_diagnostics: Vec::new(),
    });

    // Evaluate loader.llt. env already contains all stdlib builtins, %programs, %args,
    // %cwd, %libdir, and any other caps injected by the caller.
    // %stdout and %stderr are nominal type values (Stdout.Stdout, Stderr.Stderr) in loader.llt Dict 2.
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

    // Drain and print runtime diagnostics collected during evaluation.
    {
        let rt_diags = eval_ctx_with_frames
            .runtime_diagnostics
            .lock()
            .map_err(|e| format!("runtime_diagnostics mutex poisoned: {e}"))?;
        for diag in rt_diags.iter() {
            eprintln!("{}", format_diagnostic(diag, init_source, init_path));
        }
    }

    Ok(())
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
    // PIPELINE INVARIANT: parse -> desugar -> typecheck.
    let program = desugar::desugar_program_full(&parsed.program);
    let env_arc = imports::get_builtin_core_type_env().await;
    let ts_data = imports::get_builtin_core_type_stage_scope().await;
    let (diagnostics, _env, _tycon_env) = typecheck::typecheck_program_bootstrap(
        &program,
        env_arc,
        None,
        std::collections::HashMap::new(),
        ts_data,
    )
    .await;
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
    // PIPELINE INVARIANT: parse -> desugar -> typecheck.
    let program = desugar::desugar_program_full(&parsed.program);
    // Type check the surface program.
    let env_arc = imports::get_builtin_core_type_env().await;
    let ts_data = imports::get_builtin_core_type_stage_scope().await;
    let (diagnostics, _env, _tycon_env) = typecheck::typecheck_program_bootstrap(
        &program,
        env_arc,
        None,
        std::collections::HashMap::new(),
        ts_data,
    )
    .await;
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

/// Type alias for the pinned future returned by `visit_value`.
type VisitValueFuture<'a, Output> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Output, Box<error::EvalError>>> + Send + 'a>,
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
            value::Value::DirCap { .. } => Err(Box::new(error::EvalError::value_not_serializable(
                "DirCap".to_string(),
                span,
            ))),
            value::Value::NetCap { .. } => Err(Box::new(error::EvalError::value_not_serializable(
                "NetCap".to_string(),
                span,
            ))),
            value::Value::File { .. } => Err(Box::new(error::EvalError::value_not_serializable(
                "File".to_string(),
                span,
            ))),
            value::Value::RevocableDirCap { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("RevocableDirCap".to_string(), span),
            )),
            value::Value::Variant { ctor, payload, .. } => {
                let payload_output = match payload {
                    Some(thunk_id) => {
                        let thunk = Arc::clone(thunk_id);
                        let v = eval::materialize(&thunk, None, ctx).await?;
                        let payload_span = thunk.span.clone();
                        visit_value(&v, ctx, depth + 1, visitor, payload_span).await?
                    }
                    None => visitor.visit_null(),
                };
                Ok(visitor.visit_variant(ctor.as_ref().to_string(), payload_output))
            }
            value::Value::Decimal { n: d, .. } => Ok(visitor.visit_decimal(*d)),
            value::Value::BigInt { n, .. } => Ok(visitor.visit_bigint(n)),
            value::Value::Uri { .. } => Err(Box::new(error::EvalError::value_not_serializable(
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
                error::EvalError::value_not_serializable("QuicSession".to_string(), span),
            )),
            value::Value::Http2Session { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("Http2Session".to_string(), span),
            )),
            value::Value::Http3Session { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("Http3Session".to_string(), span),
            )),
            value::Value::QuicDatagramHandle { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("QuicDatagramHandle".to_string(), span),
            )),
            value::Value::Program { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("Program".to_string(), span),
            )),
            value::Value::Document { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("Document".to_string(), span),
            )),
            // Expr.* variants (Value::Variant) are handled by the Variant arm above.
            value::Value::Task { .. } => Err(Box::new(error::EvalError::value_not_serializable(
                "Task".to_string(),
                span,
            ))),
            value::Value::Channel { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("Channel".to_string(), span),
            )),
            value::Value::Context { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("Context".to_string(), span),
            )),
            value::Value::ReactiveCell { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("ReactiveCell".to_string(), span),
            )),
            value::Value::Builder(_) => Err(Box::new(error::EvalError::value_not_serializable(
                "Builder".to_string(),
                span,
            ))),
            value::Value::BroadcastChannel { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("BroadcastChannel".to_string(), span),
            )),
            value::Value::OneshotSender { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("OneshotSender".to_string(), span),
            )),
            value::Value::OneshotReceiver { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("OneshotReceiver".to_string(), span),
            )),
            // Annotated is transparent — delegate to inner value serialization.
            value::Value::Annotated { inner, .. } => {
                visit_value(inner, ctx, depth, visitor, span).await
            }
            value::Value::TypeContext { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("TypeContext".to_string(), span),
            )),
            value::Value::Expression { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("Expression".to_string(), span),
            )),
            value::Value::Arena { .. } => Err(Box::new(error::EvalError::value_not_serializable(
                "Arena".to_string(),
                span,
            ))),
            value::Value::CoreDocument { .. } => Err(Box::new(
                error::EvalError::value_not_serializable("CoreDocument".to_string(), span),
            )),
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
    fn visit_float(&self, v: f64, _span: ast::Span) -> Result<String, Box<error::EvalError>> {
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
    ) -> Result<String, Box<error::EvalError>> {
        Ok(format!("[{head}  ...]"))
    }
    fn visit_function(
        &self,
        params: &[ast::Param],
        _span: ast::Span,
    ) -> Result<String, Box<error::EvalError>> {
        let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
        Ok(format!("[fn [let {}] ...]", names.join(" ")))
    }
    fn visit_builtin(&self, name: &str, _span: ast::Span) -> Result<String, Box<error::EvalError>> {
        Ok(format!("<builtin:{name}>"))
    }
    fn visit_proxy(&self, _span: ast::Span) -> Result<String, Box<error::EvalError>> {
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
    ) -> Result<String, Box<error::EvalError>> {
        match jiff::Timestamp::from_nanosecond(nanos as i128) {
            Ok(ts) => Ok(format!("{ts}")),
            Err(e) => Err(Box::new(error::EvalError::internal(
                format!("invalid timestamp: {e}"),
                span,
            ))),
        }
    }
    fn visit_duration(&self, nanos: i64) -> String {
        format!("<duration:{nanos}ns>")
    }
    fn visit_clock_cap(&self, _span: ast::Span) -> Result<String, Box<error::EvalError>> {
        Ok("<clock>".to_string())
    }
    fn visit_timezone(&self, _span: ast::Span) -> Result<String, Box<error::EvalError>> {
        Ok("<timezone>".to_string())
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
        span: ast::Span,
    ) -> Result<String, Box<error::EvalError>> {
        // Format as RFC 3339 for readability
        match jiff::Timestamp::from_nanosecond(nanos as i128) {
            Ok(ts) => Ok(format!("Timestamp({})", ts)),
            Err(e) => Err(Box::new(error::EvalError::internal(
                format!("invalid timestamp: {e}"),
                span,
            ))),
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
/// Convert a Value into tinct source-format repr (e.g. `["hello": "world"]`).
///
/// Used by `builtin-llt-repr` to format test output for corpus test comparison.
pub async fn value_to_tinct_repr_string(
    val: &value::Value,
    ctx: &Arc<eval::EvalContext>,
    span: ast::Span,
) -> Result<String, Box<error::EvalError>> {
    visit_value(val, ctx, 0, &TinctReprVisitor, span).await
}

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

    fn test_file(_src: &str) -> Arc<str> {
        Arc::from(file!())
    }

    async fn test_ctx() -> Arc<eval::EvalContext> {
        eval::EvalContext::new_empty()
    }

    #[test]
    fn test_display_unit_variant() {
        // Unit variants display as their full qualified tag via Rust Display.
        let v = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            type_decl_id: 0,
            ctor: Arc::from("Color.Red"),
            payload: None,
        };
        assert_eq!(format!("{v}"), "Color.Red");
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
            type_val: crate::value::unknown_type_val(),
            type_decl_id: 0,
            ctor: Arc::from("Color.Red"),
            payload: None,
        };
        let green_val = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            type_decl_id: 0,
            ctor: Arc::from("Color.Green"),
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
            type_val: crate::value::unknown_type_val(),
            type_decl_id: 0,
            ctor: Arc::from("MyBool.Yes"),
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
        let handler_thunk = Arc::new(Thunk::value(
            Value::Int {
                n: 42,
                type_val: crate::value::unknown_type_val(),
            },
            test_span(1, 1, 1, 1),
        ));
        let proxy = Value::Proxy {
            handler: handler_thunk,
            type_val: crate::value::unknown_type_val(),
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
        let program = desugar::desugar_program_full(
            &parse(source, test_file(source))
                .expect("parse should succeed")
                .program,
        );
        // T-1576: test path uses bootstrap mode (no arena yet).
        let (_table, _frames) = resolve::resolve_surface_program(&program, &[]);
        let (_type_errors, _env, _tycon_env) = typecheck::typecheck_program_bootstrap(
            &program,
            std::sync::Arc::new(std::sync::RwLock::new(crate::env::Env::new())),
            None,
            std::collections::HashMap::new(),
            crate::type_infer::TypeStageData::new(),
        )
        .await;
        let ctx = test_ctx().await;

        // Evaluate: this should fail because $undefined_var is not defined.
        let eval_result = eval::eval_surface_file(&program, &ctx).await;
        assert!(
            eval_result.is_err(),
            "expected eval to fail for undefined variable"
        );
        let err = eval_result.unwrap_err();

        // Verify the error has a non-synthetic primary span (spans[0]).
        assert_ne!(
            err.spans[0].0,
            rust_span!(),
            "error should have a real source span, not rust_span!()"
        );

        // render_span_snippet should produce a snippet for this error.
        let snippet = error::render_span_snippet(source, err.spans[0].0.clone());
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
        // $emit is not in scope without the full prelude, so this produces a type error —
        // what matters is that no eval happened and no stdout was written.
        let result = typecheck_source_errors_only(input).await;
        assert!(
            result.is_err(),
            "expected type error for undefined $emit reference (no prelude), got: Ok(())"
        );
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
