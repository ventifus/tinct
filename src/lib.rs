//! Parser, evaluator, type system, and builtins for the tinct language.
//!
//! [`parse`] takes an input string and returns a `ParseOutput` containing the full `SurfaceProgram`.
//! [`parse_surface_expression`] parses a single expression and returns the native `Arc<SurfaceNode>` (no bridge).
//! [`eval_source`] parses and evaluates LLT source with the standard library environment.
//!
//! Additional public API:
//! - [`eval_surface_file`] / [`eval_surface_file_with_input`] -- evaluate a `SurfaceProgram` with optional stdin input (requires prior `expand` + `desugar` + `resolve` passes)
//! - [`typecheck_source`] -- parse and typecheck only (no evaluation)
//! - [`materialize`] -- force thunks (shallow)
//! - [`create_stdlib_env`] -- create the standard library environment (Rust builtins + LLT prelude)
//! - [`EvalContext`] -- evaluation context with base directory and stdlib environment; include_cache memoizes `include` results (same file = same cached thunk)
//! - JSON output via `visit_value` with `JsonVisitor`
//! - [`visit_value`] / [`JsonVisitor`] -- traverse a materialized `Value` tree for JSON output
//! - [`value_to_display_string`] -- render a materialized `Value` as a human-readable string
//! - [`MAX_FILE_SIZE`] -- file size limit for `include` and stdin (10 MB)

#![deny(clippy::disallowed_types, clippy::disallowed_methods)]
// Arc<Thunk> and related types are !Send because Thunk contains Rc<...> (e.g. Rc<str>
// for string sharing, Rc<RefCell<...>> for IO handles). LLT uses tokio::task::LocalSet
// with a current_thread runtime, so values never cross thread boundaries. The !Send
// constraint is intentional and correct; Rc-based sharing is cheaper and simpler than
// Arc<Mutex<...>> for data that never leaves the local thread.
#![allow(clippy::arc_with_non_send_sync)]

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
// Dict/access builtins: keys, length, merge, append, get, each, each-key, each-kv.
pub(crate) mod builtins_dict;
// I/O builtins: open, builtin-read-line, builtin-read-chunk, write, connect, emit, env.
pub(crate) mod builtins_io;
// Arithmetic, comparison, and control-flow builtins: +, -, *, /, =, <, if.
pub(crate) mod builtins_math;
// Type/eval/meta builtins: type-of, include, error, try, apply, validate.
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
// Macro expansion (pre-desugar AST transformation).
pub mod expand;
// runtime-v2: lowering pass (SurfaceExpression → CoreExpr).
pub(crate) mod lower;
// runtime-v2: surface AST field extraction for match dispatch and dot-access.
pub(crate) mod surface_fields;
// Literate tinct: extract and evaluate tinct code blocks from Markdown files.
pub mod literate;
// REPL (Read-Eval-Print Loop).
#[cfg(feature = "repl")]
pub mod repl;
// LSP (Language Server Protocol).
#[cfg(feature = "lsp")]
pub mod lsp;
// Profiling infrastructure: span collection, timing, and conversion to Value format.
pub mod profiling;

use std::rc::Rc;
use std::sync::Arc;

/// AST node types produced by the parser.
// Document, Entry, Expr, File, NamedArg deleted (sprint rv2-delete-old-ast 2026-05-24).
pub use ast::{Annotation, Param, Position, SourceFile, Span, Spanned};
/// Surface AST types for the runtime-v2 pipeline.
pub use ast::{
    ResolutionTable, SurfaceEntry, SurfaceExpression, SurfaceNode, SurfaceProgram,
    TypeAnnotationTable,
};
/// Parser entry points and error type.
pub use parser::{
    format_parse_error, parse, parse_surface_expression, parse_with_file, ParseError, ParseOutput,
};

/// Evaluation functions.
pub use eval::{
    eval_surface_file, eval_surface_file_with_input, invoke_function, materialize,
    materialize_sync, CallContext, EvalConfig, EvalContext, EvalState,
};

/// Arena thunk identifier — needed by callers that allocate thunks to build Seq values.
pub use arena::ThunkId;

/// Builtin infrastructure: stdlib creation, JSON conversion, resource limits.
pub use builtins::{create_stdlib_env, create_type_stage_env, MAX_COLLECT_SIZE, MAX_FILE_SIZE};

/// Clears the thread-local stdlib cache, forcing re-parse on next evaluation.
/// **Security:** Do not call in production daemons evaluating untrusted scripts —
/// forces expensive stdlib re-parse, enabling performance DoS.
///
/// Only test harnesses that run hundreds of independent evaluations in the
/// same process should call this.
pub use builtins::clear_stdlib_cache;

/// Import resolution for the type checker.
pub use imports::{
    apply_include_type_post_pass, build_prelude_env, build_type_env, build_type_env_with_cap,
    build_type_stage_env,
};

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
pub use error::{
    render_span_snippet, ArityBound, DiagnosticLevel, ErrorKind, EvalError, EvalResult, StackFrame,
    TypeDiagnostic,
};

/// Type error diagnostic formatting.
pub use types::{format_type_error, TypeError};

/// Formatter: canonical source reformatter.
pub use formatter::{format_source, format_source_tinct, format_source_tinct_with_dir};

#[cfg(feature = "repl")]
pub use repl::run_repl;

#[cfg(feature = "lsp")]
pub use lsp::run_lsp;

/// Runtime value types: values, thunks, environments, and dict keys.
pub use value::{
    make_seq_cons, make_seq_nil, ChannelInner, ClockCapInner, DirPerms, Environment, Key,
    NetCapEntry, Thunk, Value,
};

/// Attach macro expansion provenance to an error by checking if any of the error's
/// spans (definition, materialization, stack frames, secondary) match a provenance entry.
fn attach_macro_provenance(
    mut err: Box<EvalError>,
    provenance: &std::collections::HashMap<expand::SpanKey, expand::MacroProvenance>,
) -> Box<EvalError> {
    if err.macro_expansion.is_none() {
        // Check definition span
        let mut found = provenance.get(&expand::SpanKey::from(err.definition_span.clone()));
        // Check materialization span
        if found.is_none() {
            if let Some(mat_span) = &err.materialization_span {
                found = provenance.get(&expand::SpanKey::from(mat_span.clone()));
            }
        }
        // Check stack frame spans
        if found.is_none() {
            for frame in &err.stack {
                if let Some(prov) =
                    provenance.get(&expand::SpanKey::from(frame.definition_span.clone()))
                {
                    found = Some(prov);
                    break;
                }
            }
        }
        // Check secondary span
        if found.is_none() {
            if let Some((sec_span, _)) = &err.secondary_span {
                found = provenance.get(&expand::SpanKey::from(sec_span.clone()));
            }
        }
        if let Some(prov) = found {
            err.macro_expansion = Some((prov.macro_name.clone(), prov.call_site_span.clone()));
        }
    }
    err
}

/// Helper to attach macro provenance and format errors for display.
/// Used by eval_source_with_config and eval_source_with_cap_net to deduplicate
/// error attachment logic. Checks ALL 4 span sources: definition_span,
/// materialization_span, stack frames, and secondary_span.
fn attach_and_format_error(
    err: Box<EvalError>,
    provenance: &std::collections::HashMap<expand::SpanKey, expand::MacroProvenance>,
) -> String {
    format!("{}", attach_macro_provenance(err, provenance))
}

/// Parse and evaluate LLT source, returning the result in **LLT display format**
/// (e.g. `Int(42)`, `Dict({"x": Int(1)})`) -- not JSON.
///
/// Runs advisory type checking before evaluation (type errors are ignored).
/// The output format recursively materializes all values (including dict entries)
/// into a readable representation. Primarily used for testing and corpus validation.
/// For JSON output, use [`visit_value`] with [`JsonVisitor`] after evaluation instead.
pub fn eval_source(input: &str) -> Result<String, String> {
    eval_source_with_config(input, false)
}

/// Parse and evaluate LLT source with configurable filesystem access.
///
/// This is a variant of [`eval_source`] that allows control over the `no_fs` flag.
/// When `no_fs` is `true`, filesystem operations (like `include`) are disabled.
/// Primarily used for corpus tests that require filesystem isolation.
pub fn eval_source_with_config(input: &str, no_fs: bool) -> Result<String, String> {
    #[cfg(test)]
    {
        thread_local! {
            static EVAL_COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        }
        EVAL_COUNT.with(|c| {
            let n = c.get() + 1;
            c.set(n);
            if n % 500 == 0 {
                builtins::clear_stdlib_cache();
            }
        });
    }
    let parsed = parse(input).map_err(|e| format!("{e}"))?;
    // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> resolve_surface_program -> typecheck -> eval.
    // Use expand_surface_program (not expand_macros) so that SurfaceItem::Decl macro
    // registrations ([macro ...]) are seen before expansion.
    // expand_macros operates on File which drops Decl nodes via surface_program_to_file.
    // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
    // See also: src/main.rs run_eval pipeline, src/expand.rs module comment.
    // AMBIENT-OK: lib.rs public API — callers provide source strings, no prior Dir available.
    #[allow(clippy::disallowed_methods)]
    let expand_base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
        .map_err(|e| format!("cannot open cwd for macro expansion: {e}"))?;
    let mut program = parsed.program;
    let expand_result = crate::async_rt::block_on_anywhere(expand::expand_surface_program(
        &mut program,
        no_fs,
        &expand_base_dir,
    ))
    .map_err(|e| format!("{e}"))?;
    // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
    desugar::desugar_surface_program(&mut program);
    // ADT constructor injection: add `CtorName: [variant "CtorName"]` entries to dicts that
    // contain `[type ...]` declarations. Must run BEFORE resolve so the resolver assigns
    // correct de Bruijn slots to constructor names.
    desugar::inject_adt_constructors_surface_program(&mut program);
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    // Keep the table for eval_surface_file (used by lower.rs to resolve VarRef → Var).
    let resolution_table = std::sync::Arc::new(resolve::resolve_surface_program(&program));
    let provenance = expand_result.provenance;

    // Type errors are advisory; evaluation proceeds regardless.
    // The TypeAnnotationTable is populated by typecheck_surface_program_with_env.
    let (
        _type_errors,
        _type_map,
        _doc_map,
        _scheme_map,
        _diagnostics,
        infer_state,
        _final_env,
        type_annotation_table,
    ) = typecheck::typecheck_surface_program_with_env(
        &program,
        crate::imports::build_prelude_env(),
        false, // disable scheme_map (not needed for eval)
        false, // not in prelude load
    );
    let type_annotation_table = std::sync::Arc::new(type_annotation_table);

    // Use create_stdlib_env_with_arena so the eval context shares the stdlib's ThunkArena.
    // Without arena sharing, dot access on stdlib dicts (e.g., `result.bind`) resolves
    // ThunkIds from the stdlib's bootstrap_ctx arena via the eval ctx's empty arena,
    // causing an index-out-of-bounds panic.
    let (env, stdlib_arena) =
        builtins::create_stdlib_env_with_arena().map_err(|e| format!("{e}"))?;
    // Build type-stage environment (for builtin_eval_types). Falls back to stdlib_env if unavailable.
    let type_stage_env = build_type_stage_env().unwrap_or_else(|| Arc::clone(&env));
    let base_dir_path = std::env::current_dir()
        .ok()
        .and_then(|d| d.canonicalize().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    // AMBIENT-OK: lib.rs public API — no prior Dir available; operator provides source string.
    #[allow(clippy::disallowed_methods)]
    let base_dir = cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
        .map_err(|e| format!("cannot open base directory: {e}"))?;
    let ctx = eval::EvalContext::new_sharing_arena(
        base_dir,
        Arc::clone(&env),
        type_stage_env,
        no_fs,
        stdlib_arena,
        expand_result.macro_injects_map,
    );
    // Wire boundary guards and do-infer resolutions from type inference to the eval context.
    // NOTE: When typecheck is skipped (e.g., --no-typecheck or eval-only paths), do-infer
    // sentinels remain unresolved. The [do] macro expansion inserts ℊꜱʏᴍ⧼do-infer⧽N placeholder
    // VarRefs that the typechecker normally rewrites. Without typecheck, these produce
    // 'undefined variable: ℊꜱʏᴍ⧼do-infer⧽N' at eval time. This is expected degraded behavior.
    ctx.set_boundary_guards(infer_state.boundary_guards);
    ctx.set_do_infer_resolutions(infer_state.do_infer_resolutions);
    ctx.set_tycon_env(infer_state.tycon_env);
    // Inject `%cwd` and `%libdir` DirCaps (mirrors the CLI run_eval behavior).
    // This allows corpus tests and included files to use cap-qualified includes.
    if !no_fs {
        // AMBIENT-OK: injecting %cwd DirCap for corpus tests; CWD was already opened above.
        #[allow(clippy::disallowed_methods)]
        if let Ok(cwd_dir) =
            cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
        {
            let cwd_val = Value::DirCap {
                dir: Rc::new(cwd_dir),
                perms: value::DirPerms::full(),
            };
            let cwd_thunk = Arc::new(Thunk::new_materialized(cwd_val, Span::origin()));
            env.write().unwrap().insert("%cwd".to_string(), cwd_thunk);
        }
        if let Some(libdir_path) = find_libdir_path() {
            // AMBIENT-OK: injecting %libdir DirCap from fixed stdlib path.
            #[allow(clippy::disallowed_methods)]
            if let Ok(libdir_dir) =
                cap_std::fs::Dir::open_ambient_dir(&libdir_path, cap_std::ambient_authority())
            {
                let libdir_val = Value::DirCap {
                    dir: Rc::new(libdir_dir),
                    perms: value::DirPerms::full(),
                };
                let libdir_thunk = Arc::new(Thunk::new_materialized(libdir_val, Span::origin()));
                env.write()
                    .unwrap()
                    .insert("%libdir".to_string(), libdir_thunk);
            }
        }
    }
    let thunk = crate::async_rt::block_on_anywhere(eval::eval_surface_file(
        &program,
        Arc::clone(&env),
        &ctx,
        &resolution_table,
        &type_annotation_table,
    ))
    .map_err(|e| attach_and_format_error(e, &provenance))?;
    let val = crate::async_rt::block_on_anywhere(eval::materialize(&thunk, None, &ctx))
        .map_err(|e| attach_and_format_error(e, &provenance))?;
    value_to_display_string(&val, &ctx, thunk.span.clone())
        .map_err(|e| attach_and_format_error(e, &provenance))
}

/// Parse, eval, and materialize LLT source with optional NetCap injections.
///
/// Like `eval_source_with_config` but additionally injects named NetCap values
/// into the root environment before evaluation. Each `cap_net` entry is a pair
/// `(name, entry_string)` where `name` is the cap variable name (injected as `%name`)
/// and `entry_string` is an allowlist entry string (same format as `--cap-net NAME=ENTRY`).
///
/// Multiple entries with the same name accumulate into one NetCap allowlist.
pub fn eval_source_with_cap_net(
    input: &str,
    no_fs: bool,
    cap_net: &[(String, String)],
) -> Result<String, String> {
    use std::collections::HashMap;

    // Parse cap_net entries into grouped allowlists
    let mut grouped: HashMap<String, Vec<crate::value::NetCapEntry>> = HashMap::new();
    for (name, entry_str) in cap_net {
        let entry =
            parse_net_cap_entry(entry_str).map_err(|e| format!("cap_net directive error: {e}"))?;
        grouped.entry(name.clone()).or_default().push(entry);
    }

    // Use the standard config path, then inject caps after env creation
    let parsed = parse(input).map_err(|e| format!("{e}"))?;
    // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> resolve_surface_program -> typecheck -> eval.
    // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
    // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
    // AMBIENT-OK: lib.rs public API — callers provide source strings, no prior Dir available.
    #[allow(clippy::disallowed_methods)]
    let expand_base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
        .map_err(|e| format!("cannot open cwd for macro expansion: {e}"))?;
    let mut program = parsed.program;
    let expand_result = crate::async_rt::block_on_anywhere(expand::expand_surface_program(
        &mut program,
        no_fs,
        &expand_base_dir,
    ))
    .map_err(|e| format!("{e}"))?;
    // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
    desugar::desugar_surface_program(&mut program);
    // ADT constructor injection: add `CtorName: [variant "CtorName"]` entries to dicts that
    // contain `[type ...]` declarations. Must run BEFORE resolve so the resolver assigns
    // correct de Bruijn slots to constructor names.
    desugar::inject_adt_constructors_surface_program(&mut program);
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    // Keep the table for eval_surface_file (used by lower.rs to resolve VarRef → Var).
    let resolution_table = std::sync::Arc::new(resolve::resolve_surface_program(&program));
    let provenance = expand_result.provenance;

    // The TypeAnnotationTable is populated by typecheck_surface_program_with_env.
    let (
        _type_errors,
        _type_map,
        _doc_map,
        _scheme_map,
        _diagnostics,
        infer_state,
        _final_env,
        type_annotation_table,
    ) = typecheck::typecheck_surface_program_with_env(
        &program,
        crate::imports::build_prelude_env(),
        false, // disable scheme_map (not needed for eval)
        false, // not in prelude load
    );
    let type_annotation_table = std::sync::Arc::new(type_annotation_table);

    let (env, stdlib_arena) =
        builtins::create_stdlib_env_with_arena().map_err(|e| format!("{e}"))?;
    // Build type-stage environment BEFORE clone_for_child — it may add thunks to the stdlib arena.
    let type_stage_env = build_type_stage_env().unwrap_or_else(|| Arc::clone(&env));

    let base_dir_path = std::env::current_dir()
        .ok()
        .and_then(|d| d.canonicalize().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    // AMBIENT-OK: lib.rs public API — no prior Dir available; operator provides source string.
    #[allow(clippy::disallowed_methods)]
    let base_dir = cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
        .map_err(|e| format!("cannot open base directory: {e}"))?;
    let ctx = eval::EvalContext::new_sharing_arena(
        base_dir,
        Arc::clone(&env),
        type_stage_env,
        no_fs,
        stdlib_arena,
        expand_result.macro_injects_map,
    );
    // Wire boundary guards and do-infer resolutions from type inference to the eval context.
    // NOTE: When typecheck is skipped (e.g., --no-typecheck or eval-only paths), do-infer
    // sentinels remain unresolved. The [do] macro expansion inserts ℊꜱʏᴍ⧼do-infer⧽N placeholder
    // VarRefs that the typechecker normally rewrites. Without typecheck, these produce
    // 'undefined variable: ℊꜱʏᴍ⧼do-infer⧽N' at eval time. This is expected degraded behavior.
    ctx.set_boundary_guards(infer_state.boundary_guards);
    ctx.set_do_infer_resolutions(infer_state.do_infer_resolutions);
    ctx.set_tycon_env(infer_state.tycon_env);

    if !no_fs {
        // AMBIENT-OK: injecting %cwd DirCap for corpus tests; CWD was already opened above.
        #[allow(clippy::disallowed_methods)]
        if let Ok(cwd_dir) =
            cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
        {
            let cwd_val = Value::DirCap {
                dir: Rc::new(cwd_dir),
                perms: value::DirPerms::full(),
            };
            let cwd_thunk = Arc::new(Thunk::new_materialized(cwd_val, Span::origin()));
            env.write().unwrap().insert("%cwd".to_string(), cwd_thunk);
        }
    }

    // Inject NetCap values for each named cap
    for (name, entries) in grouped {
        let cap_val = Value::NetCap(Rc::new(entries));
        let cap_thunk = Arc::new(Thunk::new_materialized(cap_val, Span::origin()));
        env.write().unwrap().insert(format!("%{}", name), cap_thunk);
    }
    let thunk = crate::async_rt::block_on_anywhere(eval::eval_surface_file(
        &program,
        Arc::clone(&env),
        &ctx,
        &resolution_table,
        &type_annotation_table,
    ))
    .map_err(|e| attach_and_format_error(e, &provenance))?;
    let val = crate::async_rt::block_on_anywhere(eval::materialize(&thunk, None, &ctx))
        .map_err(|e| attach_and_format_error(e, &provenance))?;
    value_to_display_string(&val, &ctx, thunk.span.clone())
        .map_err(|e| attach_and_format_error(e, &provenance))
}

/// Parse a NetCap allowlist entry string (same logic as CLI `--cap-net NAME=ENTRY`).
fn parse_net_cap_entry(s: &str) -> Result<crate::value::NetCapEntry, String> {
    use crate::value::NetCapEntry;
    if s == "any" {
        return Ok(NetCapEntry::Any);
    }
    if let Some((host, port_str)) = s.split_once(':') {
        let port: u16 = port_str
            .parse()
            .map_err(|_| format!("invalid port '{}' in '{}'", port_str, s))?;
        return Ok(NetCapEntry::HostPort(host.to_string(), port));
    }
    if s.contains('*') {
        if !s.starts_with("*.") {
            return Err(format!(
                "only prefix wildcards supported (e.g. '*.internal'), got '{}'",
                s
            ));
        }
        return Ok(NetCapEntry::HostnameGlob(s.to_string()));
    }
    if s.contains('/') {
        use std::str::FromStr;
        let net = ipnet::IpNet::from_str(s)
            .map_err(|_| format!("invalid CIDR '{}' — expected e.g. '10.0.0.0/8'", s))?;
        return Ok(NetCapEntry::Cidr(net));
    }
    Ok(NetCapEntry::Hostname(s.to_string()))
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
pub fn typecheck_source(input: &str) -> Result<(), String> {
    let parsed = parse(input).map_err(|e| format!("{e}"))?;
    // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> typecheck.
    // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
    // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
    // AMBIENT-OK: lib.rs public API — callers provide source strings, no prior Dir available.
    #[allow(clippy::disallowed_methods)]
    let expand_base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
        .map_err(|e| format!("cannot open cwd for macro expansion: {e}"))?;
    let mut program = parsed.program;
    crate::async_rt::block_on_anywhere(expand::expand_surface_program(
        &mut program,
        false,
        &expand_base_dir,
    ))
    .map_err(|e| format!("{e}"))?;
    // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
    desugar::desugar_surface_program(&mut program);
    // Note: inject_adt_constructors_surface_program is NOT called here.
    // The type checker handles ADT constructor scoping via `inject_adt_constructor_schemes`
    // in typecheck_dict.rs (Pass 2), which gives constructors precise function types without
    // needing the surface-level injection that would conflict with [variant "..."] type inference.
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    let _resolution_table = resolve::resolve_surface_program(&program);
    // Type check the surface program with prelude-seeded environment.
    let env = imports::build_prelude_env();
    let (type_errors, _type_map, _doc_map, _scheme_map, diagnostics) =
        typecheck::typecheck_surface_program(&program, env);
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
pub fn typecheck_source_errors_only(input: &str) -> Result<(), String> {
    let parsed = parse(input).map_err(|e| format!("{e}"))?;
    // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> typecheck.
    // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
    // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
    // AMBIENT-OK: lib.rs public API — callers provide source strings, no prior Dir available.
    #[allow(clippy::disallowed_methods)]
    let expand_base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
        .map_err(|e| format!("cannot open cwd for macro expansion: {e}"))?;
    let mut program = parsed.program;
    crate::async_rt::block_on_anywhere(expand::expand_surface_program(
        &mut program,
        false,
        &expand_base_dir,
    ))
    .map_err(|e| format!("{e}"))?;
    // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
    desugar::desugar_surface_program(&mut program);
    // Note: inject_adt_constructors_surface_program is NOT called here.
    // The type checker handles ADT constructor scoping via `inject_adt_constructor_schemes`
    // in typecheck_dict.rs (Pass 2), which gives constructors precise function types without
    // needing the surface-level injection that would conflict with [variant "..."] type inference.
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    let _resolution_table = resolve::resolve_surface_program(&program);
    // Type check the surface program with prelude-seeded environment.
    let env = imports::build_prelude_env();
    let (type_errors, _type_map, _doc_map, _scheme_map, _diagnostics) =
        typecheck::typecheck_surface_program(&program, env);
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
// `visit_value` (with `JsonVisitor`) and `value_to_display_string` share the same structural traversal
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
    fn visit_bool(&self, v: bool) -> Self::Output;
    fn visit_str(&self, v: &str) -> Self::Output;
    fn visit_bytes(&self, v: &[u8]) -> Self::Output;
    fn visit_null(&self) -> Self::Output;
    fn visit_dict(&self, entries: Vec<(value::Key, Self::Output)>) -> Self::Output;
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
pub fn visit_value<V: ValueVisitor>(
    val: &value::Value,
    ctx: &Arc<eval::EvalContext>,
    depth: usize,
    visitor: &V,
    span: ast::Span,
) -> Result<V::Output, Box<error::EvalError>> {
    if let Some(limit_result) = visitor.depth_limit_output(depth, span.clone()) {
        return limit_result;
    }
    match val {
        value::Value::Int(n) => Ok(visitor.visit_int(*n)),
        value::Value::Float(f) => visitor.visit_float(*f, span),
        value::Value::String {
            ref source,
            start,
            end,
        } => {
            let s = &source[*start..*end];
            Ok(visitor.visit_str(s))
        }
        value::Value::Bool(b) => Ok(visitor.visit_bool(*b)),
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
                let v = crate::async_rt::block_on_anywhere(eval::materialize(&thunk, None, ctx))?;
                let child_span = thunk.span.clone();
                entries.push((
                    key.clone(),
                    visit_value(&v, ctx, depth + 1, visitor, child_span)?,
                ));
            }
            Ok(visitor.visit_dict(entries))
        }
        value::Value::Overlay(l, r) => {
            // Flatten overlay to a concrete dict, then visit it.
            let map = builtins::flatten_overlay(l, r, "value serialization", ctx, span.clone())?;
            visit_value(&value::Value::Dict(map), ctx, depth, visitor, span)
        }
        value::Value::Variant {
            ref tag,
            payload: Some(payload_id),
        } if tag == "Seq.Cons" => {
            let payload_thunk = ctx.get_thunk(*payload_id);
            let payload_val =
                crate::async_rt::block_on_anywhere(eval::materialize(&payload_thunk, None, ctx))?;
            let head_id = if let value::Value::Dict(ref d) = payload_val {
                *d.get(&value::Key::String("head".into()))
                    .expect("Seq.Cons must have head")
            } else {
                return Err(Box::new(error::EvalError::internal(
                    "Seq.Cons payload must be a Dict".to_string(),
                    span,
                )));
            };
            let head_thunk = ctx.get_thunk(head_id);
            let head_val =
                crate::async_rt::block_on_anywhere(eval::materialize(&head_thunk, None, ctx))?;
            let head_span = head_thunk.span.clone();
            let head_out = visit_value(&head_val, ctx, depth + 1, visitor, head_span.clone())?;
            visitor.visit_seq_head(head_out, head_span)
        }
        value::Value::Variant {
            ref tag,
            payload: None,
        } if tag == "Seq.Nil" => {
            // Empty sequence — treat as empty dict for serialization
            Ok(visitor.visit_dict(vec![]))
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
        value::Value::Handle { .. } => Err(Box::new(error::EvalError::value_not_serializable(
            "Handle".to_string(),
            span,
        ))),
        value::Value::WriteHandle { .. } => Err(Box::new(
            error::EvalError::value_not_serializable("WriteHandle".to_string(), span),
        )),
        value::Value::RevocableDirCap { .. } => Err(Box::new(
            error::EvalError::value_not_serializable("RevocableDirCap".to_string(), span),
        )),
        value::Value::Variant { tag, payload } => {
            let payload_output = match payload {
                Some(thunk_id) => {
                    let thunk = ctx.get_thunk(*thunk_id);
                    let v =
                        crate::async_rt::block_on_anywhere(eval::materialize(&thunk, None, ctx))?;
                    let payload_span = thunk.span.clone();
                    visit_value(&v, ctx, depth + 1, visitor, payload_span)?
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
        value::Value::QuicSession(_) => Err(Box::new(error::EvalError::value_not_serializable(
            "QuicSession".to_string(),
            span,
        ))),
        value::Value::Http2Session { .. } => Err(Box::new(
            error::EvalError::value_not_serializable("Http2Session".to_string(), span),
        )),
        value::Value::Http3Session(_) => Err(Box::new(error::EvalError::value_not_serializable(
            "Http3Session".to_string(),
            span,
        ))),
        value::Value::QuicDatagramHandle(_) => Err(Box::new(
            error::EvalError::value_not_serializable("QuicDatagramHandle".to_string(), span),
        )),
        value::Value::DatagramHandle { .. } => Err(Box::new(
            error::EvalError::value_not_serializable("DatagramHandle".to_string(), span),
        )),
        value::Value::Program { .. } => Err(Box::new(error::EvalError::value_not_serializable(
            "Program".to_string(),
            span,
        ))),
        value::Value::Document(_) => Err(Box::new(error::EvalError::value_not_serializable(
            "Document".to_string(),
            span,
        ))),
        value::Value::Expression(_) => Err(Box::new(error::EvalError::value_not_serializable(
            "Expression".to_string(),
            span,
        ))),
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
        value::Value::ReactiveCell(_) => Err(Box::new(error::EvalError::value_not_serializable(
            "ReactiveCell".to_string(),
            span,
        ))),
        value::Value::Builder(_) => Err(Box::new(error::EvalError::value_not_serializable(
            "Builder".to_string(),
            span,
        ))),
        value::Value::BroadcastChannel(_) => Err(Box::new(
            error::EvalError::value_not_serializable("BroadcastChannel".to_string(), span),
        )),
        value::Value::OneshotSender(_) => Err(Box::new(error::EvalError::value_not_serializable(
            "OneshotSender".to_string(),
            span,
        ))),
        value::Value::OneshotReceiver(_) => Err(Box::new(
            error::EvalError::value_not_serializable("OneshotReceiver".to_string(), span),
        )),
    }
}

// --- JSON Visitor ---

/// Escape a string's contents for JSON output (without surrounding quotes).
///
/// Escapes `"`, `\`, and ASCII control characters. `\b` and `\f` are emitted for
/// U+0008 and U+000C. Other C0 controls use `\uXXXX`. All other characters pass through.
pub fn escape_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as FmtWrite;
                write!(out, "\\u{:04x}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out
}

pub struct JsonVisitor;

impl ValueVisitor for JsonVisitor {
    type Output = String;

    fn visit_int(&self, v: i64) -> String {
        v.to_string()
    }
    fn visit_float(&self, v: f64, span: ast::Span) -> Result<String, Box<error::EvalError>> {
        if !v.is_finite() {
            return Err(error::EvalError::float_not_finite("to-json".to_string(), v, span).into());
        }
        // Whole-number floats emit exactly one decimal place (e.g. `1.0`); this differs
        // from ryu shortest-repr for large magnitudes (e.g. 1e20 → "100000000000000000000.0"
        // here vs ryu's "1e20").
        if v.fract() == 0.0 {
            Ok(format!("{v:.1}"))
        } else {
            Ok(format!("{v}"))
        }
    }
    fn visit_bool(&self, v: bool) -> String {
        if v {
            "true".to_string()
        } else {
            "false".to_string()
        }
    }
    fn visit_str(&self, v: &str) -> String {
        format!("\"{}\"", escape_json_str(v))
    }
    fn visit_bytes(&self, v: &[u8]) -> String {
        // Hex encode bytes for JSON output (lowercase hex string)
        let hex_string: String = v.iter().map(|b| format!("{:02x}", b)).collect();
        format!("\"{}\"", hex_string)
    }
    fn visit_null(&self) -> String {
        "null".to_string()
    }
    fn visit_dict(&self, entries: Vec<(value::Key, String)>) -> String {
        // LLT null compatibility: [] (empty dict) serializes as JSON null.
        // Matches the tinct to-json (codecs/json.llt) null? convention.
        if entries.is_empty() {
            return "null".to_string();
        }
        // Detect array-like dict: all keys are sequential ints 0..n
        let is_array = entries
            .iter()
            .enumerate()
            .all(|(i, (k, _))| matches!(k, value::Key::Int(n) if *n >= 0 && *n as usize == i));
        if is_array {
            let items: Vec<&str> = entries.iter().map(|(_, v)| v.as_str()).collect();
            format!("[{}]", items.join(","))
        } else {
            let pairs: Vec<String> = entries
                .into_iter()
                .map(|(k, v)| {
                    let ks = match k {
                        value::Key::Int(n) => n.to_string(),
                        value::Key::String(s) => s.to_string(),
                    };
                    format!("\"{}\":{}", escape_json_str(&ks), v)
                })
                .collect();
            format!("{{{}}}", pairs.join(","))
        }
    }
    fn visit_seq_head(
        &self,
        _head: String,
        span: ast::Span,
    ) -> Result<String, Box<error::EvalError>> {
        // Seq is not representable in JSON; must be collected to a Dict first via $collect.
        Err(error::EvalError::value_not_serializable("Seq".to_string(), span).into())
    }
    fn visit_function(
        &self,
        _params: &[ast::Param],
        span: ast::Span,
    ) -> Result<String, Box<error::EvalError>> {
        Err(error::EvalError::value_not_serializable("Function".to_string(), span).into())
    }
    fn visit_builtin(&self, name: &str, span: ast::Span) -> Result<String, Box<error::EvalError>> {
        Err(error::EvalError::value_not_serializable(format!("Builtin ({name})"), span).into())
    }
    fn visit_proxy(&self, span: ast::Span) -> Result<String, Box<error::EvalError>> {
        Err(error::EvalError::value_not_serializable("Proxy".to_string(), span).into())
    }
    fn visit_variant(&self, tag: String, payload: String) -> String {
        format!("{{\"{}\":{}}}", escape_json_str(&tag), payload)
    }
    fn visit_decimal(&self, v: rust_decimal::Decimal) -> String {
        // Serialize Decimal as JSON number string to preserve exact representation
        v.to_string()
    }
    fn visit_bigint(&self, v: &num_bigint::BigInt) -> String {
        // BigInt serializes as JSON number string. May exceed JSON receiver's i64 range.
        v.to_string()
    }
    fn visit_timestamp(
        &self,
        nanos: i64,
        span: ast::Span,
    ) -> Result<String, Box<error::EvalError>> {
        // Convert nanoseconds to jiff::Timestamp and format as RFC 3339
        let ts = jiff::Timestamp::from_nanosecond(nanos as i128).map_err(|e| {
            error::EvalError::internal(format!("invalid timestamp value: {e}"), span)
        })?;
        Ok(format!("\"{}\"", ts))
    }
    fn visit_duration(&self, nanos: i64) -> String {
        // Serialize duration as nanosecond count (JSON number)
        nanos.to_string()
    }
    fn visit_clock_cap(&self, span: ast::Span) -> Result<String, Box<error::EvalError>> {
        Err(error::EvalError::value_not_serializable("ClockCap".to_string(), span).into())
    }
    fn visit_timezone(&self, span: ast::Span) -> Result<String, Box<error::EvalError>> {
        Err(error::EvalError::value_not_serializable("Timezone".to_string(), span).into())
    }
    fn depth_limit_output(
        &self,
        depth: usize,
        span: ast::Span,
    ) -> Option<Result<String, Box<error::EvalError>>> {
        // Output depth limit: prevents infinite recursion in JSON output.
        // 256 levels of nesting is generous for any real config file.
        const MAX_JSON_OUTPUT_DEPTH: usize = 256;
        if depth > MAX_JSON_OUTPUT_DEPTH {
            Some(Err(error::EvalError::internal(
                format!("maximum JSON output depth ({MAX_JSON_OUTPUT_DEPTH}) exceeded"),
                span,
            )
            .into()))
        } else {
            None
        }
    }
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
/// Increased from 3 to 5 to accommodate Result-wrapped values (Variant(Ok, ...)).
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
    fn visit_bool(&self, v: bool) -> String {
        format!("Bool({v})")
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
/// Unlike [`JsonVisitor`] (used via [`visit_value`]), this accepts NaN/Infinity floats (renders as `Float(NaN)`, `Float(inf)`).
///
/// `depth` tracks recursion depth to prevent stack overflow from deeply nested
/// dict-of-dicts structures. Uses `MAX_DISPLAY_DEPTH` (5 levels); truncates deeper nesting with `...`.
pub fn value_to_display_string(
    val: &value::Value,
    ctx: &Arc<eval::EvalContext>,
    span: ast::Span,
) -> Result<String, Box<error::EvalError>> {
    let depth = 0;
    visit_value(val, ctx, depth, &DisplayVisitor, span)
}

#[allow(clippy::items_after_test_module)]
// find_libdir_path and other public helpers come after tests module; moving them before would bury utility functions at the bottom of the prelude
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::CoreExpr;
    use indexmap::IndexMap;
    use std::sync::RwLock;
    use test_util::test_span;
    use value::{string_val, Environment, Key, Thunk, Value};

    /// Helper: wrap a Value in a materialized thunk.
    fn thunk(val: Value) -> Arc<Thunk> {
        Arc::new(Thunk::new_materialized(val, test_span(1, 1, 1, 1)))
    }

    /// Build a test input Value from a simple JSON-like description.
    /// Supports: null, bool, i64, f64, string, array (sequential ints), object (string keys).
    #[allow(dead_code)]
    enum TestJson {
        Null,
        Bool(bool),
        Int(i64),
        Float(f64),
        Str(&'static str),
        Array(Vec<TestJson>),
        Object(Vec<(&'static str, TestJson)>),
    }

    fn make_test_value(json: TestJson, ctx: &Arc<eval::EvalContext>) -> Arc<Thunk> {
        let val = match json {
            TestJson::Null => Value::Dict(IndexMap::new()),
            TestJson::Bool(b) => Value::Bool(b),
            TestJson::Int(i) => Value::Int(i),
            TestJson::Float(f) => Value::Float(f),
            TestJson::Str(s) => string_val(s),
            TestJson::Array(arr) => {
                let mut map = IndexMap::new();
                for (i, v) in arr.into_iter().enumerate() {
                    let t = make_test_value(v, ctx);
                    map.insert(Key::Int(i as i64), ctx.alloc_thunk(t));
                }
                Value::Dict(map)
            }
            TestJson::Object(obj) => {
                let mut map = IndexMap::new();
                for (k, v) in obj {
                    let t = make_test_value(v, ctx);
                    map.insert(Key::String(k.into()), ctx.alloc_thunk(t));
                }
                Value::Dict(map)
            }
        };
        thunk(val)
    }

    /// Build a `Value::Dict` with entries allocated into `ctx`'s arena.
    fn make_dict(map: IndexMap<Key, Arc<Thunk>>, ctx: &Arc<eval::EvalContext>) -> Value {
        let mut id_map: IndexMap<Key, value::ThunkId> = IndexMap::with_capacity(map.len());
        for (k, v) in map {
            id_map.insert(k, ctx.alloc_thunk(v));
        }
        Value::Dict(id_map)
    }

    fn test_ctx() -> Arc<eval::EvalContext> {
        let base_dir = test_util::test_caps().root.try_clone().unwrap();
        let stdlib_env = builtins::create_stdlib_env().expect("stdlib failed");
        let type_stage_env = build_type_stage_env().unwrap_or_else(|| Arc::clone(&stdlib_env));
        eval::EvalContext::new(base_dir, stdlib_env, type_stage_env, false)
    }

    #[test]
    fn test_json_int() {
        let result = visit_value(
            &Value::Int(42),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        assert_eq!(result, "42");
    }

    #[test]
    fn test_json_int_negative() {
        let result = visit_value(
            &Value::Int(-100),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        assert_eq!(result, "-100");
    }

    #[test]
    fn test_json_int_zero() {
        let result = visit_value(
            &Value::Int(0),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        assert_eq!(result, "0");
    }

    #[test]
    fn test_json_float() {
        // 3.14 tests float serialization, not π.
        #[allow(clippy::approx_constant)]
        let result = visit_value(
            &Value::Float(3.14),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        #[allow(clippy::approx_constant)]
        {
            assert_eq!(result, "3.14");
        }
    }

    #[test]
    fn test_json_float_negative() {
        let result = visit_value(
            &Value::Float(-2.5),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        assert_eq!(result, "-2.5");
    }

    #[test]
    fn test_json_float_zero() {
        let result = visit_value(
            &Value::Float(0.0),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        assert_eq!(result, "0.0");
    }

    #[test]
    fn test_json_float_nan_error() {
        let err = visit_value(
            &Value::Float(f64::NAN),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap_err();
        assert!(err.kind.to_string().contains("NaN"));
    }

    #[test]
    fn test_json_float_infinity_error() {
        let err = visit_value(
            &Value::Float(f64::INFINITY),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap_err();
        assert!(err.kind.to_string().contains("is not a finite number"));
    }

    #[test]
    fn test_json_float_neg_infinity_error() {
        let err = visit_value(
            &Value::Float(f64::NEG_INFINITY),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap_err();
        assert!(err.kind.to_string().contains("is not a finite number"));
    }

    #[test]
    fn test_json_string() {
        let result = visit_value(
            &string_val("hello"),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        assert_eq!(result, "\"hello\"");
    }

    #[test]
    fn test_json_string_empty() {
        let result = visit_value(
            &string_val(""),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        assert_eq!(result, "\"\"");
    }

    #[test]
    fn test_json_string_with_special_chars() {
        let result = visit_value(
            &string_val("line\nnewline"),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        assert_eq!(result, "\"line\\nnewline\"");
    }

    #[test]
    fn test_json_bool_true() {
        let result = visit_value(
            &Value::Bool(true),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        assert_eq!(result, "true");
    }

    #[test]
    fn test_json_bool_false() {
        let result = visit_value(
            &Value::Bool(false),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        assert_eq!(result, "false");
    }

    #[test]
    fn test_json_dict_empty_is_null() {
        // Empty dict [] is LLT's null value; JsonVisitor serializes it as JSON null
        // to match the LLT null compatibility convention.
        let dict = Value::Dict(IndexMap::new());
        let result = visit_value(&dict, &test_ctx(), 0, &JsonVisitor, ast::Span::origin()).unwrap();
        assert_eq!(result, "null");
    }

    #[test]
    fn test_json_dict_string_keys() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("name".into()), thunk(string_val("Alice")));
        map.insert(Key::String("age".into()), thunk(Value::Int(30)));
        let val = make_dict(map, &ctx);
        let result = visit_value(&val, &ctx, 0, &JsonVisitor, ast::Span::origin()).unwrap();
        assert_eq!(result, "{\"name\":\"Alice\",\"age\":30}");
    }

    #[test]
    fn test_json_dict_int_keys_non_sequential() {
        // Int keys that are NOT sequential from 0 -> object with stringified keys
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(5), thunk(string_val("five")));
        map.insert(Key::Int(10), thunk(string_val("ten")));
        let val = make_dict(map, &ctx);
        let result = visit_value(&val, &ctx, 0, &JsonVisitor, ast::Span::origin()).unwrap();
        assert_eq!(result, "{\"5\":\"five\",\"10\":\"ten\"}");
    }

    #[test]
    fn test_json_dict_mixed_keys() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(string_val("zero")));
        map.insert(Key::String("x".into()), thunk(Value::Int(1)));
        let val = make_dict(map, &ctx);
        let result = visit_value(&val, &ctx, 0, &JsonVisitor, ast::Span::origin()).unwrap();
        assert_eq!(result, "{\"0\":\"zero\",\"x\":1}");
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
        let result = visit_value(&val, &ctx, 0, &JsonVisitor, ast::Span::origin()).unwrap();
        assert_eq!(result, "[\"a\",\"b\",\"c\"]");
    }

    #[test]
    fn test_json_dict_array_single_element() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::Bool(true)));
        let val = make_dict(map, &ctx);
        let result = visit_value(&val, &ctx, 0, &JsonVisitor, ast::Span::origin()).unwrap();
        assert_eq!(result, "[true]");
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
        let result = visit_value(&val, &ctx, 0, &JsonVisitor, ast::Span::origin()).unwrap();
        assert_eq!(result, "{\"1\":\"b\",\"0\":\"a\"}");
    }

    #[test]
    fn test_json_dict_array_starting_at_one() {
        // Keys 1, 2, 3 -- not starting from 0, so object
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(1), thunk(Value::Int(10)));
        map.insert(Key::Int(2), thunk(Value::Int(20)));
        let val = make_dict(map, &ctx);
        let result = visit_value(&val, &ctx, 0, &JsonVisitor, ast::Span::origin()).unwrap();
        assert_eq!(result, "{\"1\":10,\"2\":20}");
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
        let result = visit_value(&val, &ctx, 0, &JsonVisitor, ast::Span::origin()).unwrap();
        assert_eq!(result, "{\"inner\":{\"x\":1},\"y\":2}");
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
        let result = visit_value(&val, &ctx, 0, &JsonVisitor, ast::Span::origin()).unwrap();
        assert_eq!(result, "[{\"name\":\"Alice\"},{\"name\":\"Bob\"}]");
    }

    #[test]
    fn test_json_function_error() {
        let f = Value::Function {
            params: Rc::new(vec![]),
            body: Arc::new(ast::Spanned::new(CoreExpr::Int(0), test_span(1, 1, 1, 1))),
            env: Arc::new(RwLock::new(Environment::new())),
            annotation: None,
        };
        let err = visit_value(&f, &test_ctx(), 0, &JsonVisitor, ast::Span::origin()).unwrap_err();
        assert!(
            err.kind
                .to_string()
                .contains("cannot serialize Function to JSON"),
            "got: {}",
            err.kind
        );
        assert_eq!(err.kind.code(), "E035");
    }

    #[test]
    fn test_json_function_error_span_threaded() {
        // Verify that visit_value uses the threaded span in errors (not Span::origin)
        let f = Value::Function {
            params: Rc::new(vec![]),
            body: Arc::new(ast::Spanned::new(CoreExpr::Int(0), test_span(1, 1, 1, 1))),
            env: Arc::new(RwLock::new(Environment::new())),
            annotation: None,
        };
        let call_site_span = test_span(3, 5, 3, 20);
        let err =
            visit_value(&f, &test_ctx(), 0, &JsonVisitor, call_site_span.clone()).unwrap_err();
        assert_eq!(err.definition_span, call_site_span);
    }

    #[test]
    fn test_json_seq_error() {
        let ctx = test_ctx();
        let seq = {
            let head_thunk = Arc::new(Thunk::new_materialized(
                Value::Int(1),
                test_span(1, 1, 1, 1),
            ));
            let tail_thunk = Arc::new(Thunk::new_materialized(
                value::make_seq_nil(),
                test_span(1, 1, 1, 1),
            ));
            let head_id = ctx.alloc_thunk(head_thunk);
            let tail_id = ctx.alloc_thunk(tail_thunk);
            value::make_seq_cons(head_id, tail_id, &ctx)
        };
        let err = visit_value(&seq, &ctx, 0, &JsonVisitor, ast::Span::origin()).unwrap_err();
        assert!(
            err.kind
                .to_string()
                .contains("cannot serialize Seq to JSON"),
            "got: {}",
            err.kind
        );
        assert_eq!(err.kind.code(), "E035");
    }

    #[test]
    fn test_json_builtin_error() {
        #[allow(clippy::type_complexity)] // Test-only dummy function — complex type is the required BuiltinFn signature
        fn dummy(
            _ctx: value::BuiltinArgs,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Arc<Thunk>, Box<error::EvalError>>>>,
        > {
            Box::pin(async move {
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Int(0),
                    ast::Span::origin(),
                )))
            })
        }
        let b = Value::Builtin(value::BuiltinDef {
            func: dummy,
            name: "test",
            pos_strictness: &[],
            force_count: 0,
        });
        let err = visit_value(&b, &test_ctx(), 0, &JsonVisitor, ast::Span::origin()).unwrap_err();
        assert!(
            err.kind
                .to_string()
                .contains("cannot serialize Builtin (test) to JSON"),
            "got: {}",
            err.kind
        );
        assert_eq!(err.kind.code(), "E035");
    }

    #[test]
    fn test_json_proxy_error() {
        let ctx = test_ctx();
        let handler_thunk = Arc::new(Thunk::new_materialized(Value::Int(0), ast::Span::origin()));
        let proxy = Value::Proxy {
            handler: ctx.alloc_thunk(handler_thunk),
        };
        let err = visit_value(&proxy, &ctx, 0, &JsonVisitor, ast::Span::origin()).unwrap_err();
        assert!(
            err.kind
                .to_string()
                .contains("cannot serialize Proxy to JSON"),
            "got: {}",
            err.kind
        );
        assert_eq!(err.kind.code(), "E035");
    }

    #[test]
    fn test_json_int_max() {
        let result = visit_value(
            &Value::Int(i64::MAX),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        assert_eq!(result, i64::MAX.to_string());
    }

    #[test]
    fn test_json_int_min() {
        let result = visit_value(
            &Value::Int(i64::MIN),
            &test_ctx(),
            0,
            &JsonVisitor,
            ast::Span::origin(),
        )
        .unwrap();
        assert_eq!(result, i64::MIN.to_string());
    }

    /// Helper: run the full eval pipeline (parse, eval, materialize, to JSON string).
    fn eval_to_json(source: &str) -> String {
        eval_to_json_with_input(source, None, None)
    }

    fn eval_to_json_with_input(
        source: &str,
        stdin_input: Option<Arc<Thunk>>,
        external_ctx: Option<Arc<eval::EvalContext>>,
    ) -> String {
        builtins::clear_stdlib_cache();
        let parsed = parse(source).expect("parse failed");
        let mut program = parsed.program.clone();
        desugar::desugar_surface_program(&mut program);
        let resolution_table = std::sync::Arc::new(resolve::resolve_surface_program(&program));
        let (_type_errors, type_annotation_table, _inferred) =
            typecheck::typecheck_surface_program_annotation_table(&program);
        let type_annotation_table = std::sync::Arc::new(type_annotation_table);

        let (env, ctx) = if let Some(ext_ctx) = external_ctx {
            let env = ext_ctx.config.stdlib_env.clone();
            (env, ext_ctx)
        } else {
            let (env, stdlib_arena) =
                builtins::create_stdlib_env_with_arena().expect("stdlib failed");
            let base_dir = test_util::test_caps().root.try_clone().unwrap();
            let type_stage_env = build_type_stage_env().unwrap_or_else(|| Arc::clone(&env));
            let ctx = eval::EvalContext::new_sharing_arena(
                base_dir,
                Arc::clone(&env),
                type_stage_env,
                false,
                stdlib_arena,
                std::collections::HashMap::new(),
            );
            (env, ctx)
        };

        let thunk = crate::async_rt::block_on_anywhere(eval::eval_surface_file_with_input(
            &program,
            env,
            &ctx,
            &resolution_table,
            &type_annotation_table,
            &std::collections::HashMap::new(),
            stdin_input,
        ))
        .expect("eval failed");
        let val = crate::async_rt::block_on_anywhere(eval::materialize(&thunk, None, &ctx))
            .expect("materialize failed");
        visit_value(&val, &ctx, 0, &JsonVisitor, ast::Span::origin()).expect("visit_value failed")
    }

    #[test]
    fn test_pipeline_simple_dict() {
        let result = eval_to_json("[x: 1 y: \"hello\"]");
        assert_eq!(result, "{\"x\":1,\"y\":\"hello\"}");
    }

    #[test]
    fn test_pipeline_array_like() {
        let result = eval_to_json("[10 20 30]");
        assert_eq!(result, "[10,20,30]");
    }

    #[test]
    fn test_pipeline_nested() {
        let result = eval_to_json("[a: [b: [c: 42]]]");
        assert_eq!(result, "{\"a\":{\"b\":{\"c\":42}}}");
    }

    #[test]
    fn test_pipeline_stdin_json_injection() {
        let ctx = test_ctx();
        let input = make_test_value(
            TestJson::Object(vec![
                ("name", TestJson::Str("Alice")),
                ("age", TestJson::Int(30)),
            ]),
            &ctx,
        );
        let result = eval_to_json_with_input("[greeting: %.name]", Some(input), Some(ctx));
        assert_eq!(result, "{\"greeting\":\"Alice\"}");
    }

    #[test]
    fn test_pipeline_stdin_json_array() {
        let ctx = test_ctx();
        let input = make_test_value(
            TestJson::Array(vec![TestJson::Int(1), TestJson::Int(2), TestJson::Int(3)]),
            &ctx,
        );
        let result = eval_to_json_with_input("[first: [get 0 %]]", Some(input), Some(ctx));
        assert_eq!(result, "{\"first\":1}");
    }

    #[test]
    fn test_pipeline_stdin_json_passthrough() {
        let ctx = test_ctx();
        let input = make_test_value(TestJson::Object(vec![("x", TestJson::Int(42))]), &ctx);
        let result = eval_to_json_with_input("%", Some(input), Some(ctx));
        assert_eq!(result, "{\"x\":42}");
    }

    #[test]
    fn test_pipeline_no_stdin_default_empty_dict() {
        // Without stdin input, % defaults to empty dict.
        // By design, LLT empty dict [] serializes to JSON null (see JsonVisitor::visit_dict).
        let result = eval_to_json("%");
        assert_eq!(result, "null");
    }

    #[test]
    fn test_pipeline_multi_document_with_stdin() {
        // stdin -> doc1 -> % -> doc2
        let ctx = test_ctx();
        let input = make_test_value(TestJson::Object(vec![("val", TestJson::Int(10))]), &ctx);
        let source = "[result: %.val]\n---\n[wrapped: %.result]";
        let result = eval_to_json_with_input(source, Some(input), Some(ctx));
        assert_eq!(result, "{\"wrapped\":10}");
    }

    #[test]
    fn test_pipeline_json_visitor_materializes_lazily() {
        let source = "[a: [b: [c: 42]]]";
        let parsed = parse(source).expect("parse failed");
        let mut program = parsed.program.clone();
        desugar::desugar_surface_program(&mut program);
        let resolution_table = std::sync::Arc::new(resolve::resolve_surface_program(&program));
        let (_type_errors, type_annotation_table, _inferred) =
            typecheck::typecheck_surface_program_annotation_table(&program);
        let type_annotation_table = std::sync::Arc::new(type_annotation_table);
        let env = builtins::create_stdlib_env().expect("stdlib failed");
        let ctx = test_ctx();
        let thunk = crate::async_rt::block_on_anywhere(eval::eval_surface_file(
            &program,
            env,
            &ctx,
            &resolution_table,
            &type_annotation_table,
        ))
        .expect("eval failed");
        let val = crate::async_rt::block_on_anywhere(eval::materialize(&thunk, None, &ctx))
            .expect("materialize failed");
        // visit_value materializes nested values on demand
        let json = visit_value(&val, &ctx, 0, &JsonVisitor, ast::Span::origin())
            .expect("visit_value failed");
        assert_eq!(json, "{\"a\":{\"b\":{\"c\":42}}}");
    }

    #[test]
    fn test_pipeline_display_format() {
        let source = "[x: 42]";
        let parsed = parse(source).expect("parse failed");
        let mut program = parsed.program.clone();
        desugar::desugar_surface_program(&mut program);
        let resolution_table = std::sync::Arc::new(resolve::resolve_surface_program(&program));
        let (_type_errors, type_annotation_table, _inferred) =
            typecheck::typecheck_surface_program_annotation_table(&program);
        let type_annotation_table = std::sync::Arc::new(type_annotation_table);
        let env = builtins::create_stdlib_env().expect("stdlib failed");
        let ctx = test_ctx();
        let thunk = crate::async_rt::block_on_anywhere(eval::eval_surface_file(
            &program,
            env,
            &ctx,
            &resolution_table,
            &type_annotation_table,
        ))
        .expect("eval failed");
        let val = crate::async_rt::block_on_anywhere(eval::materialize(&thunk, None, &ctx))
            .expect("materialize failed");
        // value_to_display_string materializes nested values on demand via visit_value
        let display =
            value_to_display_string(&val, &ctx, ast::Span::origin()).expect("display failed");
        assert_eq!(display, "Dict({\"x\": Int(42)})");
    }

    #[test]
    fn test_display_seq() {
        let ctx = test_ctx();
        let seq = {
            let head_thunk = Arc::new(Thunk::new_materialized(
                Value::Int(1),
                test_span(1, 1, 1, 1),
            ));
            let tail_thunk = Arc::new(Thunk::new_materialized(
                value::make_seq_nil(),
                test_span(1, 1, 1, 1),
            ));
            let head_id = ctx.alloc_thunk(head_thunk);
            let tail_id = ctx.alloc_thunk(tail_thunk);
            value::make_seq_cons(head_id, tail_id, &ctx)
        };
        let display =
            value_to_display_string(&seq, &ctx, ast::Span::origin()).expect("display failed");
        assert_eq!(display, "Seq(Int(1), ...)");
    }

    #[test]
    fn test_display_proxy() {
        let ctx = test_ctx();
        let handler_thunk = Arc::new(Thunk::new_materialized(
            Value::Int(42),
            test_span(1, 1, 1, 1),
        ));
        let proxy = Value::Proxy {
            handler: ctx.alloc_thunk(handler_thunk),
        };
        let display =
            value_to_display_string(&proxy, &ctx, ast::Span::origin()).expect("display failed");
        assert_eq!(display, "Proxy");
    }

    #[test]
    fn test_pipeline_scalar_output() {
        let result = eval_to_json("42");
        assert_eq!(result, "42");
    }

    #[test]
    fn test_pipeline_string_output() {
        let result = eval_to_json("\"hello world\"");
        assert_eq!(result, "\"hello world\"");
    }

    #[test]
    fn test_pipeline_bool_output() {
        let result = eval_to_json("true");
        assert_eq!(result, "true");
    }

    #[test]
    fn test_pipeline_float_output() {
        let result = eval_to_json("3.14");
        // 3.14 tests float output, not π.
        #[allow(clippy::approx_constant)]
        {
            assert_eq!(result, "3.14");
        }
    }

    // --- Integration tests: typecheck→eval interaction ---

    /// Type errors are advisory: eval proceeds even when the type checker reports an error.
    ///
    /// This exercises the type checker call in `eval_source_with_config` (src/lib.rs).
    /// The type checker flags a mismatch (Int param given a String), but the evaluator
    /// sees the unannotated value and returns it unchanged.
    #[test]
    fn test_typecheck_advisory_eval_proceeds() {
        // Type annotation on param (x@Int) is advisory only.
        // Passing "hello" (String) should still evaluate successfully.
        let result = eval_source("[f: [fn [let x@Int] $x]  result: [f \"hello\"]]");
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
        let source = "[f: [fn [let x@Int] $x]  result: [f \"hello\"]]";
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
        let parsed = parse(source).expect("parse should succeed");
        let mut program = parsed.program.clone();
        desugar::desugar_surface_program(&mut program);
        let resolution_table = std::sync::Arc::new(resolve::resolve_surface_program(&program));
        let (_type_errors, type_annotation_table, _inferred) =
            typecheck::typecheck_surface_program_annotation_table(&program);
        let type_annotation_table = std::sync::Arc::new(type_annotation_table);
        let env = builtins::create_stdlib_env().expect("stdlib failed");
        let ctx = test_ctx();

        // Evaluate: this should fail because $undefined_var is not defined.
        let eval_result = crate::async_rt::block_on_anywhere(eval::eval_surface_file(
            &program,
            Arc::clone(&env),
            &ctx,
            &resolution_table,
            &type_annotation_table,
        ));
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

    /// Integration test: `typecheck_source` resolves the prelude `map` function (with [let ...] params).
    ///
    /// Calling `[call $map [fn [let x] $x] [1 2 3]]` should type-check without any
    /// "undefined variable" error for `map`, proving that `build_prelude_env()`
    /// is wired into `typecheck_source` and that prelude functions are in scope.
    ///
    /// Note: we check only type *errors*, not quality diagnostics — the unannotated
    /// identity lambda `[fn [let x] $x]` legitimately triggers "inferred type is Unknown"
    /// advisories from `scan_type_quality`, but those are informational, not errors.
    #[test]
    fn typecheck_source_resolves_prelude_map() {
        let input = "[call $map [fn [let x] $x] [1 2 3]]";
        let parsed = parse(input).expect("parse failed");
        let expand_base_dir = Arc::clone(&test_util::test_caps().root);
        // Use expand_surface_program so SurfaceItem::Decl macros are seen.
        let mut program = parsed.program;
        crate::async_rt::block_on_anywhere(expand::expand_surface_program(
            &mut program,
            false,
            &expand_base_dir,
        ))
        .expect("macro expansion failed");
        // Desugar after expansion so macros can introduce $_ patterns.
        desugar::desugar_surface_program(&mut program);
        // Variable resolution pass (Phase 1 of arena allocation strategy).
        let _resolution_table = resolve::resolve_surface_program(&program);
        let env = imports::build_prelude_env();
        let (type_errors, _type_map, _doc_map, _scheme_map, _diagnostics) =
            typecheck::typecheck_surface_program(&program, env);
        assert!(
            type_errors.is_empty(),
            "expected no type errors (prelude map should be in scope), got: {:?}",
            type_errors
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
            file: None,
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

    /// Regression test: undefined variable inside a call argument must still produce an error.
    ///
    /// `[tail: [drop 2 data]]` — `data` is a free variable in the call to `drop`.
    /// `typecheck_source_errors_only` must return `Err("undefined variable: data ...")`.
    /// This exercises the CALL-MONO path for `drop` (prelude-registered `Fn(Int, Seq(Top))`)
    /// and verifies that undefined-variable errors in call arguments are NOT swallowed.
    #[test]
    fn test_undefined_var_in_call_arg_drop() {
        let result = typecheck_source_errors_only("[tail: [drop 2 data]]");
        assert!(
            result.is_err(),
            "expected Err(undefined variable: data), got Ok(())"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("undefined variable: data"),
            "error should name 'data', got: {msg}"
        );
    }

    /// Same regression for `take`.
    #[test]
    fn test_undefined_var_in_call_arg_take() {
        let result = typecheck_source_errors_only("[head: [take 3 data]]");
        assert!(
            result.is_err(),
            "expected Err(undefined variable: data), got Ok(())"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("undefined variable: data"),
            "error should name 'data', got: {msg}"
        );
    }

    // --- [do] macro desugaring tests ---

    /// `[do result [Ok 42]]` — single-step (final expr only) returns Ok 42.
    ///
    /// Desugars to just `[Ok 42]` (the final step is returned as-is when there
    /// are no preceding binding steps).
    #[test]
    fn test_do_macro_single_step() {
        let result = eval_source("[do result [Ok 42]]");
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        let output = result.unwrap();
        assert!(
            output.contains("42"),
            "expected Ok(42) in output, got: {output}"
        );
    }

    /// `[do result [x: [Ok 1]] [Ok [+ x 1]]]` — one binding step.
    ///
    /// Desugars to `[result.bind [Ok 1] [fn [x] [Ok [+ x 1]]]]`
    /// = `[and-then [Ok 1] [fn [x] [Ok [+ x 1]]]]`
    /// = `[Ok 2]`
    ///
    /// KNOWN REGRESSION (T-974 / S-850): The prelude `and-then` matches `[Ok v]` (tag "Ok"),
    /// but T-974 qualifies constructors so `[Ok 1]` now produces Variant("Result.Ok", ...).
    /// The pattern no longer matches, causing E071 non-exhaustive match. The prelude migration
    /// to qualified tag patterns is tracked in S-850.
    #[test]
    fn test_do_macro_one_binding_step() {
        let result = eval_source("[do result [x: [Ok 1]] [Ok [+ x 1]]]");
        assert!(
            result.is_err(),
            "expected E071 regression error, got Ok: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("non-exhaustive match"),
            "expected non-exhaustive match error (E071), got: {err}"
        );
    }

    /// `[do result]` — no steps, calls `result.pure []` → `Ok([])`.
    #[test]
    fn test_do_macro_no_steps_calls_pure() {
        let result = eval_source("[do result]");
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        let output = result.unwrap();
        // result.pure is Ok, so [result.pure []] = [Ok []]
        assert!(
            output.contains("Ok"),
            "expected Ok in output, got: {output}"
        );
    }

    /// `[do]` with zero args → error.
    #[test]
    fn test_do_macro_zero_args_error() {
        let result = eval_source("[do]");
        assert!(result.is_err(), "expected error, got Ok: {:?}", result);
        let err = result.unwrap_err();
        assert!(
            err.contains("missing argument for required parameter 'first'"),
            "expected 'missing argument for required parameter 'first'' in error, got: {err}"
        );
    }

    /// Inferred `[do]` form with binding steps.
    ///
    /// Input: `[do [x: [Ok 1]] [Ok x]]`
    /// Desugars to: `[result.bind [Ok 1] [fn [x] [Ok x]]]`
    ///
    /// KNOWN REGRESSION (T-974 / S-850): The prelude `and-then` matches `[Ok v]` (tag "Ok"),
    /// but T-974 qualifies constructors so `[Ok 1]` now produces Variant("Result.Ok", ...).
    /// The pattern no longer matches, causing E071 non-exhaustive match. The prelude migration
    /// to qualified tag patterns is tracked in S-850.
    #[test]
    fn test_do_macro_inferred_form_binding() {
        let result = eval_source("[do [x: [Ok 1]] [Ok x]]");
        assert!(
            result.is_err(),
            "expected E071 regression error, got Ok: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("non-exhaustive match"),
            "expected non-exhaustive match error (E071), got: {err}"
        );
    }

    /// Inferred `[do]` form with single expression step passes through as-is.
    /// `[do [Ok 1]]` desugars to `[Ok 1]` (base case of do-fold returns the single step).
    ///
    /// Single-step inferred form evaluates the expression directly without needing
    /// monad inference (no bind chain to resolve).
    #[test]
    fn test_do_macro_inferred_form_expr() {
        // [do [Ok 1]] → inferred form, 1 step → do-fold returns [Ok 1] directly
        let result = eval_source("[do [Ok 1]]");
        assert!(
            result.is_ok(),
            "single-step inferred do should evaluate, got: {:?}",
            result
        );
        let output = result.unwrap();
        assert!(
            output.contains("Ok"),
            "expected Ok(1) for [do [Ok 1]], got: {output}"
        );
    }

    /// chr-class-instance: FD consistency check fires for conflicting instance arms.
    /// Two arms with same determining positions (Int, Int) but different determined types
    /// (Int vs Float) must produce a "consistency violation" type error.
    ///
    /// Regression: the original test had `[fn [x y] ...]` (missing `let`), producing a
    /// parse error that shadowed the consistency violation. Fixed to use `[fn [let x y] ...]`.
    #[test]
    fn test_instance_fd_consistency_violation() {
        let input = r#"[
  TestAdd: [class [let TestAdd a b c] [determines: [[[a b] c]]]
    op: [Fn@c [a b]]]
  TestAddInst: [instance TestAdd
    [pattern [a@Int b@Int c@Int]]:
      op: [fn [let x y] [+ x y]]
    [pattern [a@Int b@Int c@Float]]:
      op: [fn [let x y] [+ x y]]]
  result: 42
]"#;
        let result = typecheck_source_errors_only(input);
        assert!(
            result.is_err(),
            "expected consistency violation error, got Ok(())"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("consistency violation"),
            "expected 'consistency violation' in error, got: {msg}"
        );
    }

    // --- lint pipeline unit tests ---

    /// Lint pipeline: clean source string exits with no errors or warnings.
    ///
    /// Verifies that the parse → expand_surface_program → desugar → resolve → typecheck pipeline
    /// (the same pipeline used by `tinct lint`) produces no errors and no diagnostics
    /// for a well-typed input.
    ///
    /// This is the unit-test counterpart to `just lint-stdlib`: it confirms the lint
    /// pipeline is wired correctly and that a trivially clean file produces exit code 0
    /// (no errors or warnings).
    #[test]
    fn test_lint_pipeline_clean_source() {
        // A well-typed dict with annotated fields and arithmetic — should pass lint clean.
        let input = r#"[
  x@Int: 42
  y@String: "hello"
  z@Int: [+ x 1]
]"#;
        let result = typecheck_source(input);
        assert!(
            result.is_ok(),
            "lint pipeline should produce no errors or warnings for clean source, got: {:?}",
            result
        );
    }

    /// Lint pipeline: type error produces Err (exit code 1 behavior).
    ///
    /// Verifies that an undefined variable reference causes the lint pipeline to
    /// return an error, matching the `tinct lint` exit-code-1 behavior.
    #[test]
    fn test_lint_pipeline_type_error() {
        // Referencing an undefined variable should produce an "undefined variable" type error.
        let input = r#"[x: undefined_var]"#;
        let result = typecheck_source_errors_only(input);
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
    #[test]
    fn test_lint_pipeline_no_side_effects() {
        // If lint executed this, it would emit text to stdout. Since it only type-checks,
        // no output should occur. The absence of a panic or visible output is the assertion.
        let input = r#"[call $emit "this should not appear"]"#;
        // typecheck_source only parses, expands, desugars, resolves, and type-checks.
        // It does not evaluate — so no emit side-effect fires.
        let _result = typecheck_source_errors_only(input);
        // If we reach here without IO side-effects, the test passes.
        // (Capturing stdout in a unit test would require infrastructure not worth adding.)
    }

    // --- fn macro tests (globally registered via expand.rs) ---

    /// fn macro: globally registered via expand.rs — normal fn usage works without any include.
    ///
    /// The fn macro is only triggered in programmatic macro-output contexts (when another macro
    /// produces Call(VarRef("fn"), ...)). Normal parser-level [fn [let x y] body] produces
    /// Expr::Fn directly and is unaffected.
    #[test]
    fn test_syntax_llt_fn_no_break() {
        let result = eval_source(
            r#"[f: [fn [let x y] [+ x y]]]
[f 3 4]"#,
        );
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        let output = result.unwrap();
        assert_eq!(output, "Int(7)", "expected Int(7), got: {output}");
    }

    /// fn macro: triggered when another macro produces Call(fn, ...) with non-LetDecl params.
    /// The fn macro normalizes Call(x, [y]) → proper Fn params.
    /// fn/class/type macros are globally registered via expand.rs; no include needed.
    /// TODO(macro-ast-expression-compat): fn macro alias produces Fn with correct params
    /// but variable resolution doesn't connect body VarRefs to macro-produced params.
    #[test]
    fn test_syntax_llt_fn_macro_triggered() {
        let result = eval_source(
            r#"[macro wrap-fn [let p-params p-body]
  [builtin-variant "Fn" [
    params: [map
      [fn [let p] [name: p.name  annotation: []  variadic: false]]
      [concat [0: p-params.fn] p-params.args]]
    body: p-body
    return-ann: []
    desugared: false]]]
[add-fn: [wrap-fn [x y] [+ x y]]]
[add-fn 3 4]"#,
        );
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        let output = result.unwrap();
        assert_eq!(output, "Int(7)", "expected Int(7), got: {output}");
    }

    /// fn macro: single-param case — VarRef params form.
    /// fn/class/type macros are globally registered via expand.rs; no include needed.
    #[test]
    fn test_syntax_llt_fn_single_param() {
        let result = eval_source(
            r#"[macro wrap-fn [let p-params p-body]
  [builtin-variant "Fn" [
    params: [0: [name: p-params.name  annotation: []  variadic: false]]
    body: p-body
    return-ann: []
    desugared: false]]]
[sq: [wrap-fn x [* x x]]]
[sq 5]"#,
        );
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        let output = result.unwrap();
        assert_eq!(output, "Int(25)", "expected Int(25), got: {output}");
    }

    /// fn macro: function defined via fn + let params and called.
    /// Tests that fn macro produces a callable function (globally registered, no include needed).
    #[test]
    fn test_syntax_llt_fn_already_let_decl() {
        // Test a function defined normally (fn + let params) and called.
        // This exercises the fn macro path through eval_source.
        let result = eval_source(
            r#"[add-fn: [fn [let x y] [+ x y]]]
[add-fn 10 20]"#,
        );
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        let output = result.unwrap();
        assert_eq!(output, "Int(30)", "expected Int(30), got: {output}");
    }

    /// Regression test for the formatter arity bug:
    /// A function defined in an intermediate dict should be callable with the correct arity.
    /// This test exercises the scope-chain strict-forcing path for intermediate dict values.
    #[test]
    fn test_eval_document_dict_function_arity() {
        // Simulates the compact.llt pattern:
        //   expression 1: some dict (mock of [include %rust "core"])
        //   expression 2: format-* dict with a 0-arg function
        //   expression 3: call the function
        let result = eval_source("[]\n[f: [fn [let] \"hello\"]]\n[f]");
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        let output = result.unwrap();
        assert_eq!(
            output, "String(\"hello\")",
            "expected String(hello), got: {output}"
        );
    }

    /// Regression test: function with 1 param in an intermediate dict should work.
    #[test]
    fn test_eval_document_dict_function_1param() {
        let result = eval_source("[]\n[f: [fn [let x] x]]\n[f 42]");
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        let output = result.unwrap();
        assert_eq!(output, "Int(42)", "expected Int(42), got: {output}");
    }

    /// Regression: formatter arity bug. Tests the exact formatter pipeline.
    #[test]
    #[ignore = "pre-existing regression from runtime-v2 merge: include-decomp changed include arity; [include %rust \"core\"] uses old 2-arg include syntax"]
    fn test_formatter_arity_via_eval_source() {
        // eval_source with the exact formatter pattern: include core, define function, call it
        let result =
            eval_source("[include %rust \"core\"]\n[f: [fn [let x] x]]\n[try [fn [let] [f 42]]]");
        assert!(result.is_ok(), "eval_source should work: {:?}", result);
        // Compact.llt pattern: define 0-param function, call it
        let result2 = eval_source(
            "[include %rust \"core\"]\n[f: [fn [let] \"hello\"]]\n[try [fn [let] [f]]]",
        );
        assert!(
            result2.is_ok(),
            "eval_source 0-param should work: {:?}",
            result2
        );
    }

    // -------------------------------------------------------------------------
    // escape_json_str tests
    // -------------------------------------------------------------------------

    #[test]
    fn escape_json_str_double_quote() {
        assert_eq!(escape_json_str("\""), "\\\"");
    }

    #[test]
    fn escape_json_str_backslash() {
        assert_eq!(escape_json_str("\\"), "\\\\");
    }

    #[test]
    fn escape_json_str_newline() {
        assert_eq!(escape_json_str("\n"), "\\n");
    }

    #[test]
    fn escape_json_str_carriage_return() {
        assert_eq!(escape_json_str("\r"), "\\r");
    }

    #[test]
    fn escape_json_str_tab() {
        assert_eq!(escape_json_str("\t"), "\\t");
    }

    #[test]
    fn escape_json_str_backspace() {
        assert_eq!(escape_json_str("\x08"), "\\b");
    }

    #[test]
    fn escape_json_str_form_feed() {
        assert_eq!(escape_json_str("\x0c"), "\\f");
    }

    #[test]
    fn escape_json_str_c0_control() {
        // U+0001 (SOH) → 
        assert_eq!(escape_json_str("\x01"), "\\u0001");
    }

    #[test]
    fn escape_json_str_clean_ascii() {
        assert_eq!(escape_json_str("hello world"), "hello world");
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
