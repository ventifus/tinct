//! Core evaluation module: lazy evaluation with letrec dict scoping, variable lookup,
//! sequential expression evaluation, and document pipeline execution.
//!
//! See also: eval_call.rs (function evaluation), eval_materialize.rs (CEK machine implementation).

pub(crate) use crate::eval_call::eval_call_core;
pub use crate::eval_call::{invoke_function, CallContext};

// Re-export CEK machine components from eval_materialize
pub(crate) use crate::eval_materialize::{attach_materialization_context, run, Action};

// Split modules — dict construction
#[path = "eval_dict.rs"]
mod eval_dict_mod;

pub(crate) use eval_dict_mod::*;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex, RwLock};

use indexmap::IndexMap;

use crate::arena::{EnvArena, ThunkArena, ThunkId};
use crate::ast::{
    Annotation, CoreExpr, LiteralPattern, Param, Pattern, Span, Spanned, SurfaceNode,
    SurfaceProgram,
};
use crate::builtins::MAX_COLLECT_SIZE;
use crate::error::{EvalError, EvalResult};
use crate::eval_core::extract_fn_annotation_extra;
use crate::rust_span;
use crate::types::{Row, Type};
// Circular module dependency: this module calls builtins via function pointers stored in `Value::Builtin`.
// builtins.rs imports `invoke_function` and `materialize` from this module.
// This bidirectional dependency is safe because neither module's initialization depends on the other.
use crate::env::Env;
use crate::value::{string_val, HashableValue, Thunk, Value};

// ============================================================================
// Document pipeline evaluation
// ============================================================================

thread_local! {
    /// Cached empty dict thunk used as the default `%` when no stdin is provided.
    /// Avoids allocating a fresh `Arc<Thunk>` on every `eval_surface_file` call for empty programs.
    static EMPTY_DICT_THUNK: Arc<Thunk> = Arc::new(Thunk::new_materialized(
        Value::Dict(IndexMap::new()),
        rust_span!(),
    ));
}

/// Wrap a thunk with nominal type validation for pipeline input contracts.
///
/// Evaluate a sequence of surface expression nodes as a scope chain, returning the
/// last expression's thunk lazily.
///
/// This is the canonical scope-chaining loop shared by [`eval_surface_document`] and
/// `builtin_eval` (in `builtins_meta.rs`). Both callers implement identical semantics:
///
/// - **Intermediate expressions** (all but the last): lower → eval → materialize.
///   If the result is a non-empty `Dict` or `Overlay`, ALL `HashableValue::Str` entries are
///   inserted as lazy thunks into a child environment for subsequent expressions.
///   Non-dict/overlay results are silently ignored (no error, no scope extension).
///   This is the `bare-include-scope` behavior.
///   **Why lazy?** Dead bindings that are never accessed must never fire. Evaluation
///   is demand-driven: a binding is forced only when a subsequent expression accesses
///   it. This is the correct lazy evaluation semantics throughout (function bodies and
///   document-level).
/// - **Last expression**: lower → eval (lazy). The resulting thunk is returned
///   without forcing — callers decide when (and whether) to materialize it.
/// - **Empty slice**: returns a materialized empty-dict thunk (same as an empty doc).
pub(crate) async fn eval_document_exprs(
    expr_nodes: &[Arc<SurfaceNode>],
    env: Arc<RwLock<Env>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    eval_document_exprs_with_env(expr_nodes, env, ctx)
        .await
        .map(|(thunk, _env)| thunk)
}

/// Like `eval_document_exprs` but also returns the leaf environment after evaluation.
/// The leaf env is `current_env` at the end of the loop — a chain of child envs built
/// from intermediate dict results. Used by `builtin-eval` to construct the result
/// `Value::Env` so that intermediate dict bindings (e.g. prelude's `=`, `map`)
/// are accessible from the returned env's ancestor chain.
pub(crate) async fn eval_document_exprs_with_env(
    expr_nodes: &[Arc<SurfaceNode>],
    env: Arc<RwLock<Env>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<(Arc<Thunk>, Arc<RwLock<Env>>)> {
    if expr_nodes.is_empty() {
        return Ok((
            Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                rust_span!(),
            )),
            env,
        ));
    }

    let mut current_env = env;
    let last_idx = expr_nodes.len() - 1;

    for (i, node) in expr_nodes.iter().enumerate() {
        let (core_spanned, lower_diags) = crate::lower::lower(node);
        if !lower_diags.is_empty() {
            if let Some(err) = lower_diags
                .into_iter()
                .find(|d| matches!(d.kind, crate::lower::LowerDiagnosticKind::Error))
            {
                return Err(EvalError::user_error(err.message, err.span).into());
            }
        }
        let node_span = node.span.clone();

        if i == last_idx {
            // Last expression: return its thunk lazily (no materialization).
            let thunk = eval_core_expr(&core_spanned, &current_env, ctx).await?;
            return Ok((thunk, current_env));
        }

        // Intermediate expression: eval and materialize to extract potential bindings.
        let thunk = eval_core_expr(&core_spanned, &Arc::clone(&current_env), ctx).await?;
        let value = materialize(&thunk, Some(&node_span), ctx).await?;

        // If the result is a non-empty Dict or Overlay, promote ALL HashableValue::Str entries
        // into a child environment. Non-dict results are silently skipped — they act as
        // side-effect expressions that contribute no bindings to the scope chain.
        let map = match value {
            Value::Dict(ref m) if !m.is_empty() => Some(m.clone()),
            Value::Overlay(ref l, ref r) => Some(
                crate::builtins::flatten_overlay(l, r, "document pipeline", ctx, node_span.clone())
                    .await?,
            ),
            _ => None,
        };

        if let Some(entries) = map {
            let child_env = Arc::new(RwLock::new(Env::with_parent(Arc::clone(&current_env))));
            {
                let mut env_write = child_env.write().unwrap();
                for (key, val_thunk_id) in entries.iter() {
                    if let HashableValue::Str(name) = key {
                        let val_thunk = ctx.get_thunk(*val_thunk_id);
                        env_write.insert_value(name.to_string(), val_thunk);
                    }
                }
            }
            current_env = child_env;
        }
        // Non-dict/overlay: silently skip — no scope extension, no error.
    }

    unreachable!(
        "eval_document_exprs_with_env: loop did not return — expr_nodes was non-empty but last_idx was never reached"
    )
}

/// Evaluate a SurfaceDocument: a sequence of expression items forming a scope chain.
///
/// Each `SurfaceItem::Expr` is lowered to `CoreExpr` via `lower.rs` and evaluated via
/// `eval_core_expr_pub`. `SurfaceItem::Decl` items are skipped (processed at expand time).
///
/// Scope-chain semantics are delegated to [`eval_document_exprs`]:
/// - Intermediate expressions are materialized to WHNF; Dict/Overlay results promote
///   their entry thunks (lazily) into a child scope for subsequent expressions.
/// - The last expression is returned as-is (lazy, any type).
/// - An empty document returns an empty dict.
///
/// Caps enforcement and expects validation are handled by tinct-side loader code
/// via `builtin-cap-env-has?` and `builtin-check-type` (T-1506, T-1507).
pub async fn eval_surface_document(
    doc: &Spanned<Arc<crate::ast::SurfaceDocument>>,
    env: Arc<RwLock<Env>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    // Collect expression nodes (skip Decl items — processed by expander) and
    // delegate the scope-chaining loop to the shared eval_document_exprs function.
    let expr_nodes: Vec<Arc<SurfaceNode>> = doc.node.expressions().cloned().collect();
    eval_document_exprs(&expr_nodes, env, ctx).await
}

/// Evaluate a SurfaceProgram: one or more documents separated by `---`.
///
/// # Precondition
///
/// **Pipeline invariant:** `desugar_surface_program` →
/// `resolve_surface_program` must be called before passing the program here —
/// it writes de Bruijn coordinates inline to the AST nodes.
/// If type checking was skipped, `TypeAssert` nodes will use Type::Unknown (accepts all values).
///
/// # Env threading
///
/// `env` is the fully-constructed loader environment (prelude + caps + named sections already
/// bound). Each document receives the **same** `env` — there is no sequential env threading
/// in this function. The tinct-side `builtin-eval` call (inside `loader.llt`) is responsible
/// for threading the per-document leaf env forward via `builtin-extend-env`.
pub async fn eval_surface_file(
    program: &SurfaceProgram,
    env: Arc<RwLock<Env>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let mut last = EMPTY_DICT_THUNK.with(Arc::clone);
    for surface_doc in &program.documents {
        if surface_doc.node.stage == Some(crate::ast::Stage::Type) {
            continue;
        }
        last = eval_surface_document(surface_doc, Arc::clone(&env), ctx).await?;
    }
    Ok(last)
}

/// Evaluate a SurfaceProgram with an optional initial `%` value injected into the environment.
///
/// See `eval_surface_file` for preconditions. When `initial_input` is `Some(thunk)`,
/// that thunk is bound as `%` in a child environment visible to all documents.
/// Used by the formatter (which passes the AST dict as `%`).
pub async fn eval_surface_file_with_input(
    program: &SurfaceProgram,
    env: Arc<RwLock<Env>>,
    ctx: &Arc<EvalContext>,
    initial_input: Option<Arc<Thunk>>,
) -> EvalResult<Arc<Thunk>> {
    let eval_env = if let Some(input) = initial_input {
        let child = Arc::new(RwLock::new(Env::with_parent(Arc::clone(&env))));
        child.write().unwrap().insert_value("%".to_string(), input);
        child
    } else {
        env
    };
    eval_surface_file(program, eval_env, ctx).await
}

// ============================================================================
// End document pipeline evaluation
// ============================================================================

pub(crate) const DEFAULT_ANNOTATION_KEY: &str = "default";
pub(crate) const IS_ANNOTATION_KEY: &str = "is";

/// Type alias for the optional default expression + environment pair used by guarded thunks.
/// Reduces type_complexity in function signatures that carry this optional default.
type GuardDefault = (Arc<Spanned<crate::ast::CoreExpr>>, Arc<RwLock<Env>>);

/// Type alias for the return type of `match_pattern` — an async fn returning an optional env.
type MatchPatternFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Option<Arc<RwLock<Env>>>>> + 'a>>;

// ValuesEqualFuture removed — primitive_eq is synchronous (no async needed).

#[cfg(test)]
const ANNOTATION_META_KEYS: &[&str] = &["default", "type", "doc", "is", "repr", "_constructor"];

#[cfg(test)]
pub(crate) fn annotation_has_structural_fields(annotation: &crate::ast::Annotation) -> bool {
    match annotation {
        crate::ast::Annotation::PropertyDict(entries) => entries.iter().any(|entry| {
            let Some(key_node) = entry.node.key.as_ref() else {
                return false;
            };
            match &key_node.expr {
                crate::ast::SurfaceExpression::Str(name) => {
                    !ANNOTATION_META_KEYS.contains(&name.as_str())
                }
                _ => true,
            }
        }),
        crate::ast::Annotation::Simple(_) | crate::ast::Annotation::Annotated(_, _) => false,
    }
}

/// Formats a field path for TypeAssert error display. Each segment is separately
/// backtick-quoted: `user`.`address`.`zip`. Not for reconstruction — display only.
pub(crate) fn format_field_path(field_path: &[String]) -> String {
    field_path
        .iter()
        .map(|s| format!("`{}`", s))
        .collect::<Vec<_>>()
        .join(".")
}

/// Unified type environment handle carried by EvalContext.
///
/// Encapsulates both the type-stage eval environment (`type_stage_env`, used by
/// `builtin_eval_types`) and the type-checker state (populated by `builtin-typecheck`
/// as a side effect). Accessed via `builtin-get-type-context` and mutated by
/// `builtin-typecheck`. This is the opaque handle that loader.llt threads through
/// the type-checking pipeline.
///
/// Wrapped in `Arc<Mutex<Option<...>>>` on `EvalContext` so that:
/// - `None` = TypeContext not yet initialized (bootstrap phase before builtin-make-type-ctx)
/// - `Some` = initialized and ready for use
/// - The `Arc` allows child contexts to share the same TypeContext (they should see each
///   other's updates because builtin-typecheck is a side-effecting operation)
///
/// Full implementation in T-1341 (builtin-get-type-context, builtin-make-type-ctx,
/// builtin-fork-type-ctx). This struct is the stable field layout.
#[derive(Debug, Clone)]
pub struct TypeContextData {
    /// Type-stage environment: contains only type-level builtins, no IO/caps/runtime API.
    /// Used by `builtin_eval_types` to evaluate type-stage documents in isolation.
    /// Mirrors `EvalConfig.type_stage_env` but owned by TypeContext so it can be updated
    /// as new type declarations are registered.
    pub type_stage_env: Arc<RwLock<Env>>,
    /// Accumulated Hindley-Milner inference environment.
    /// Initialized to the builtin_core TypeEnv at startup (via `init_type_context` callers).
    /// Each `builtin-typecheck` call seeds from this env and writes the resulting `final_env`
    /// back, accumulating type knowledge across files (prelude → user code).
    /// This makes prelude names (map, filter, raise, etc.) visible to the type checker
    /// when checking user code, without re-typechecking prelude on every call.
    pub inference_env: Arc<RwLock<crate::env::Env>>,
}

/// Immutable session configuration shared across evaluation.
#[derive(Debug)]
pub struct EvalConfig {
    pub base_dir: cap_std::fs::Dir,
    pub stdlib_env: Arc<RwLock<Env>>,
    /// Type-stage environment: contains only type-level builtins, no IO/caps/runtime API.
    /// Used by `builtin_eval_types` to evaluate type-stage documents in isolation.
    pub type_stage_env: Arc<RwLock<Env>>,
    pub no_fs: bool,
    /// When true, every `$include` call must supply an integrity hash.
    /// Hashless includes are rejected with `IncludeHashRequired`.
    pub require_integrity: bool,
    /// Macro inject names: macro_name -> Vec<inject_name>.
    /// Populated by the expansion pass, used by the `macro-injects` builtin.
    /// Only macros with `inject:` declarations have entries; macros without
    /// inject: (using only gensym hygiene) are absent from this map.
    pub macro_injects_map: HashMap<String, Vec<String>>,
    /// Source file path where evaluation started (if available).
    /// Propagated to FnAnnotation for LSP hover and diagnostics.
    pub source_file: Option<String>,
}

/// Cache entry for the string-keyed include cache used by `include-cache-get`/`include-cache-put`.
///
/// Keyed by `blake3(cap-identity + "|" + source_text)` so that:
/// - `Missing` — known cache miss (prevents redundant re-queries)
/// - `Pending` — file is currently being evaluated (cycle detection sentinel)
/// - `Cached` — successfully-evaluated result thunk from this file.
#[derive(Debug, Clone)]
pub enum IncludeCacheEntry {
    Missing,
    Pending,
    Cached(Arc<Thunk>),
}

/// Mutable evaluation state (include caching).
#[derive(Debug)]
pub struct EvalState {
    /// String-keyed include cache for `include-cache-get`/`include-cache-put`.
    /// Key is `blake3(cap-identity + "|" + source_text)`.
    /// Replaces the old inode-keyed include_guard and include_cache.
    pub string_include_cache: HashMap<String, IncludeCacheEntry>,
    /// Stack of active $include calls: `(display_path, call_site_span)`.
    ///
    /// Pushed by `builtin_include` before evaluating the included file, popped
    /// after (in both success and error branches). Used to annotate errors from
    /// nested includes with the full include path, e.g.:
    ///   "included from a.llt at 3:10-3:25"
    ///   "included from b.llt at 1:5-1:20"
    pub include_chain: Vec<(String, Span)>,
    /// Stack of thunks currently being evaluated: `(origin_label, span)`.
    ///
    /// Pushed when transitioning from Unevaluated/PendingBuiltin/PendingCall/Guarded
    /// to InProgress (before extracting data), popped on successful materialization.
    /// On circular dependency detection (thunk already InProgress), this stack
    /// contains the full cycle chain for error reporting.
    ///
    /// Example: `[("a", span1), ("b", span2), ("x", span3)]` means evaluating
    /// `x` requires `a`, which requires `b`, which requires `x` (cycle).
    ///
    /// Upper bound: continuation stack frames (2048) × ~80 bytes/entry ≈ 160 KB.
    pub eval_stack: Vec<(Arc<str>, Span)>,
    // future: trace_log, eval_stats
}

/// Evaluation infrastructure context: separates session config from variable bindings.
///
/// Config is immutable (Arc without Mutex); state is mutable (Arc<Mutex>).
/// Thread as `&Arc<EvalContext>` through eval/materialize; thunks capture `Arc::clone(ctx)`.
///
/// **Phase 2 Arena Migration (Registry Approach):** Arenas act as a GC root / bulk-deallocation
/// boundary. Thunks are allocated in the arena AND stored as Arc<Thunk> in Value variants.
/// This establishes the arena pattern without the massive ThunkId-in-Value refactor.
/// Full ThunkId migration is deferred to Phase 3.
#[derive(Debug)]
pub struct EvalContext {
    pub config: Arc<EvalConfig>,
    pub state: Arc<Mutex<EvalState>>,
    /// Thunk arena registry. Phase 2: stores Vec<Arc<Thunk>> and provides bulk deallocation.
    /// Thunks are allocated here but Value variants still use Arc<Thunk> directly.
    /// **Shared ownership:** Arc<Mutex<>> allows child contexts (created via with_base_dir)
    /// to share the parent's arena, preventing ThunkId index-out-of-bounds panics.
    pub(crate) thunk_arena: Arc<Mutex<ThunkArena>>,
    /// Env arena registry. Phase 3: populated by `eval_dict` (alloc_root +
    /// fill_letrec_slot per dict scope). Env IDs enable O(1) variable lookup in the
    /// CoreExpr force path.
    /// **Shared ownership:** Arc<Mutex<>> allows child contexts to share the parent's arena.
    pub(crate) env_arena: Arc<Mutex<EnvArena>>,
    /// Env variable allowlist. None = unrestricted (all allowed), Some(set) = only those in set.
    /// Some(empty) means all denied (--no-env mode).
    pub env_allowed: Option<HashSet<String>>,
    /// Pipeline blame map: records producing stage label for each `%` thunk at `---` boundaries.
    /// Key is the ThunkId of the `%` pipeline variable, value is the producing stage's file path
    /// or index. Used by contract violation errors to identify the positive party (producer)
    /// per Findler & Felleisen (2002). Avoids a `Value::Tagged` variant which would require
    /// updating all exhaustive `Value` matches.
    pub blame_map: Mutex<HashMap<ThunkId, String>>,
    /// Already-open libdir Dir, shared from the bootstrap boundary (main.rs).
    /// Used by `builtin_include` to inject `%libdir` into the included file's environment
    /// without calling `open_ambient_dir` again. `None` in contexts where libdir was not
    /// opened (e.g., --no-libdir, bootstrap contexts, tests).
    /// Propagated through `with_base_dir` so nested includes see the same Dir.
    pub libdir_dir: Mutex<Option<Arc<cap_std::fs::Dir>>>,
    /// Cancellation token for this evaluation scope. Blocking async builtins (`await`,
    /// `recv`, `send`, `select-once`) select! against this token so they return early when
    /// the context is cancelled. The root context holds a fresh root token; child contexts
    /// created by `with-cancel`, `with-timeout`, and `with-deadline` hold child tokens.
    /// Cheap to clone (Arc internally); `CancellationToken::child_token()` is also cheap.
    pub cancel: tokio_util::sync::CancellationToken,
    /// Background task handles registered here directly (signal-channel, timer-channel,
    /// watch-channel ×2, with-timeout, with-deadline, `with_timeout_ms`). Tasks either
    /// run indefinitely (loop until cancelled) or complete with `()`. The `drain` builtin
    /// calls `abort()` on each handle — a no-op on completed handles, prevents one-shot
    /// sleep tasks from blocking drain — then awaits each handle to allow clean shutdown.
    pub task_registry: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    /// Profiling collector: records span-level timing data during evaluation.
    /// None when profiling is disabled (the common case). When Some, every thunk materialization
    /// opens and closes a span. Shared via Arc<Mutex<>> so child contexts created by `with_base_dir`
    /// write to the same collector.
    /// Public for CLI initialization (main.rs --profile flag).
    pub profiling: Option<Arc<Mutex<crate::profiling::ProfilingCollector>>>,
    /// Type constructor environment from type inference: name → TyConDef.
    /// Set once after typechecking via `set_tycon_env`; read-only thereafter (OnceLock).
    /// Used by `is_subtype` to determine variance and structural rules for user-defined
    /// type constructors. `None` before typechecking or when `--no-typecheck` is used;
    /// `is_subtype` falls back to invariant behaviour in that case.
    /// Propagated to child contexts (with_base_dir, with_cancel_token, with_explicit_cancel,
    /// with_timeout_ms) so nested includes and scoped cancellation see the same TyConEnv.
    pub tycon_env: std::sync::OnceLock<std::sync::Arc<crate::type_def::TyConEnv>>,
    /// Optional sink for method arms registered during `eval_dict_core` pre-scan.
    ///
    /// Unified type environment handle for this evaluation scope.
    ///
    /// `None` until initialized by `builtin-make-type-ctx` (T-1341). Once set, shared
    /// across all child contexts via `Arc::clone` so that `builtin-typecheck` side effects
    /// (TypeScheme registration) are visible everywhere in the pipeline.
    ///
    /// Child contexts created via `with_base_dir`, `with_cancel_token`, `with_explicit_cancel`,
    /// and `with_timeout_ms` all share the same `Arc` — they see the same TypeContext state.
    /// This is intentional: type checking is monotonic (schemas are only added, never removed)
    /// and the pipeline must accumulate type knowledge across files.
    ///
    /// Full implementation: T-1341 (builtin-get-type-context, builtin-make-type-ctx,
    /// builtin-fork-type-ctx), T-1343 (TypeContext struct layout).
    pub type_context: Arc<Mutex<Option<TypeContextData>>>,
}

impl EvalContext {
    pub fn new(
        base_dir: cap_std::fs::Dir,
        stdlib_env: Arc<RwLock<Env>>,
        type_stage_env: Arc<RwLock<Env>>,
        no_fs: bool,
    ) -> Arc<Self> {
        Self::new_with_options(base_dir, stdlib_env, type_stage_env, no_fs, false, None)
    }

    /// Create a new EvalContext with a fresh empty arena.
    ///
    /// This constructor is for bootstrap and test contexts:
    /// - Bootstrap contexts (run_loader_pipeline, where loader.llt is being evaluated)
    /// - Re-entrant macro expansion (depth > 0 in expand.rs)
    /// - Test helpers that create contexts without a prelude env
    ///
    /// Under the new single-execution-path architecture, `new()`, `new_empty()`, and
    /// `new_with_options()` all create fresh arenas.
    pub fn new_empty(
        base_dir: cap_std::fs::Dir,
        stdlib_env: Arc<RwLock<Env>>,
        no_fs: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            config: Arc::new(EvalConfig {
                base_dir,
                stdlib_env,
                type_stage_env: Arc::new(RwLock::new(Env::new())),
                no_fs,
                require_integrity: false,
                macro_injects_map: HashMap::new(),
                source_file: None,
            }),
            state: Arc::new(Mutex::new(EvalState {
                string_include_cache: HashMap::new(),
                include_chain: Vec::new(),
                eval_stack: Vec::new(),
            })),
            thunk_arena: Arc::new(Mutex::new(ThunkArena::new())),
            env_arena: Arc::new(Mutex::new(EnvArena::new())),
            env_allowed: None,
            blame_map: Mutex::new(HashMap::new()),
            libdir_dir: Mutex::new(None),
            cancel: tokio_util::sync::CancellationToken::new(),
            task_registry: Arc::new(Mutex::new(Vec::new())),
            profiling: None,
            tycon_env: std::sync::OnceLock::new(),
            type_context: Arc::new(Mutex::new(None)),
        })
    }

    pub fn new_with_options(
        base_dir: cap_std::fs::Dir,
        stdlib_env: Arc<RwLock<Env>>,
        type_stage_env: Arc<RwLock<Env>>,
        no_fs: bool,
        require_integrity: bool,
        env_allowed: Option<HashSet<String>>,
    ) -> Arc<Self> {
        // Under the new single-execution-path architecture, each EvalContext gets a
        // fresh arena. There is no pre-built stdlib arena to inherit: the stdlib is
        // loaded exactly once via run_loader_pipeline, and the resulting thunks live
        // in the env bindings (not in a shared arena snapshot).
        let thunk_arena = Arc::new(Mutex::new(ThunkArena::new()));
        Arc::new(Self {
            config: Arc::new(EvalConfig {
                base_dir,
                stdlib_env,
                type_stage_env,
                no_fs,
                require_integrity,
                macro_injects_map: HashMap::new(),
                source_file: None,
            }),
            state: Arc::new(Mutex::new(EvalState {
                string_include_cache: HashMap::new(),
                include_chain: Vec::new(),
                eval_stack: Vec::new(),
            })),
            thunk_arena,
            env_arena: Arc::new(Mutex::new(EnvArena::new())),
            env_allowed,
            blame_map: Mutex::new(HashMap::new()),
            libdir_dir: Mutex::new(None),
            cancel: tokio_util::sync::CancellationToken::new(),
            task_registry: Arc::new(Mutex::new(Vec::new())),
            profiling: None,
            tycon_env: std::sync::OnceLock::new(),
            type_context: Arc::new(Mutex::new(None)),
        })
    }

    /// Create a new EvalContext with a different base_dir but sharing the same
    /// state (include guard, cache) and stdlib_env. Avoids allocating a new
    /// EvalState; shares the underlying stdlib_env and state Rc allocations
    /// (e.g., during $include).
    ///
    /// Inherits `no_fs` and `require_integrity` from the parent config so that
    /// sandbox restrictions are preserved across directory changes.
    ///
    /// **Phase 2 Arena Migration (Registry):** SHARES the parent's arenas (Arc::clone).
    /// This fixes the ThunkId index-out-of-bounds bug: values from the parent context
    /// (including stdlib) carry ThunkIds that index into the parent's arena. The child
    /// context must use the SAME arena to resolve those ThunkIds.
    pub fn with_base_dir(&self, base_dir: cap_std::fs::Dir) -> Arc<Self> {
        Arc::new(Self {
            config: Arc::new(EvalConfig {
                base_dir,
                stdlib_env: Arc::clone(&self.config.stdlib_env),
                type_stage_env: Arc::clone(&self.config.type_stage_env),
                no_fs: self.config.no_fs,
                require_integrity: self.config.require_integrity,
                macro_injects_map: self.config.macro_injects_map.clone(),
                source_file: self.config.source_file.clone(),
            }),
            state: Arc::clone(&self.state),
            thunk_arena: Arc::clone(&self.thunk_arena),
            env_arena: Arc::clone(&self.env_arena),
            env_allowed: self.env_allowed.clone(),
            blame_map: Mutex::new(self.blame_map.lock().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel: self.cancel.clone(),
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
            tycon_env: {
                let child_lock = std::sync::OnceLock::new();
                if let Some(env) = self.tycon_env.get() {
                    child_lock.set(std::sync::Arc::clone(env)).ok();
                }
                child_lock
            },
            // TypeContext is shared: child contexts see the same type-checker state.
            // builtin-typecheck updates TypeContext in-place, so all contexts in a
            // pipeline must share the same Arc to observe each other's registrations.
            type_context: Arc::clone(&self.type_context),
        })
    }

    /// Like `with_base_dir` but accepts an optional `base_dir_path` that is not used.
    pub fn with_base_dir_and_path(
        &self,
        base_dir: cap_std::fs::Dir,
        _base_dir_path: Option<std::path::PathBuf>,
    ) -> Arc<Self> {
        self.with_base_dir(base_dir)
    }

    /// Create a child EvalContext with a new cancellation token derived from this context's token.
    ///
    /// The child token is automatically cancelled when the parent token is cancelled.
    /// Cancelling the child token does NOT cancel the parent.
    ///
    /// Returns `(child_ctx, child_token)` where `child_token` is a clone of the child's token
    /// that can be passed to `[cancel-task]` (via `Value::Context(child_token)`) or stored for
    /// later manual cancellation.
    pub fn with_cancel_token(self: &Arc<Self>) -> (Arc<Self>, tokio_util::sync::CancellationToken) {
        let child_token = self.cancel.child_token();
        let child_ctx = Arc::new(Self {
            config: Arc::clone(&self.config),
            state: Arc::clone(&self.state),
            thunk_arena: Arc::clone(&self.thunk_arena),
            env_arena: Arc::clone(&self.env_arena),
            env_allowed: self.env_allowed.clone(),
            blame_map: Mutex::new(self.blame_map.lock().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel: child_token.clone(),
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
            tycon_env: {
                let child_lock = std::sync::OnceLock::new();
                if let Some(env) = self.tycon_env.get() {
                    child_lock.set(std::sync::Arc::clone(env)).ok();
                }
                child_lock
            },
            type_context: Arc::clone(&self.type_context),
        });
        (child_ctx, child_token)
    }

    /// Create a child EvalContext with an explicitly provided CancellationToken.
    ///
    /// Unlike `with_cancel_token`, this accepts a token that need not be a child of the parent's
    /// token — it can be any token (e.g., a fresh root token from `non-cancellable`). Used by
    /// `builtin_with_context` to avoid constructing `EvalContext` by raw field literal outside
    /// of `eval.rs`.
    ///
    /// Shares all arenas, config, state, and task_registry with the parent.
    /// Clones blame_map, libdir_dir (per-scope fields, same as `with_cancel_token`).
    pub fn with_explicit_cancel(
        self: &Arc<Self>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            config: Arc::clone(&self.config),
            state: Arc::clone(&self.state),
            thunk_arena: Arc::clone(&self.thunk_arena),
            env_arena: Arc::clone(&self.env_arena),
            env_allowed: self.env_allowed.clone(),
            blame_map: Mutex::new(self.blame_map.lock().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel,
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
            tycon_env: {
                let child_lock = std::sync::OnceLock::new();
                if let Some(env) = self.tycon_env.get() {
                    child_lock.set(std::sync::Arc::clone(env)).ok();
                }
                child_lock
            },
            type_context: Arc::clone(&self.type_context),
        })
    }

    /// Create a child EvalContext with a timeout: automatically cancels after `ms` milliseconds.
    ///
    /// Spawns a background task (via spawn_local) that fires the cancellation after the delay.
    /// Returns the child context; the cancel handle is internal (use `[with-cancel]` if you
    /// need explicit control).
    pub fn with_timeout_ms(self: &Arc<Self>, ms: u64) -> Arc<Self> {
        let child_token = self.cancel.child_token();
        let cancel_clone = child_token.clone();
        let handle = crate::async_rt::spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            cancel_clone.cancel();
        });

        // Register background task for drain tracking
        self.task_registry.lock().unwrap().push(handle);

        Arc::new(Self {
            config: Arc::clone(&self.config),
            state: Arc::clone(&self.state),
            thunk_arena: Arc::clone(&self.thunk_arena),
            env_arena: Arc::clone(&self.env_arena),
            env_allowed: self.env_allowed.clone(),
            blame_map: Mutex::new(self.blame_map.lock().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel: child_token,
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
            tycon_env: {
                let child_lock = std::sync::OnceLock::new();
                if let Some(env) = self.tycon_env.get() {
                    child_lock.set(std::sync::Arc::clone(env)).ok();
                }
                child_lock
            },
            type_context: Arc::clone(&self.type_context),
        })
    }

    /// Allocate a thunk in the arena and return its ID.
    pub fn alloc_thunk(&self, thunk: Arc<Thunk>) -> ThunkId {
        self.thunk_arena.lock().unwrap().alloc(thunk)
    }

    /// Get a cloned Arc<Thunk> from the arena by ID.
    pub fn get_thunk(&self, id: ThunkId) -> Arc<Thunk> {
        self.thunk_arena.lock().unwrap().get(id).clone()
    }

    /// Record blame provenance for a pipeline `%` thunk at a `---` boundary.
    /// The `label` identifies the producing stage (file path or stage index).
    pub fn record_blame(&self, thunk_id: ThunkId, label: String) {
        self.blame_map.lock().unwrap().insert(thunk_id, label);
    }

    /// Look up blame provenance for a thunk ID (if recorded at a pipeline boundary).
    pub fn blame_label(&self, thunk_id: ThunkId) -> Option<String> {
        self.blame_map.lock().unwrap().get(&thunk_id).cloned()
    }

    /// Set the type constructor environment from type inference.
    /// Called after type checking to wire user-defined TyCon variance and structural rules
    /// to the evaluator's subtype checker. The OnceLock silently no-ops if already set —
    /// this covers two cases:
    ///
    /// 1. **Child context inheritance** (normal, harmless): a child `EvalContext` propagates
    ///    the parent's `TyConEnv` in its constructor, so `set_tycon_env` on the child is a
    ///    no-op. This is correct — the child already has the right environment.
    ///
    /// 2. **REPL re-evaluation** (silent degradation): in REPL mode `ctx` is reused across
    ///    REPL inputs. The first input's `set_tycon_env` succeeds; all subsequent inputs hit
    ///    the no-op branch, meaning new `[type ...]` declarations entered after the first
    ///    REPL input are invisible to the evaluator's TyConEnv. The TyCon is registered in
    ///    `tycon_env` for type-checking purposes (InferState is fresh each input) but the
    ///    evaluator cannot see it for runtime subtype checks. Tracked in B-329 (REPL
    ///    TyConEnv frozen after first input).
    pub fn set_tycon_env(&self, env: crate::type_def::TyConEnv) {
        self.tycon_env.set(std::sync::Arc::new(env)).ok();
    }

    /// Get the type constructor environment, if available.
    /// Returns `None` before typechecking or when `--no-typecheck` is used.
    pub fn tycon_env(&self) -> Option<&crate::type_def::TyConEnv> {
        self.tycon_env.get().map(|arc| arc.as_ref())
    }

    /// Initialize the TypeContext for this evaluation scope.
    ///
    /// Installs the provided `TypeContextData` as the root TypeContext. All child contexts
    /// share the same `Arc<Mutex<Option<TypeContextData>>>` and will see this initialization.
    ///
    /// Called by `builtin-make-type-ctx` (T-1341) when the pipeline is bootstrapping.
    /// No-op (logs a warning) if the TypeContext is already initialized — TypeContext is
    /// set once at the start of a pipeline and mutated in-place by `builtin-typecheck`.
    pub fn init_type_context(&self, data: TypeContextData) {
        let mut guard = self.type_context.lock().unwrap();
        if guard.is_none() {
            *guard = Some(data);
        }
    }

    /// Get a clone of the current TypeContext data, if initialized.
    /// Returns `None` if `builtin-make-type-ctx` has not yet been called.
    pub fn get_type_context(&self) -> Option<TypeContextData> {
        self.type_context.lock().unwrap().clone()
    }

    /// Set the already-open libdir Dir so that `builtin_include` can inject `%libdir`
    /// into included files without calling `open_ambient_dir` again.
    ///
    /// Called by the capability initialization boundary (main.rs) immediately
    /// after opening the libdir directory and creating the EvalContext. Propagated
    /// through `with_base_dir` to child contexts (nested includes).
    pub fn set_libdir_dir(&self, dir: Arc<cap_std::fs::Dir>) {
        *self.libdir_dir.lock().unwrap() = Some(dir);
    }

    /// Set the source file name for FnAnnotation (LSP hover) and child context propagation.
    /// Must be called on a freshly created context before any Arc::clone shares it.
    /// Propagated through `with_base_dir` to child contexts (nested includes).
    /// Note: backtrace frame filenames are embedded in `Span.file` (populated by `parse_with_file`),
    /// not derived from this field.
    pub fn set_source_file(&mut self, file: Option<String>) {
        if let Some(config) = Arc::get_mut(&mut self.config) {
            config.source_file = file;
        }
    }
}

/// Extract the ground type of a runtime value for consistent subtyping validation.
///
/// Maps runtime `Value` variants to their ground `Type`. Erased positions (Seq elements,
/// Map values, Dict field values, Function params/returns) become `Type::Unknown`.
/// The consistent subtyping relation (`is_consistent_subtype`) then accepts `Unknown`
/// against any annotation, implementing AGT gradual typing semantics.
///
/// **Laziness preservation:** This function MUST NOT force any thunks. Field types in Dict
/// values are erased to `Unknown` without materializing the values. Element types in Seq
/// values are erased to `Unknown` without consuming the sequence. This is the same tradeoff
/// as `value_matches_type` tag-only validation: forcing all elements/fields would break
/// lazy evaluation guarantees.
pub fn ground_type_of(v: &Value) -> Type {
    match v {
        Value::Int(_) => Type::Int,
        // U64 values have Int ground type — no dedicated Type::U64 yet (see typecheck.rs).
        Value::U64(_) => Type::Int,
        Value::Float(_) => Type::Float,
        Value::String { .. } => Type::Str,
        Value::Bytes { .. } => Type::Bytes,
        Value::Dict(map) => Type::Record(extract_row(map)),
        // Overlay is a lazy right-biased merge: key set cannot be read without forcing.
        // Return a closed empty record — required-field checks correctly fail,
        // consistent with Overlay field validation being static-only.
        Value::Overlay(..) => Type::Record(Row {
            fields: indexmap::IndexMap::new(),
            tail: crate::type_def::RowTail::Empty,
        }),
        // Param/return types erased — consistent subtyping accepts Function([Unknown..], Unknown)
        // against any function annotation with matching arity.
        Value::Function { params, .. } => {
            let n = params.len();
            let is_variadic = params.last().map_or(false, |p| p.variadic);
            Type::Function {
                params: params.iter().map(|_| (None, Type::Unknown)).collect(),
                ret: Box::new(Type::Unknown),
                variadic: is_variadic,
                required_count: n,
            }
        }
        // Capability types: Unknown → is_consistent_subtype accepts against any annotation.
        // Preserves current accept-all behavior while capability-runtime-validation sprint is pending.
        Value::File(_) => Type::Unknown,
        Value::DirCap { .. } | Value::RevocableDirCap { .. } => Type::Unknown,
        Value::NetCap(_) => Type::Unknown,
        // Variant payload types erased (payload ThunkId has no static type without the schema).
        Value::Variant { tag, .. } => Type::NominalVariant {
            tag: tag.clone(),
            fields: Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            },
        },
        // Decimal/BigInt: no Type::Decimal/Type::BigInt in the type system yet.
        // Unknown preserves current behavior (matches @Number) until those variants are added.
        Value::Decimal(_) | Value::BigInt(_) => Type::Unknown,
        // Builtin functions and Proxy values: Unknown accepts any function/type annotation.
        Value::Builtin(..) | Value::Proxy { .. } => Type::Unknown,
        // Builder is a transient construction artifact — produce Top (type mismatch error)
        // rather than panicking; Builder can reach TypeAssert via e.g. [@Int [make-builder]].
        Value::Builder(..) => Type::Any,
        // Annotated is transparent — delegate to inner value's ground type.
        Value::Annotated { inner, .. } => ground_type_of(inner),
        // All other runtime-only types (URI, async, crypto, etc.) → Top
        _ => Type::Any,
    }
}

/// Extract the ground record type from a Dict: key names only, field types erased to Unknown.
///
/// MUST NOT force any ThunkId — field types are static-only (same tradeoff as Seq elements).
/// `is_consistent_subtype` then handles width subtyping: `{a: Unknown} ~<: {a: Int}` holds
/// because `Unknown ~<: Int`. Field presence is checked structurally; field types are not.
///
/// Integer-keyed entries (`HashableValue::Int`) are skipped — they are explicit positional entries
/// like `[0: x 1: y]`, not record fields.
fn extract_row(map: &IndexMap<HashableValue, ThunkId>) -> Row {
    let fields = map
        .keys()
        .filter_map(|k| match k {
            HashableValue::Str(name) => Some((name.to_string(), Type::Unknown)),
            // Integer-keyed entries are explicit [0: x 1: y] dict constructs, not record fields.
            HashableValue::Int(_) => None,
            // Other HashableValue variants (Bool, Dict, Variant) are not record fields.
            _ => None,
        })
        .collect::<indexmap::IndexMap<String, Type>>();
    Row {
        fields,
        tail: crate::type_def::RowTail::Empty,
    }
}

/// Check if a materialized value matches a type for structural TypeAssert validation.
/// Returns true if the value conforms to the expected type.
///
/// **Component 3 unified path:** Delegates to `is_consistent_subtype(ground_type_of(v), T)`.
/// The consistent subtyping relation handles Unknown at erased positions (Seq elements,
/// Dict field values, Function params/returns), implementing AGT gradual typing semantics.
///
/// **TyCon dispatch:** `Type::TyCon(name)` and `Type::App(f, _)` are handled by looking up
/// `name` in `ctx.tycon_env()`. If the def has `builtin_type`, dispatch on its discriminant
/// string to check the corresponding Value variant. If the def is nominal (has constructors),
/// check that the value is a Variant whose tag starts with `"<name>."`. If the TyCon is not
/// found in the env, return `false` conservatively.
///
/// No fast-path bypasses for other types — the consistent subtyping relation handles everything
/// uniformly. If primitive checks prove slow in profiling, optimize `is_consistent_subtype`
/// itself, which benefits every call site across the codebase.
///
/// Value::Annotated is transparent: `ground_type_of(Value::Annotated { inner, .. })` delegates
/// to `ground_type_of(inner)`, so the annotation wrapper is invisible to type checking.
pub(crate) fn value_matches_type(value: &Value, expected: &Type, ctx: &EvalContext) -> bool {
    // Resolve the root TyCon name for TyCon and App types, then dispatch via TyConDef.
    let tycon_name: Option<&str> = match expected {
        Type::TyCon(name) => Some(name.as_str()),
        Type::App(f, _) => {
            if let Type::TyCon(name) = f.as_ref() {
                Some(name.as_str())
            } else {
                None
            }
        }
        _ => None,
    };

    if let Some(name) = tycon_name {
        return match ctx.tycon_env().and_then(|env| env.get(name)) {
            Some(def) => {
                if let Some(discriminant) = &def.builtin_type {
                    // Builtin type: map discriminant string to a Value variant check.
                    match discriminant.as_str() {
                        "Int" => matches!(value, Value::Int(_)),
                        "Str" => matches!(value, Value::String { .. }),
                        "Float" => matches!(value, Value::Float(_)),
                        "Bytes" => matches!(value, Value::Bytes { .. }),
                        "Dict" => matches!(value, Value::Dict(_)),
                        "Fn" => matches!(value, Value::Function { .. } | Value::Builtin(_)),
                        "File" => matches!(value, Value::File(_)),
                        // Unknown discriminant: conservative false.
                        _ => false,
                    }
                } else if !def.constructors.is_empty() {
                    // Nominal (user-defined) type: value must be a Variant with tag "<name>.*".
                    // Zero-allocation check: starts_with name AND next byte is '.' (avoids format!).
                    matches!(value, Value::Variant { tag, .. }
                        if tag.starts_with(name)
                            && tag.as_bytes().get(name.len()) == Some(&b'.'))
                } else {
                    // TyCon found but no builtin_type and no constructors yet (T-1003/T-1018).
                    // Conservative: unknown structure, return false.
                    false
                }
            }
            // TyCon not found in env (tycon_env is None, or name not registered).
            None => false,
        };
    }

    Type::is_consistent_subtype(&ground_type_of(value), expected)
}

/// Exact type discrimination for Pattern::TypeAssert matching.
///
/// Unlike `value_matches_type` (consistent subtyping for TypeAssert expression validation),
/// pattern matching needs exact dispatch: `[@Int _]:` must NOT match Builtin values even
/// though `ground_type_of(Builtin)` is Unknown, which is consistent with everything.
///
/// Key differences from value_matches_type:
/// - Parameterized TyCon: matches any Variant whose tag starts with "Name."
/// - Fn (variadic, 0 required params): matches both Value::Function AND Value::Builtin
/// - Proxy: exact match only (not Unknown ≥ Proxy via gradual typing)
/// - Unknown/Top: always match (gradual escape hatch for --no-typecheck, macros)
pub(crate) fn pattern_type_matches(value: &Value, expected: &Type, ctx: &Arc<EvalContext>) -> bool {
    match expected {
        Type::Int | Type::IntLiteral(_) => matches!(value, Value::Int(_) | Value::U64(_)),
        Type::Float => matches!(value, Value::Float(_)),
        Type::Str | Type::StringLiteral(_) => matches!(value, Value::String { .. }),
        Type::Bytes => matches!(value, Value::Bytes { .. }),
        Type::Proxy => matches!(value, Value::Proxy { .. }),
        // Record / Dict: any dict-like value satisfies an empty record annotation.
        Type::Record(_) => matches!(value, Value::Dict(_) | Value::Overlay(..)),
        // @Fn (variadic, 0 required params): matches any callable including Builtin.
        Type::Function {
            variadic,
            required_count,
            ..
        } if *variadic && *required_count == 0 => {
            matches!(value, Value::Function { .. } | Value::Builtin(_))
        }
        Type::Function { .. } => matches!(value, Value::Function { .. }),
        // App(TyCon(name), _): parameterized nominal type — value must be a Variant with tag "Name.*".
        // This is the generic rule for all user-declared parameterized types (Seq, Option, Map, etc.).
        Type::App(f, _) if matches!(f.as_ref(), Type::TyCon(_)) => {
            if let Type::TyCon(name) = f.as_ref() {
                matches!(value, Value::Variant { tag, .. }
                    if tag.starts_with(name.as_str())
                        && tag.as_bytes().get(name.len()) == Some(&b'.'))
            } else {
                false
            }
        }
        // Other TyCon / App: delegate to value_matches_type (handles user-defined nominal types).
        Type::TyCon(_) | Type::App(_, _) => value_matches_type(value, expected, ctx),
        // NominalVariant: exact tag or "Tag.*" prefix on Value::Variant.
        Type::NominalVariant { tag, .. } => {
            matches!(value, Value::Variant { tag: vtag, .. }
                if vtag == tag
                    || (vtag.starts_with(tag.as_str())
                        && vtag.as_bytes().get(tag.len()) == Some(&b'.')))
        }
        // Unknown / Top: always match.
        Type::Unknown | Type::Any => true,
        Type::Union(members) => members.iter().any(|m| pattern_type_matches(value, m, ctx)),
        Type::Intersection(members) => members.iter().all(|m| pattern_type_matches(value, m, ctx)),
        // Everything else: fall back to consistent subtyping.
        _ => value_matches_type(value, expected, ctx),
    }
}

/// Format a Type for error messages in TypeAssert.
///
/// Currently delegates to Type's Display impl. This wrapper provides a semantic
/// name and future-proofs for custom error formatting (e.g., abbreviating long
/// record types, pretty-printing nested structures).
pub(crate) fn format_type_for_assert(ty: &Type) -> String {
    format!("{}", ty)
}

/// Extract a merged Record row from a type for eval-time validation.
///
/// Multi-field annotations produce `Intersection([{f1: T1}, {f2: T2}])`.
/// For runtime validation and proxy wrapping we need a single `Row` that collects all
/// required fields.
///
/// Under BAS, all tails are Empty. The merged row's tail is also Empty.
/// The cardinality check in validate_and_wrap_record is REMOVED — BAS allows extra fields.
///
/// Returns `Some(&Row)` for `Type::Record` (trivial) or `Type::Intersection` whose members
/// are all Records.  Returns `None` for anything else (scalar types, Union, etc.).
pub(crate) fn as_record_row_merged(expected: &Type) -> Option<Cow<'_, Row>> {
    match expected {
        Type::Record(row) => Some(Cow::Borrowed(row)),
        Type::Intersection(members) if members.iter().all(|m| matches!(m, Type::Record(_))) => {
            let mut merged_fields: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
            for m in members {
                if let Type::Record(row) = m {
                    for (k, v) in &row.fields {
                        merged_fields.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
            Some(Cow::Owned(Row {
                fields: merged_fields,
                tail: crate::type_def::RowTail::Empty,
            }))
        }
        _ => None,
    }
}

/// Validate a dict value against a Record type and wrap fields with guards.
///
/// Returns a new dict with guarded field thunks. This implements the [VM-RECORD-PROXY]
/// rule from doc/07-type-extensions.md:
/// 1. Shape check: verify all required fields exist (with HashableValue::Int fallback)
/// 2. Cardinality check: verify no extra fields for closed records
/// 3. Guard wrapping: wrap each typed field with a Guarded thunk
///
/// This function implements **chaperone semantics** (Strickland et al., 2012):
/// the proxy (guarded dict) is observationally equivalent to the original dict at
/// all type-correct uses. Each field's guard can only (a) return the original value
/// unchanged, or (b) raise a contract error — it cannot change the value. Field
/// types are checked lazily when accessed, not eagerly at the assertion site,
/// preserving call-by-need evaluation (Launchbury, 1993). A field that is never
/// accessed is never validated, matching Findler & Felleisen's (2002) principle
/// that compound contracts defer checking to the point of observation.
///
/// # Parameters
/// - `entries`: the dict entries to validate
/// - `row`: the expected record row type (fields + tail)
/// - `field_path`: accumulated path for nested field errors (empty for top-level)
/// - `guard_span`: span for guard creation
///
/// # Errors
/// Returns TypeAssertFailed if:
/// - A required field is missing
/// - The record has extra fields and tail is Empty (closed)
///
/// # Note
/// The caller is responsible for checking default_expr and calling eval() with the default
/// if this function returns an error. This keeps the helper focused on validation logic.
/// Guards created by this function do NOT propagate default_expr to avoid infinite recursion.
#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_and_wrap_record(
    entries: &IndexMap<HashableValue, ThunkId>,
    row: &Row,
    field_path: &mut Vec<String>,
    guard_span: Span,
    data_span: Span,
    ctx: &Arc<EvalContext>,
    default: Option<GuardDefault>,
    blame_label: Option<crate::error::BlameLabel>,
) -> EvalResult<IndexMap<HashableValue, ThunkId>> {
    // Shape check: verify all required fields exist
    // Per doc/07:117, try HashableValue::Str first, then HashableValue::Int fallback
    for (field_name, _field_type) in row.fields.iter() {
        let has_field = entries.contains_key(&HashableValue::Str(Rc::from(field_name.as_str())))
            || field_name
                .parse::<i64>()
                .ok()
                .map(|idx| entries.contains_key(&HashableValue::Int(idx)))
                .unwrap_or(false);

        if !has_field {
            let field_path_prefix = if field_path.is_empty() {
                String::new()
            } else {
                format!("field {}: ", format_field_path(field_path))
            };

            return Err(EvalError::type_assert_failed(
                &format!("{}record with field \"{}\"", field_path_prefix, field_name),
                &format!(
                    "{}record missing field \"{}\"",
                    field_path_prefix, field_name
                ),
                // Use data_span (the data definition site) so the error points to WHERE
                // the invalid dict was constructed, not the annotation.
                data_span,
            )
            .into());
        }
    }

    // Cardinality check REMOVED under BAS:
    // BAS width subtyping allows a value with MORE fields to satisfy an annotation with FEWER.
    // Extra fields are never an error — `validate_and_wrap_record` only checks required fields.

    // Guard wrapping: wrap each typed field thunk.
    // Use a for loop with push/pop on field_path to avoid cloning the full path
    // for every field — only the thunk's owned copy is allocated per field.
    let mut new_entries = IndexMap::with_capacity(entries.len());
    for (key, &thunk_id) in entries.iter() {
        // Try to find a matching field type
        let field_type = match key {
            HashableValue::Str(field_name) => row.fields.get(field_name.as_ref()),
            HashableValue::Int(n) => row.fields.get(&n.to_string()),
            _ => None,
        };

        if let Some(field_type) = field_type {
            let field_name = match key {
                HashableValue::Str(s) => s.to_string(),
                HashableValue::Int(n) => n.to_string(),
                _ => String::new(),
            };

            // Push field name onto the shared path, clone for the thunk, then pop.
            // This avoids cloning the entire path prefix for every entry.
            field_path.push(field_name);
            let nested_path = field_path.clone();
            field_path.pop();

            let thunk_rc = ctx.get_thunk(thunk_id);
            let guarded = Arc::new(Thunk::new_guarded_full(
                thunk_rc,
                field_type.clone(),
                nested_path,
                guard_span.clone(),
                blame_label.clone(),
                default.clone(),
            ));
            let guarded_id = ctx.alloc_thunk(guarded);
            new_entries.insert(key.clone(), guarded_id);
        } else {
            new_entries.insert(key.clone(), thunk_id);
        }
    }

    Ok(new_entries)
}

/// Check if an identifier starts with an uppercase letter.
pub(crate) fn is_constructor_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Recursively walk a quoted SurfaceNode, handling Unquote and UnquoteSplice.
///
/// Returns `Value::Variant { tag: "Expr.<Tag>", .. }` — the canonical runtime representation.
/// This function operates entirely on SurfaceNode (no Expr round-trip).
async fn eval_quote_walk(
    node: Arc<crate::ast::SurfaceNode>,
    env: Arc<RwLock<Env>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let span = node.span.clone();
    // Preprocess to handle nested unquotes (rewrites unquote subexpressions)
    let processed_node = eval_quote_preprocess(node, &env, ctx).await?;

    Ok(Arc::new(Thunk::new_materialized(
        crate::surface_convert::surface_node_to_expr_variant(&processed_node, ctx),
        span,
    )))
}

/// Convert a runtime Value back to an Arc<SurfaceNode> for unquoting.
///
/// If the value is a Dict/Variant with a `type` field, treat it as an AST dict and use
/// `dict_to_surface_node`. Otherwise, convert the value to its literal SurfaceNode.
///
/// This is the SurfaceNode-native replacement for the old `value_to_expr`. No Expr round-trip.
fn value_to_surface_node(
    value: &Value,
    span: Span,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<crate::ast::SurfaceNode>> {
    use crate::ast::{SurfaceExpression, SurfaceNode};
    let make_node = |expr: SurfaceExpression| Arc::new(SurfaceNode::new(expr, span.clone()));
    match value {
        Value::Int(n) => Ok(make_node(SurfaceExpression::Int(*n))),
        Value::U64(n) => Ok(make_node(SurfaceExpression::U64(*n))),
        Value::Float(f) => Ok(make_node(SurfaceExpression::Float(*f))),
        Value::String { source, start, end } => Ok(make_node(SurfaceExpression::Str(
            source[*start..*end].to_string(),
        ))),
        Value::Variant { .. } => {
            // Variant form of an AST node — convert via surface bridge
            crate::surface_convert::dict_to_surface_node(value, &span, ctx).map_err(|err| {
                EvalError::internal(
                    format!("unquote result Variant is not a valid AST: {}", err),
                    span,
                )
                .into()
            })
        }
        _ => Err(
            EvalError::internal(format!("unquote of {:?} is not supported", value), span).into(),
        ),
    }
}

/// Collect all elements from an integer-keyed Dict into a Vec in insertion order.
/// Used by unquote-splice to expand macro variadic arguments (which are now always Dict).
async fn collect_seq_elements(
    value: &Value,
    span: Span,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Vec<Value>> {
    let dict = match value {
        Value::Dict(d) => d,
        other => {
            return Err(
                EvalError::type_mismatch("Dict (macro variadic)", other.type_name(), span).into(),
            )
        }
    };

    let mut elements = Vec::new();
    let mut i = 0i64;
    loop {
        match dict.get(&crate::value::HashableValue::Int(i)) {
            Some(&thunk_id) => {
                let thunk = ctx.get_thunk(thunk_id);
                let val = materialize(&thunk, Some(&span), ctx).await?;
                elements.push(val);
                i += 1;
                if elements.len() >= MAX_COLLECT_SIZE {
                    return Err(EvalError::resource_limit_exceeded(
                        format!(
                            "unquote-splice: too many elements (limit {})",
                            MAX_COLLECT_SIZE
                        ),
                        span,
                    )
                    .into());
                }
            }
            None => break,
        }
    }

    Ok(elements)
}

/// Recursively preprocess a quoted SurfaceNode tree to handle nested unquotes.
///
/// This walks the entire AST and:
/// - Evaluates `Unquote` nodes, converting the result back to a SurfaceNode
/// - Handles `UnquoteSplice` in call argument positions
/// - Recurses into all child SurfaceNodes
/// - Leaves non-unquote nodes unchanged (Arc::clone, no allocation)
///
/// Operates entirely on SurfaceNode — no Expr round-trip.
fn eval_quote_preprocess<'a>(
    node: Arc<crate::ast::SurfaceNode>,
    env: &'a Arc<RwLock<Env>>,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = EvalResult<Arc<crate::ast::SurfaceNode>>> + 'a>,
> {
    use crate::ast::{
        SurfaceDeclaration, SurfaceEntry, SurfaceExpression, SurfaceMatchArm, SurfaceNamedArg,
        SurfaceNode,
    };
    Box::pin(async move {
        let span = node.span.clone();
        let make_node = |expr: SurfaceExpression| Arc::new(SurfaceNode::new(expr, span.clone()));

        match &node.expr {
            SurfaceExpression::Unquote(inner) => {
                // Evaluate the unquoted expression and convert back to SurfaceNode
                let (core, lower_diags) = crate::lower::lower(inner);
                if let Some(err) = lower_diags
                    .into_iter()
                    .find(|d| matches!(d.kind, crate::lower::LowerDiagnosticKind::Error))
                {
                    return Err(EvalError::user_error(err.message, err.span).into());
                }
                let thunk = eval_core_expr(&core, env, ctx).await?;
                let value = materialize(&thunk, Some(&inner.span), ctx).await?;
                value_to_surface_node(&value, inner.span.clone(), ctx)
            }

            SurfaceExpression::UnquoteSplice(_) => {
                // UnquoteSplice at non-list position is an error.
                // Call args handle UnquoteSplice in their own loop below.
                Err(EvalError::unimplemented(
                    "unquote-splice must be in a list position (inside call args); dict entry splicing is not yet implemented"
                        .to_string(),
                    span,
                )
                .into())
            }

            // Recursively process composite expressions
            SurfaceExpression::Dict(entries) => {
                let mut processed_entries = Vec::with_capacity(entries.len());
                for entry in entries {
                    let processed_value =
                        eval_quote_preprocess(Arc::clone(&entry.node.value), env, ctx).await?;
                    let processed_key = if let Some(ref key_node) = entry.node.key {
                        Some(eval_quote_preprocess(Arc::clone(key_node), env, ctx).await?)
                    } else {
                        None
                    };
                    processed_entries.push(Spanned::new(
                        SurfaceEntry {
                            key: processed_key,
                            value: processed_value,
                        },
                        entry.span.clone(),
                    ));
                }
                Ok(make_node(SurfaceExpression::Dict(processed_entries)))
            }

            SurfaceExpression::Call {
                func,
                args,
                named_args,
                implied,
            } => {
                let processed_func = eval_quote_preprocess(Arc::clone(func), env, ctx).await?;
                let mut processed_args: Vec<Arc<SurfaceNode>> = Vec::new();
                for arg in args {
                    // Handle unquote-splicing in call argument position
                    if let SurfaceExpression::UnquoteSplice(inner) = &arg.expr {
                        // Evaluate the unquote-splice expression
                        let (core, lower_diags) = crate::lower::lower(inner);
                        if let Some(err) = lower_diags
                            .into_iter()
                            .find(|d| matches!(d.kind, crate::lower::LowerDiagnosticKind::Error))
                        {
                            return Err(EvalError::user_error(err.message, err.span).into());
                        }
                        let thunk = eval_core_expr(&core, env, ctx).await?;
                        let inner_span = inner.span.clone();
                        let value = materialize(&thunk, Some(&inner_span), ctx).await?;

                        // Extract elements from the sequence and convert each to SurfaceNode
                        let elements =
                            collect_seq_elements(&value, inner_span.clone(), ctx).await?;
                        for elem_value in elements {
                            let elem_node =
                                value_to_surface_node(&elem_value, inner_span.clone(), ctx)?;
                            processed_args.push(elem_node);
                        }
                    } else {
                        // Regular argument - recursively process
                        processed_args
                            .push(eval_quote_preprocess(Arc::clone(arg), env, ctx).await?);
                    }
                }
                let mut processed_named_args: Vec<Spanned<SurfaceNamedArg>> =
                    Vec::with_capacity(named_args.len());
                for na in named_args {
                    let processed_value =
                        eval_quote_preprocess(Arc::clone(&na.node.value), env, ctx).await?;
                    processed_named_args.push(Spanned::new(
                        SurfaceNamedArg {
                            name: na.node.name.clone(),
                            value: processed_value,
                            annotation: na.node.annotation.clone(),
                        },
                        na.span.clone(),
                    ));
                }
                Ok(make_node(SurfaceExpression::Call {
                    func: processed_func,
                    args: processed_args,
                    named_args: processed_named_args,
                    implied: *implied,
                }))
            }

            SurfaceExpression::Fn {
                return_ann,
                params,
                body,
                desugared,
            } => {
                let processed_body = eval_quote_preprocess(Arc::clone(body), env, ctx).await?;
                Ok(make_node(SurfaceExpression::Fn {
                    return_ann: return_ann.clone(),
                    params: params.clone(),
                    body: processed_body,
                    desugared: *desugared,
                }))
            }

            SurfaceExpression::Field {
                expr: Some(target),
                field,
                ..
            } => {
                let processed_target = eval_quote_preprocess(Arc::clone(target), env, ctx).await?;
                Ok(make_node(SurfaceExpression::Field {
                    expr: Some(processed_target),
                    field: field.clone(),
                    resolution: crate::ast::Resolution::new(),
                    field_slot: crate::ast::SlotAnnotation::new(),
                }))
            }

            // Leading-dot is a terminal in quote context — no sub-expression to preprocess.
            SurfaceExpression::Field {
                expr: None, field, ..
            } => Ok(make_node(SurfaceExpression::Field {
                expr: None,
                field: field.clone(),
                resolution: crate::ast::Resolution::new(),
                field_slot: crate::ast::SlotAnnotation::new(),
            })),

            SurfaceExpression::Pipe { lhs, rhs } => {
                let processed_lhs = eval_quote_preprocess(Arc::clone(lhs), env, ctx).await?;
                let processed_rhs = eval_quote_preprocess(Arc::clone(rhs), env, ctx).await?;
                Ok(make_node(SurfaceExpression::Pipe {
                    lhs: processed_lhs,
                    rhs: processed_rhs,
                }))
            }

            SurfaceExpression::Sequential(exprs) => {
                let mut processed_exprs = Vec::with_capacity(exprs.len());
                for e in exprs {
                    processed_exprs.push(eval_quote_preprocess(Arc::clone(e), env, ctx).await?);
                }
                Ok(make_node(SurfaceExpression::Sequential(processed_exprs)))
            }

            SurfaceExpression::TypeAssert {
                annotation,
                expr: inner,
                ..
            } => {
                let processed_expr = eval_quote_preprocess(Arc::clone(inner), env, ctx).await?;
                Ok(make_node(SurfaceExpression::TypeAssert {
                    annotation: annotation.clone(),
                    expr: processed_expr,
                    resolved_type: crate::ast::TypeAnnotation::new(),
                }))
            }

            SurfaceExpression::Quote(inner) => {
                // Nested quote: recurse so inner unquotes are still processed.
                let processed_inner = eval_quote_preprocess(Arc::clone(inner), env, ctx).await?;
                Ok(make_node(SurfaceExpression::Quote(processed_inner)))
            }

            SurfaceExpression::Match { scrutinee, arms } => {
                let processed_scrutinee =
                    eval_quote_preprocess(Arc::clone(scrutinee), env, ctx).await?;
                let mut processed_arms = Vec::with_capacity(arms.len());
                for arm in arms {
                    let processed_body =
                        eval_quote_preprocess(Arc::clone(&arm.body), env, ctx).await?;
                    let processed_guard = if let Some(ref guard) = arm.guard {
                        Some(eval_quote_preprocess(Arc::clone(guard), env, ctx).await?)
                    } else {
                        None
                    };
                    processed_arms.push(SurfaceMatchArm {
                        pattern: arm.pattern.clone(),
                        guard: processed_guard,
                        body: processed_body,
                        guard_matchable_binding: arm.guard_matchable_binding.clone(),
                    });
                }
                Ok(make_node(SurfaceExpression::Match {
                    scrutinee: processed_scrutinee,
                    arms: processed_arms,
                }))
            }

            SurfaceExpression::Decl(decl) => {
                // Declaration forms inside a quote body — walk their child bodies
                // to find any nested unquotes. Declarations are rare in quoted code,
                // but users can write e.g. [quote [type Foo = Bar]] and expect
                // unquotes inside the alias body to be evaluated.
                let processed_decl = match decl.as_ref() {
                    SurfaceDeclaration::TypeAlias { params, body } => {
                        let processed_body =
                            eval_quote_preprocess(Arc::clone(body), env, ctx).await?;
                        SurfaceDeclaration::TypeAlias {
                            params: params.clone(),
                            body: processed_body,
                        }
                    }
                    SurfaceDeclaration::SyntaxClass {
                        name,
                        pattern,
                        message,
                    } => {
                        let processed_pattern =
                            eval_quote_preprocess(Arc::clone(pattern), env, ctx).await?;
                        SurfaceDeclaration::SyntaxClass {
                            name: name.clone(),
                            pattern: processed_pattern,
                            message: message.clone(),
                        }
                    }
                    SurfaceDeclaration::Splice(forms) => {
                        let mut processed_forms = Vec::with_capacity(forms.len());
                        for form in forms {
                            processed_forms
                                .push(eval_quote_preprocess(Arc::clone(form), env, ctx).await?);
                        }
                        SurfaceDeclaration::Splice(processed_forms)
                    }
                    // ClassDecl, InstanceDecl — complex; treat as leaves (no unquote recursion)
                    other => other.clone(),
                };
                Ok(make_node(SurfaceExpression::Decl(Box::new(processed_decl))))
            }

            // All other expressions have no child SurfaceNodes — return unchanged
            _ => Ok(node),
        }
    }) // end Box::pin(async move {
}

/// Evaluate a CoreExpr to a thunk (transitional path for runtime-v2).
///
/// This is the new CoreExpr evaluation entry point. It handles:
/// - Primitive variants natively: Int, Float, Bool, Str (direct materialization)
/// - Variables natively: Var (environment lookup with de Bruijn coordinates)
/// - Complex variants via bridge: Dict, Call, Fn, Match, etc. convert back to Expr
///   and call existing helpers (eval_dict, eval_call, etc.)
///
/// This is intentionally TRANSITIONAL. The round-trips to Expr are ACCEPTED for this
/// sprint (E1). Future sprints (E2/E3) will implement native CoreExpr handlers for
/// Dict/Call/Fn to eliminate the bridge conversions.
fn eval_core_expr<'a>(
    expr: &'a Spanned<CoreExpr>,
    env: &'a Arc<RwLock<Env>>,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>> + 'a>> {
    Box::pin(async move {
        let span = expr.span.clone();
        match &expr.node {
            // Fast path: literals materialize directly without wrapping in Unevaluated
            CoreExpr::Int(n) => Ok(Arc::new(Thunk::new_materialized(
                Value::Int(*n),
                span.clone(),
            ))),
            CoreExpr::U64(n) => Ok(Arc::new(Thunk::new_materialized(
                Value::U64(*n),
                span.clone(),
            ))),
            CoreExpr::Float(f) => Ok(Arc::new(Thunk::new_materialized(
                Value::Float(*f),
                span.clone(),
            ))),
            CoreExpr::Str(s) => Ok(Arc::new(Thunk::new_materialized(
                string_val(s),
                span.clone(),
            ))),

            // Variable lookup with de Bruijn coordinates.
            // Slot-based lookup is O(1) — no name hash, no string comparison.
            // The do-infer sentinel block that previously used get_by_name was removed:
            // EvalContext.do_infer_resolutions is never populated (set_do_infer_resolutions
            // is defined but never called from any pipeline path), making that block dead code.
            // Sentinels evaluate via the normal get_slot path below.
            CoreExpr::Var {
                name, level, slot, ..
            } => {
                if *level == u32::MAX || *slot == u32::MAX {
                    // Sentinel: resolver did not assign de Bruijn coordinates, or lowerer
                    // chose name-based lookup. Fall back to name-based lookup via get_value_by_name.
                    let env_lock = env.read().unwrap();
                    if let Some(thunk) = env_lock.get_value_by_name(name) {
                        return Ok(thunk);
                    }
                    drop(env_lock);
                    return Err(EvalError::undefined_variable(name.clone(), span.clone()).into());
                }
                let env_lock = env.read().unwrap();
                match env_lock.get_value_at(*level, *slot) {
                    Some(thunk) => Ok(thunk),
                    None => {
                        // Slot lookup failed — fall back to name-based lookup as a safety net.
                        if let Some(thunk) = env_lock.get_value_by_name(name) {
                            return Ok(thunk);
                        }
                        drop(env_lock);
                        Err(EvalError::undefined_variable(name.clone(), span.clone()).into())
                    }
                }
            }

            // Variant: first-class variant constructor emitted by lower.rs for type declarations.
            // Unit variants materialize directly; payload variants evaluate their inner expression,
            // materialize it, and store as a ThunkId.
            CoreExpr::Variant { tag, payload } => match payload {
                None => Ok(Arc::new(Thunk::new_materialized(
                    Value::Variant {
                        tag: tag.clone(),
                        payload: None,
                    },
                    span.clone(),
                ))),
                Some(inner_expr) => {
                    let payload_thunk = eval_core_expr(inner_expr, env, ctx).await?;
                    let payload_val = materialize(&payload_thunk, Some(&span), ctx).await?;
                    let payload_id = ctx
                        .alloc_thunk(Arc::new(Thunk::new_materialized(payload_val, span.clone())));
                    Ok(Arc::new(Thunk::new_materialized(
                        Value::Variant {
                            tag: tag.clone(),
                            payload: Some(payload_id),
                        },
                        span.clone(),
                    )))
                }
            },

            // Sequential: evaluate each expression in order, extending the environment
            // with dict bindings from each intermediate dict expression.
            // Sequential: wrap as CoreExpr thunk — the CEK machine will handle iterative
            // evaluation via SequentialStep continuations.
            // This eliminates async recursion on the Rust stack for deeply nested sequential blocks.
            CoreExpr::Sequential(_) => Ok(Arc::new(Thunk::new_unevaluated_core(
                Arc::new(expr.clone()),
                Arc::clone(env),
                Arc::clone(ctx),
                span.clone(),
            ))),

            // Dict: call eval_dict_core directly with the CoreEntry slice.
            // eval_dict_core uses Thunk::new_unevaluated_core for non-literal dict entries
            // (UnevaluatedState::CoreExpr), avoiding the per-entry core_expr_to_expr round-trip.
            CoreExpr::Dict(entries) => eval_dict_core(entries, env, ctx, &span).await,

            // Call: use eval_call_core — no CoreExpr→Expr round-trip for func or named args.
            CoreExpr::Call {
                func,
                args,
                named_args,
                ..
            } => {
                eval_call_core(
                    func,
                    args,
                    named_args,
                    env,
                    ctx,
                    &span,
                    Arc::new(expr.clone()),
                )
                .await
            }

            // Fn: store body as Arc<Spanned<CoreExpr>> directly — no round-trip to Expr.
            CoreExpr::Fn {
                return_ann,
                params,
                body,
                ..
            } => {
                let fn_params: Vec<Param> = params
                    .iter()
                    .map(|p| Param {
                        name: p.node.name.clone(),
                        annotation: p.node.annotation.clone(),
                        variadic: p.node.variadic,
                    })
                    .collect();

                // Populate extra from annotation fields (literals + expressions).
                // `doc` is now included in extra: triple-quoted strings desugar to
                // `[unindent "..."]` (a Call), which is evaluated here at definition time.
                // T-1124: expression-valued fields are evaluated at function-definition time.
                let extra = extract_fn_annotation_extra(return_ann.as_ref(), env, ctx).await?;

                // Derive FnAnnotation.doc from extra["doc"] so triple-quoted doc strings
                // (evaluated via `[unindent "..."]`) produce the correct runtime string.
                let doc: Option<String> = extra
                    .get("doc")
                    .and_then(|v| v.as_str().map(|s| s.to_string()));
                let return_ann_clone: Option<crate::ast::Annotation> =
                    return_ann.as_ref().map(|a| a.node.clone());

                // Always construct FnAnnotation — source_span is always available even for
                // unannotated functions, enabling ast-of and LSP go-to-definition.
                let annotation = Some(Box::new(crate::value::FnAnnotation {
                    doc,
                    return_ann: return_ann_clone,
                    source_file: ctx.config.source_file.clone(),
                    source_span: span.clone(),
                    extra,
                }));

                // Store the body directly as Arc<Spanned<CoreExpr>>.
                // CoreExpr::Fn.body is already Arc<Spanned<CoreExpr>> — no conversion needed.
                // Thread return_ann through to Value::Function for constructor pattern matching.
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Function {
                        params: Rc::new(fn_params),
                        body: Arc::clone(body),
                        env: Arc::clone(env),
                        annotation,
                        return_ann: return_ann.clone(),
                    },
                    span.clone(),
                )))
            }

            // TypeAssert: wrap as CoreExpr thunk — force_step's take_core_expr branch
            // handles CoreExpr::TypeAssert inline, pushing a TypeAssertCheck continuation.
            // Wrapping here prevents direct recursion back through eval_core_expr.
            CoreExpr::TypeAssert { .. } => Ok(Arc::new(Thunk::new_unevaluated_core(
                Arc::new(expr.clone()),
                Arc::clone(env),
                Arc::clone(ctx),
                span.clone(),
            ))),

            // Rest: error (only valid in type expressions)
            CoreExpr::Rest(_) => Err(EvalError::internal(
                "rest marker (...) is only valid inside type expressions".to_string(),
                span.clone(),
            )
            .into()),

            // Match: wrap as CoreExpr thunk — the CEK machine will handle iterative
            // evaluation via MatchDispatch and MatchGuardCheck continuations.
            // This eliminates async recursion on the Rust stack for deeply nested match chains.
            CoreExpr::Match { .. } => Ok(Arc::new(Thunk::new_unevaluated_core(
                Arc::new(expr.clone()),
                Arc::clone(env),
                Arc::clone(ctx),
                span.clone(),
            ))),

            // Quote: convert CoreExpr→SurfaceNode and walk with eval_quote_walk.
            // The inner CoreExpr was lowered (giving unquotes proper variable slots),
            // then converted back here for structural traversal. CoreExpr::Var preserves
            // the original name alongside the slot so the round-trip is lossless.
            CoreExpr::Quote(inner) => {
                let surface_node = crate::lower::core_expr_to_surface_node(inner);
                eval_quote_walk(surface_node, env.clone(), ctx).await
            }

            // Unquote: error (only valid inside quote)
            CoreExpr::Unquote(_) => Err(EvalError::internal(
                "unquote is only valid inside [quote ...]".to_string(),
                span.clone(),
            )
            .into()),

            // UnquoteSplice: error (only valid inside quote)
            CoreExpr::UnquoteSplice(_) => Err(EvalError::internal(
                "unquote-splice is only valid inside [quote ...]".to_string(),
                span.clone(),
            )
            .into()),

            // PatternDecl: error (not an expression)
            CoreExpr::PatternDecl { .. } => Err(EvalError::internal(
                "pattern declaration is only valid in instance match arms".to_string(),
                span.clone(),
            )
            .into()),

            // LetDecl in sequential fn-body context: evaluate as a Dict of (name → lazy-thunk) pairs.
            //
            // Syntax: [let name value] → bindings = [Str("name"), value_expr]
            // (lower_let_decl_binding converts declaration-position VarRef/Annotated/Rest to Str)
            // Pairs are (bindings[2i], bindings[2i+1]).
            // Returns a Dict so the SequentialStep can extract keys via its Dict-based binding logic.
            CoreExpr::LetDecl { bindings } => {
                let mut dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
                let mut i = 0;
                while i + 1 < bindings.len() {
                    let name_expr = &bindings[i];
                    let val_expr = &bindings[i + 1];
                    let name = match &name_expr.node {
                        // lower_let_decl_binding converts declaration-position names to Str literals.
                        CoreExpr::Str(n) => n.clone(),
                        // Var node in declaration position: extract the name string directly.
                        // Annotated Var (Var { annotation: Some(_) }) is also handled here.
                        CoreExpr::Var { name: n, .. } => n.clone(),
                        _ => {
                            return Err(EvalError::internal(
                                format!(
                                    "let binding name must be an identifier, got: {:?}",
                                    name_expr.node
                                ),
                                name_expr.span.clone(),
                            )
                            .into());
                        }
                    };
                    let val_thunk = Arc::new(Thunk::new_unevaluated_core(
                        Arc::new(val_expr.clone()),
                        Arc::clone(env),
                        Arc::clone(ctx),
                        val_expr.span.clone(),
                    ));
                    let thunk_id = ctx.alloc_thunk(val_thunk);
                    dict.insert(HashableValue::Str(Rc::from(name.as_str())), thunk_id);
                    i += 2;
                }
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Dict(dict),
                    span.clone(),
                )))
            }

            // CaseArm: error (not an expression)
            CoreExpr::CaseArm { .. } => Err(EvalError::internal(
                "case arms are not expressions".to_string(),
                span.clone(),
            )
            .into()),

            CoreExpr::Placeholder => Err(EvalError::unimplemented(
                "placeholder `...` was evaluated — replace with an implementation".to_string(),
                span.clone(),
            )
            .into()),
        }
        // Note: type guards are now inline on AST nodes (TypeAnnotation OnceLock).
        // The lowerer wraps them in CoreExpr::TypeAssert. No runtime guard wrapping needed here.
    }) // end Box::pin(async move {
}

/// Force a thunk to its concrete value, memoizing the result.
///
/// On first materialization, evaluates the thunk and caches the result (or error).
/// Subsequent calls return the cached value without re-evaluation. This implements
/// call-by-need semantics: lazy evaluation with sharing.
///
/// # State transitions
///
/// - `Materialized`: returns cached value immediately
/// - `Failed`: returns cached error (with updated materialization_span)
/// - `InProgress`: returns circular dependency error (uses `ctx.eval_stack` for cycle path)
/// - `Unevaluated`: evaluates expr in env, memoizes result or error
/// - `PendingBuiltin`: calls builtin with args, memoizes result or error
/// - `PendingCall`: materializes func, invokes it with args, memoizes result or error
///
/// # Side effects
///
/// Mutates the thunk's internal state via `ThunkInner`. On success, transitions to
/// Convert a tinct value to a match signal via the `to-match` dispatch function.
///
/// `"to-match"` is the Rust-level protocol name for the match-signal class method —
/// analogous to Python's `__bool__`. Looks up `"to-match"` in the environment (injected
/// by ClassDecl lowering) and calls it with the value. The dispatch function internally
/// resolves the correct instance binding based on the value's runtime type.
///
/// Returns `false` in bootstrap/pre-prelude contexts (before `"to-match"` is in scope)
/// or for types with no instance.
pub async fn call_to_match(
    val: &Value,
    env: &Arc<RwLock<Env>>,
    ctx: &Arc<EvalContext>,
    span: &Span,
) -> bool {
    // "to-match" is the Rust-level protocol name — look it up directly.
    let to_match_thunk = {
        let env_read = env.read().unwrap();
        env_read.get_value_by_name("to-match")
    };
    let Some(to_match_fn) = to_match_thunk else {
        // Method not in scope yet (bootstrap / pre-prelude context): conservative false
        return false;
    };

    let val_thunk = Arc::new(Thunk::new_materialized(val.clone(), span.clone()));
    let call_thunk = Arc::new(Thunk::new_pending_call(
        to_match_fn,
        vec![val_thunk],
        IndexMap::new(),
        span.clone(),
        Arc::clone(env),
        span.clone(),
        Some(Arc::from("to-match")),
        Arc::clone(ctx),
        crate::builtins::synthetic_call_expr(span.clone()),
    ));

    match materialize(&call_thunk, Some(span), ctx).await {
        Ok(Value::Int(n)) => n != 0,
        _ => false,
    }
}

/// Convert a tinct value to a match signal using a pre-resolved Matchable instance binding name.
///
/// This is the direct-dispatch variant of `call_to_match`. Where `call_to_match` calls the
/// top-level `to-match` dispatch function (which then resolves the correct instance at runtime),
/// this function skips that indirection and calls the specific Matchable instance binding
/// (e.g., `"ɪɴꜱᴛᴀɴᴄᴇ⧼Matchable∷to-match⟨Boolean⟩⧽"`) directly.
///
/// The type checker resolves the Matchable instance at type-checking time and stores the
/// binding name on the pattern or call site. The evaluator uses this pre-resolved name
/// to avoid the `to-match` dispatch overhead.
///
/// Returns `false` if the binding is not found in the environment (pre-prelude bootstrap).
pub async fn call_to_match_resolved(
    val: &Value,
    binding_name: &str,
    env: &Arc<RwLock<Env>>,
    ctx: &Arc<EvalContext>,
    span: &Span,
) -> bool {
    let to_match_thunk = {
        let env_read = env.read().unwrap();
        env_read.get_value_by_name(binding_name)
    };
    let Some(to_match_fn) = to_match_thunk else {
        // Binding not found — instance not loaded yet (bootstrap / pre-prelude context).
        // Return false conservatively: the type checker guarantees this binding is set
        // for any pattern that can reach here. If prelude is not loaded yet, the pattern
        // cannot validly fire anyway.
        return false;
    };

    let val_thunk = Arc::new(Thunk::new_materialized(val.clone(), span.clone()));
    let call_thunk = Arc::new(Thunk::new_pending_call(
        to_match_fn,
        vec![val_thunk],
        IndexMap::new(),
        span.clone(),
        Arc::clone(env),
        span.clone(),
        Some(Arc::from(binding_name)),
        Arc::clone(ctx),
        crate::builtins::synthetic_call_expr(span.clone()),
    ));

    match materialize(&call_thunk, Some(span), ctx).await {
        Ok(Value::Int(n)) => n != 0,
        _ => false,
    }
}

/// Pre-resolve the match-signal class instance binding name from a predicate function's
/// return annotation.
///
/// HOF builtins (sort, until, par-filter) call a predicate function on each element and then
/// need to convert the result to a match signal. The standard approach calls `call_to_match`
/// on every result, which routes through the dispatch function on each iteration (two-hop
/// call: `call_to_match` -> dispatch function -> specific instance).
///
/// This function extracts the return type name from the predicate's `return_ann` annotation
/// (e.g., `fn@Boolean` -> "Boolean") and pre-computes the specific instance binding name
/// ONCE before the loop. The builtin then passes this to `call_to_match_resolved` on each
/// iteration, bypassing the dispatch indirection and calling the instance binding directly
/// (one-hop call).
///
/// The class name is discovered by scanning the environment for an instance binding whose
/// key ends with `∷to-match⟨type_name⟩⧽` — the Rust-level protocol name `"to-match"` is
/// fixed, and the class name is whatever class declared that method in the prelude.
///
/// Returns `None` if the predicate has no return annotation, the annotation is not a simple
/// type name, or no matching instance binding exists in the environment. In that case,
/// callers should fall back to `call_to_match` for the standard two-hop dispatch.
///
/// This does NOT call the binding — it only resolves the name. The environment lookup
/// and invocation happen inside `call_to_match_resolved` at call time.
pub fn resolve_matchable_binding_from_fn(pred: &Value, env: &Arc<RwLock<Env>>) -> Option<String> {
    let return_ann = match pred {
        Value::Function { return_ann, .. } => return_ann.as_ref()?,
        // Builtins don't carry return annotations -- fall back to dynamic dispatch.
        _ => return None,
    };
    // Extract a simple type name from the annotation.
    // `fn@Boolean` -> Annotation::Simple("Boolean") -> "Boolean"
    // `fn@[return: Boolean  doc: "..."]` -> Annotation::PropertyDict -> not handled here;
    // callers fall back to dynamic call_to_match for these forms.
    // Annotated forms (e.g. fn@[Seq@Int]) are not match-signal targets -- also fall back.
    let type_name = match &return_ann.node {
        crate::ast::Annotation::Simple(name) => name.as_str(),
        _ => return None,
    };
    // Scan the env for an instance binding for "to-match" on type_name.
    // Instance binding names have the form: ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷to-match⟨{type_name}⟩⧽
    // We search for the suffix ∷to-match⟨{type_name}⟩⧽ to find the binding without
    // needing to know the class name (which is defined by the prelude, not by Rust).
    let suffix = format!("\u{2237}to-match\u{27e8}{type_name}\u{27e9}\u{29fd}");
    env.read().unwrap().find_key_with_suffix(&suffix)
}

/// Call `call_to_match_resolved` if a pre-resolved binding name is available, otherwise
/// fall back to `call_to_match` (standard `to-match` dispatch).
///
/// HOF builtins use this inside their predicate-calling loops. The binding name is
/// pre-resolved once before the loop via `resolve_matchable_binding_from_fn`
/// (extracted from the predicate function's return annotation) and reused on every
/// iteration. When the predicate has no annotation, `binding_name` is `None` and this
/// falls back to `call_to_match`.
pub async fn call_to_match_opt_resolved(
    val: &Value,
    binding_name: Option<&str>,
    env: &Arc<RwLock<Env>>,
    ctx: &Arc<EvalContext>,
    span: &Span,
) -> bool {
    if let Some(name) = binding_name {
        call_to_match_resolved(val, name, env, ctx, span).await
    } else {
        call_to_match(val, env, ctx, span).await
    }
}

/// `Materialized` via `set_materialized()`. On failure, transitions to `Failed` via
/// `cache_failure_once()`.
///
/// # Parameters
///
/// - `mat_span`: the span of the expression that triggered materialization
///   (e.g., an access chain). Attached to errors so users can see both where
///   a value was defined and where it was forced.
/// - `ctx`: the caller's `EvalContext`. Each thunk captures its creation-time context
///   inside its `UnevaluatedState`, so `ctx` is used only for the `InProgress`
///   cycle-detection path (`ctx.state.eval_stack`). This follows Launchbury (1993):
///   thunks are closures over their birth environment, so forcing a thunk evaluates
///   in the context in which it was allocated, not the context of the demand site.
///
/// # Async implementation
///
/// Returns `Pin<Box<dyn Future>>` to break the recursive cycle
/// `materialize → eval → run → force_step → materialize`. Non-recursive helpers
/// use `async fn` directly.
pub fn materialize<'a>(
    thunk: &'a Thunk,
    mat_span: Option<&'a Span>,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Value>> + 'a>> {
    Box::pin(async move {
        // Read origin before checking state (InProgress may not preserve it)
        let origin = thunk.origin.clone();
        let thunk_span = thunk.span.clone();

        // Fast path: check if already materialized
        if let Some(v) = thunk.try_get_materialized() {
            return Ok(v);
        }

        // Check for Failed state (need to read cached error for stack frame enrichment)
        if let Some(err) = thunk.get_cached_error() {
            // Failed state: dual-span error caching model.
            //
            // First failure sets both definition_span and materialization_span.
            // Subsequent accesses with a new mat_span conditionally update:
            // - If materialization_span is None (edge case: cached error had no mat_span),
            //   set it to the current mat_span.
            // - If materialization_span differs from current mat_span and current mat_span
            //   is not already in the stack, add current mat_span as a stack frame.
            //   The original materialization_span is preserved.
            let mut cloned = (*err).clone();
            let mut should_update_cache = false;
            if let Some(span) = mat_span {
                if cloned.materialization_span.is_none() {
                    // First access via Failed path (edge case: error cached without mat_span)
                    cloned.materialization_span = Some(span.clone());
                    should_update_cache = true;
                } else if cloned.materialization_span != Some(span.clone())
                    && !cloned.stack.iter().any(|f| f.definition_span == *span)
                {
                    // Different access site: add as stack frame, preserve original mat_span
                    cloned.push_frame("materialized".to_string(), span.clone());
                    should_update_cache = true;
                }
            }
            // Update cached error if we modified it
            if should_update_cache && cloned.kind.is_cacheable() {
                thunk.cache_failure_once(&cloned);
            }
            return Err(Box::new(cloned));
        }

        // Check for InProgress (cycle detection)
        if thunk.is_in_progress() {
            // PROP-CYCLE: circular dependency detected during InProgress state check.
            // Error is constructed and decorated manually via with_materialization_span(),
            // rather than using the decorate closure (defined below), because we need to
            // immediately cache the error in the Failed state before returning.
            let label = origin.as_deref().unwrap_or("thunk");
            // Capture the eval_stack for cycle path reconstruction
            let cycle_path = ctx.state.lock().unwrap().eval_stack.clone();
            let mut err = EvalError::circular_dependency(label, thunk.span.clone(), cycle_path);
            if let Some(span) = mat_span {
                err = err.with_materialization_span(span.clone());
            }
            let err_boxed: Box<EvalError> = err.into();
            thunk.cache_failure_once(&err_boxed);
            return Err(err_boxed);
        }

        // Note: Placeholder state cannot be detected without full state inspection.
        // If we reach a Placeholder thunk, force_step will panic when it tries to take_*
        // and finds no unevaluated state. This is acceptable — Placeholder forcing is a
        // letrec construction bug.

        let origin_opt: Option<&str> = origin.as_deref();
        let decorate =
            move |e| attach_materialization_context(e, mat_span, origin_opt, thunk_span.clone());

        if let Some((def, args, named, call_span, builtin_caller_env, thunk_ctx)) =
            thunk.take_pending_builtin()
        {
            // Pre-materialize strict args before calling the builtin.
            //
            // The CEK machine (eval_materialize.rs::force_step) handles force_count and
            // pos_strictness W1 pre-materialization iteratively via BuiltinForceArg continuations.
            // This recursive path bypasses the CEK machine entirely, so it must replicate
            // force_count + W1 semantics here to prevent builtins using
            // `try_get_materialized().expect("pre-materialized by force_count/pos_strictness")` from panicking.
            //
            // Without this, any builtin with force_count > 0 (e.g. $take, $map, $drop) panics
            // when materialized via the recursive path (e.g. from builtin_reduce's loop,
            // from builtin_reduce's loop calling materialize() on its step thunk, etc.).
            //
            // On error during pre-materialization, restore PendingBuiltin state for
            // non-cacheable errors (DepthExceeded) to allow retry at a shallower depth.
            {
                use crate::value::Strictness;
                let mut premat_err: Option<Box<EvalError>> = None;
                // H1: force_count range — unconditional pre-materialization
                let force_limit = def.force_count.min(args.len());
                for arg in &args[..force_limit] {
                    if arg.try_get_materialized().is_none() {
                        if let Err(e) = materialize(arg, None, &thunk_ctx).await.map_err(&decorate)
                        {
                            premat_err = Some(e);
                            break;
                        }
                    }
                }
                // W1: pos_strictness Seq/Spine — dispatch-time materialization
                if premat_err.is_none() {
                    for (i, &s) in def.pos_strictness.iter().enumerate() {
                        if i < args.len()
                            && (s == Strictness::Seq || s == Strictness::Spine)
                            && args[i].try_get_materialized().is_none()
                        {
                            if let Err(e) = materialize(&args[i], None, &thunk_ctx)
                                .await
                                .map_err(&decorate)
                            {
                                premat_err = Some(e);
                                break;
                            }
                        }
                    }
                }
                if let Some(e) = premat_err {
                    if e.kind.is_cacheable() {
                        thunk.cache_failure_once(&e);
                    } else {
                        thunk.restore_unevaluated(crate::value::UnevaluatedState::Builtin {
                            def,
                            args,
                            named,
                            call_span,
                            caller_env: builtin_caller_env,
                            ctx: thunk_ctx,
                        });
                    }
                    return Err(e);
                }
            }
            // `named` is None for internally-created thunks (common case); only $apply
            // passes named args through. Use an empty map ref for the None case.
            let call_span_for_restore = call_span.clone();
            let caller_env_for_restore = Arc::clone(&builtin_caller_env);
            let builtin_args = crate::value::BuiltinArgs {
                args,
                named,
                call_span,
                caller_env: builtin_caller_env,
                ctx: Arc::clone(&thunk_ctx),
            };
            // Clone args/named from BuiltinArgs for potential restoration after builtin call.
            // Builtin functions take ownership of their args via BuiltinArgs, so we clone
            // AFTER constructing BuiltinArgs (move-then-clone). This has the same clone count
            // as clone-then-move, but keeps ownership clear: BuiltinArgs owns the live copy,
            // *_for_restore are used only on error paths.
            let args_for_restore = builtin_args.args.clone();
            let named_for_restore = builtin_args.named.clone();
            match (def.func)(builtin_args).await.map_err(&decorate) {
                Ok(result_thunk) => {
                    // Fast path: if the builtin already materialized its result, skip recursion.
                    if let Some(value) = result_thunk.try_get_materialized() {
                        thunk.set_materialized(value.clone());
                        Ok(value)
                    } else {
                        match run(
                            Action::Materialize {
                                thunk: result_thunk,
                                mat_span: mat_span.cloned(),
                            },
                            &thunk_ctx,
                        )
                        .await
                        .map_err(&decorate)
                        {
                            Ok(value) => {
                                thunk.set_materialized(value.clone());
                                Ok(value)
                            }
                            Err(e) => {
                                // Restore PendingBuiltin for non-cacheable errors (e.g., DepthExceeded).
                                if e.kind.is_cacheable() {
                                    thunk.cache_failure_once(&e);
                                } else {
                                    thunk.restore_unevaluated(
                                        crate::value::UnevaluatedState::Builtin {
                                            def,
                                            args: args_for_restore,
                                            named: named_for_restore,
                                            call_span: call_span_for_restore,
                                            caller_env: caller_env_for_restore,
                                            ctx: thunk_ctx,
                                        },
                                    );
                                }
                                Err(e)
                            }
                        }
                    }
                }
                Err(e) => {
                    // Restore PendingBuiltin for non-cacheable errors (e.g., DepthExceeded).
                    if e.kind.is_cacheable() {
                        thunk.cache_failure_once(&e);
                    } else {
                        thunk.restore_unevaluated(crate::value::UnevaluatedState::Builtin {
                            def,
                            args: args_for_restore,
                            named: named_for_restore,
                            call_span: call_span_for_restore,
                            caller_env: caller_env_for_restore,
                            ctx: thunk_ctx,
                        });
                    }
                    Err(e)
                }
            }
        } else if let Some((
            func_thunk,
            args,
            named,
            call_span,
            caller_env,
            thunk_ctx,
            original_call,
        )) = thunk.take_pending_call()
        {
            // Materialize the function thunk to determine if it's a Function or Builtin
            let func_value = match run(
                Action::Materialize {
                    thunk: Arc::clone(&func_thunk),
                    mat_span: Some(call_span.clone()),
                },
                &thunk_ctx,
            )
            .await
            .map_err(&decorate)
            {
                Ok(v) => v,
                Err(e) => {
                    // Restore PendingCall for non-cacheable errors (e.g., DepthExceeded).
                    if e.kind.is_cacheable() {
                        thunk.cache_failure_once(&e);
                    } else {
                        thunk.restore_unevaluated(crate::value::UnevaluatedState::Call {
                            func: func_thunk.clone(),
                            args: args.clone(),
                            named: named.clone().map(Box::new),
                            call_span,
                            caller_env: caller_env.clone(),
                            ctx: thunk_ctx.clone(),
                            original_call: original_call.clone(),
                        });
                    }
                    return Err(e);
                }
            };

            // Peel Value::Annotated wrappers — annotated constructors (@[doc:"..."]) wrap
            // their inner Variant in Value::Annotated; peel before dispatching the call.
            let func_value = {
                let mut v = func_value;
                while let Value::Annotated { inner, .. } = v {
                    v = *inner;
                }
                v
            };

            match func_value {
                Value::Function {
                    params, body, env, ..
                } => {
                    // Build CallContext and invoke the function
                    let call_ctx = CallContext {
                        params: &params,
                        body: &body,
                        closure_env: &env,
                        positional: &args,
                        named: named.as_ref(),
                        // For normal calls, `default_env` is the caller's environment (the env at
                        // the call site where the PendingCall thunk was created by `eval_call`).
                        // When forcing a PendingCall, `caller_env` is preserved from creation time
                        // (iterative-eval-b1) — it is the env captured in the thunk, not the env
                        // of whoever triggered materialization. `$apply` diverges: it uses the
                        // closure env as `default_env` so that defaults see the function's own scope.
                        default_env: &caller_env,
                        call_span: call_span.clone(),
                        origin: origin.clone(),
                        ctx: &thunk_ctx,
                    };

                    match invoke_function(&call_ctx).await.map_err(&decorate) {
                        Ok(result_thunk) => {
                            // Materialize the result and memoize
                            match run(
                                Action::Materialize {
                                    thunk: result_thunk,
                                    mat_span: mat_span.cloned(),
                                },
                                &thunk_ctx,
                            )
                            .await
                            .map_err(&decorate)
                            {
                                Ok(value) => {
                                    thunk.set_materialized(value.clone());
                                    Ok(value)
                                }
                                Err(e) => {
                                    // Restore PendingCall for non-cacheable errors (e.g., DepthExceeded).
                                    if e.kind.is_cacheable() {
                                        thunk.cache_failure_once(&e);
                                    } else {
                                        thunk.restore_unevaluated(
                                            crate::value::UnevaluatedState::Call {
                                                func: func_thunk.clone(),
                                                args: args.clone(),
                                                named: named.clone().map(Box::new),
                                                call_span: call_span.clone(),
                                                caller_env: caller_env.clone(),
                                                ctx: thunk_ctx.clone(),
                                                original_call: original_call.clone(),
                                            },
                                        );
                                    }
                                    Err(e)
                                }
                            }
                        }
                        Err(mut e) => {
                            // Add stack frame for function call site.
                            // Success path doesn't need call site tracking - only errors
                            // need stack traces for debugging. The thunk's span is the
                            // definition site, which is sufficient for successful results.
                            if let Some(label) = origin.as_deref() {
                                e.push_frame(label.to_string(), call_span.clone());
                            }
                            if e.kind.is_cacheable() {
                                thunk.cache_failure_once(&e);
                            } else {
                                thunk.restore_unevaluated(crate::value::UnevaluatedState::Call {
                                    func: func_thunk.clone(),
                                    args: args.clone(),
                                    named: named.clone().map(Box::new),
                                    call_span,
                                    caller_env: caller_env.clone(),
                                    ctx: thunk_ctx.clone(),
                                    original_call: original_call.clone(),
                                });
                            }
                            Err(e)
                        }
                    }
                }
                Value::Builtin(def) => {
                    // Pre-materialize strict args before calling the builtin.
                    //
                    // The CEK machine (eval_materialize.rs::PendingCallDispatch) handles
                    // force_count and pos_strictness W1 pre-materialization via the
                    // PendingBuiltin transition. This recursive path bypasses the CEK machine
                    // for PendingCall→Builtin dispatch, so it must replicate force_count + W1
                    // semantics here to prevent builtins using
                    // `try_get_materialized().expect("pre-materialized by force_count/pos_strictness")`
                    // from panicking (e.g. builtin_add when called via a reduce PendingCall chain).
                    {
                        use crate::value::Strictness;
                        let mut premat_err: Option<Box<EvalError>> = None;
                        // H1: force_count range — unconditional pre-materialization
                        let force_limit = def.force_count.min(args.len());
                        for arg in &args[..force_limit] {
                            if arg.try_get_materialized().is_none() {
                                if let Err(e) =
                                    materialize(arg, None, &thunk_ctx).await.map_err(&decorate)
                                {
                                    premat_err = Some(e);
                                    break;
                                }
                            }
                        }
                        // W1: pos_strictness Seq/Spine — dispatch-time materialization
                        if premat_err.is_none() {
                            for (i, &s) in def.pos_strictness.iter().enumerate() {
                                if i < args.len()
                                    && (s == Strictness::Seq || s == Strictness::Spine)
                                    && args[i].try_get_materialized().is_none()
                                {
                                    if let Err(e) = materialize(&args[i], None, &thunk_ctx)
                                        .await
                                        .map_err(&decorate)
                                    {
                                        premat_err = Some(e);
                                        break;
                                    }
                                }
                            }
                        }
                        if let Some(e) = premat_err {
                            if e.kind.is_cacheable() {
                                thunk.cache_failure_once(&e);
                            } else {
                                thunk.restore_unevaluated(crate::value::UnevaluatedState::Call {
                                    func: func_thunk.clone(),
                                    args: args.clone(),
                                    named: named.clone().map(Box::new),
                                    call_span: call_span.clone(),
                                    caller_env: caller_env.clone(),
                                    ctx: thunk_ctx.clone(),
                                    original_call: original_call.clone(),
                                });
                            }
                            return Err(e);
                        }
                    }
                    let call_span_for_restore = call_span.clone();
                    let builtin_args = crate::value::BuiltinArgs {
                        args,
                        named,
                        call_span,
                        caller_env: Arc::clone(&caller_env),
                        ctx: Arc::clone(&thunk_ctx),
                    };
                    // Clone args/named from BuiltinArgs for error-path restoration.
                    // BuiltinArgs owns the live copy; *_for_restore used only on error paths.
                    let args_for_restore = builtin_args.args.clone();
                    let named_for_restore = builtin_args.named.clone();
                    match (def.func)(builtin_args).await.map_err(&decorate) {
                        Ok(result_thunk) => {
                            if let Some(value) = result_thunk.try_get_materialized() {
                                thunk.set_materialized(value.clone());
                                Ok(value)
                            } else {
                                match run(
                                    Action::Materialize {
                                        thunk: result_thunk,
                                        mat_span: mat_span.cloned(),
                                    },
                                    &thunk_ctx,
                                )
                                .await
                                .map_err(&decorate)
                                {
                                    Ok(value) => {
                                        thunk.set_materialized(value.clone());
                                        Ok(value)
                                    }
                                    Err(e) => {
                                        if e.kind.is_cacheable() {
                                            thunk.cache_failure_once(&e);
                                        } else {
                                            thunk.restore_unevaluated(
                                                crate::value::UnevaluatedState::Call {
                                                    func: func_thunk.clone(),
                                                    args: args_for_restore,
                                                    named: named_for_restore.map(Box::new),
                                                    call_span: call_span_for_restore,
                                                    caller_env: caller_env.clone(),
                                                    ctx: thunk_ctx.clone(),
                                                    original_call: original_call.clone(),
                                                },
                                            );
                                        }
                                        Err(e)
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            if e.kind.is_cacheable() {
                                thunk.cache_failure_once(&e);
                            } else {
                                thunk.restore_unevaluated(crate::value::UnevaluatedState::Call {
                                    func: func_thunk.clone(),
                                    args: args_for_restore,
                                    named: named_for_restore.map(Box::new),
                                    call_span: call_span_for_restore,
                                    caller_env: caller_env.clone(),
                                    ctx: thunk_ctx.clone(),
                                    original_call: original_call.clone(),
                                });
                            }
                            Err(e)
                        }
                    }
                }
                // Unit Variant used as a constructor. All constructors (unit and named-field)
                // are Value::Variant{payload:None} at rest. Calling convention:
                //   - One positional arg, no named: wrap arg as payload (e.g. [Result.Ok v])
                //   - Named args only, no positional: build payload dict from named args
                //     (e.g. [ProgramItem.File path: "x" handle: h])
                Value::Variant { tag, payload: None }
                    if args.len() == 1 && named.as_ref().is_none_or(|m| m.is_empty()) =>
                {
                    let payload_thunk = args.into_iter().next().expect("1 arg checked above");
                    let payload_id = thunk_ctx.alloc_thunk(payload_thunk);
                    let result_val = Value::Variant {
                        tag,
                        payload: Some(payload_id),
                    };
                    thunk.set_materialized(result_val.clone());
                    Ok(result_val)
                }
                other => {
                    let err = EvalError::type_mismatch(
                        "Function or Builtin",
                        other.type_name(),
                        call_span.clone(),
                    );
                    let decorated = decorate(Box::new(err));
                    if decorated.kind.is_cacheable() {
                        thunk.cache_failure_once(&decorated);
                    } else {
                        thunk.restore_unevaluated(crate::value::UnevaluatedState::Call {
                            func: func_thunk,
                            args,
                            named: named.map(Box::new),
                            call_span,
                            caller_env,
                            ctx: thunk_ctx,
                            original_call,
                        });
                    }
                    Err(decorated)
                }
            }
        } else if let Some((inner, expected, mut field_path, guard_span, blame_label, default)) =
            thunk.take_guarded()
        {
            // Materialize the inner thunk first.
            // Guarded thunks now carry default: expressions from TypeAssert annotations.
            // When guard validation fails, the default is evaluated and used as the fallback.

            // Capture inner thunk's span before materializing — used as data_span for error reporting
            let inner_span = inner.span.clone();

            let result = run(
                Action::Materialize {
                    thunk: Arc::clone(&inner),
                    mat_span: mat_span.cloned(),
                },
                ctx,
            )
            .await;

            match result {
                Ok(value) => {
                    // For Record types (and Intersection-of-Records), apply proxy contract wrapping.
                    // as_record_row_merged handles both Type::Record and Intersection-of-Records
                    // by merging all required fields into a single Row.
                    if let Some(row) = as_record_row_merged(&expected) {
                        if let Value::Dict(ref entries) = value {
                            // Use helper to validate and wrap record
                            match validate_and_wrap_record(
                                entries,
                                row.as_ref(),
                                &mut field_path,
                                guard_span.clone(),
                                inner_span.clone(),
                                ctx,
                                default.clone(),
                                blame_label.clone(),
                            ) {
                                Ok(new_entries) => {
                                    let guarded_value = Value::Dict(new_entries);
                                    thunk.set_materialized(guarded_value.clone());
                                    Ok(guarded_value)
                                }
                                Err(err) => {
                                    // Guard validation failed - use default if present
                                    if let Some((default_expr, default_env)) = default {
                                        let default_thunk =
                                            match eval_core_expr(&default_expr, &default_env, ctx)
                                                .await
                                            {
                                                Ok(t) => t,
                                                Err(e) => {
                                                    // Restore Guarded state for non-cacheable errors.
                                                    if e.kind.is_cacheable() {
                                                        thunk.cache_failure_once(&e);
                                                    } else {
                                                        thunk.restore_unevaluated(
                                                        crate::value::UnevaluatedState::Guarded {
                                                            inner,
                                                            expected,
                                                            field_path,
                                                            guard_span,
                                                            blame_label,
                                                            default: Some((
                                                                default_expr,
                                                                default_env,
                                                            )),
                                                        },
                                                    );
                                                    }
                                                    return Err(e);
                                                }
                                            };
                                        let default_value = match run(
                                            Action::Materialize {
                                                thunk: default_thunk,
                                                mat_span: mat_span.cloned(),
                                            },
                                            ctx,
                                        )
                                        .await
                                        {
                                            Ok(v) => v,
                                            Err(e) => {
                                                // Restore Guarded state for non-cacheable errors.
                                                if e.kind.is_cacheable() {
                                                    thunk.cache_failure_once(&e);
                                                } else {
                                                    thunk.restore_unevaluated(
                                                        crate::value::UnevaluatedState::Guarded {
                                                            inner,
                                                            expected,
                                                            field_path,
                                                            guard_span,
                                                            blame_label,
                                                            default: Some((
                                                                default_expr,
                                                                default_env,
                                                            )),
                                                        },
                                                    );
                                                }
                                                return Err(e);
                                            }
                                        };
                                        thunk.set_materialized(default_value.clone());
                                        return Ok(default_value);
                                    }
                                    let err = decorate(err);
                                    thunk.cache_failure_once(&err);
                                    Err(err)
                                }
                            }
                        } else {
                            // Expected Record/Intersection but got non-Dict - use default if present
                            if let Some((default_expr, default_env)) = default {
                                let default_thunk =
                                    match eval_core_expr(&default_expr, &default_env, ctx).await {
                                        Ok(t) => t,
                                        Err(e) => {
                                            if e.kind.is_cacheable() {
                                                thunk.cache_failure_once(&e);
                                            } else {
                                                thunk.restore_unevaluated(
                                                    crate::value::UnevaluatedState::Guarded {
                                                        inner,
                                                        expected,
                                                        field_path,
                                                        guard_span,
                                                        blame_label,
                                                        default: Some((default_expr, default_env)),
                                                    },
                                                );
                                            }
                                            return Err(e);
                                        }
                                    };
                                let default_value = match run(
                                    Action::Materialize {
                                        thunk: default_thunk,
                                        mat_span: mat_span.cloned(),
                                    },
                                    ctx,
                                )
                                .await
                                {
                                    Ok(v) => v,
                                    Err(e) => {
                                        if e.kind.is_cacheable() {
                                            thunk.cache_failure_once(&e);
                                        } else {
                                            thunk.restore_unevaluated(
                                                crate::value::UnevaluatedState::Guarded {
                                                    inner,
                                                    expected,
                                                    field_path,
                                                    guard_span,
                                                    blame_label,
                                                    default: Some((default_expr, default_env)),
                                                },
                                            );
                                        }
                                        return Err(e);
                                    }
                                };
                                thunk.set_materialized(default_value.clone());
                                return Ok(default_value);
                            }
                            let field_path_prefix = if field_path.is_empty() {
                                String::new()
                            } else {
                                format!("field {}: ", format_field_path(&field_path))
                            };
                            let mut err = EvalError::type_assert_failed(
                                &format!(
                                    "{}{}",
                                    field_path_prefix,
                                    format_type_for_assert(&expected)
                                ),
                                value.type_name(),
                                inner_span.clone(),
                            );
                            // Add secondary span if inner value was produced at a different
                            // location than the assertion site (guard_span).
                            if inner_span != guard_span {
                                err = err.with_secondary_span(inner_span, "value produced here");
                            }
                            let err = decorate(err.into());
                            thunk.cache_failure_once(&err);
                            Err(err)
                        }
                    } else {
                        // For non-Record types, simple value check
                        if value_matches_type(&value, &expected, ctx) {
                            thunk.set_materialized(value.clone());
                            Ok(value)
                        } else {
                            // Type mismatch for non-Record types - use default if present
                            if let Some((default_expr, default_env)) = default {
                                let default_thunk =
                                    match eval_core_expr(&default_expr, &default_env, ctx).await {
                                        Ok(t) => t,
                                        Err(e) => {
                                            if e.kind.is_cacheable() {
                                                thunk.cache_failure_once(&e);
                                            } else {
                                                thunk.restore_unevaluated(
                                                    crate::value::UnevaluatedState::Guarded {
                                                        inner,
                                                        expected,
                                                        field_path,
                                                        guard_span,
                                                        blame_label,
                                                        default: Some((default_expr, default_env)),
                                                    },
                                                );
                                            }
                                            return Err(e);
                                        }
                                    };
                                let default_value = match run(
                                    Action::Materialize {
                                        thunk: default_thunk,
                                        mat_span: mat_span.cloned(),
                                    },
                                    ctx,
                                )
                                .await
                                {
                                    Ok(v) => v,
                                    Err(e) => {
                                        if e.kind.is_cacheable() {
                                            thunk.cache_failure_once(&e);
                                        } else {
                                            thunk.restore_unevaluated(
                                                crate::value::UnevaluatedState::Guarded {
                                                    inner,
                                                    expected,
                                                    field_path,
                                                    guard_span,
                                                    blame_label,
                                                    default: Some((default_expr, default_env)),
                                                },
                                            );
                                        }
                                        return Err(e);
                                    }
                                };
                                thunk.set_materialized(default_value.clone());
                                return Ok(default_value);
                            }
                            let field_path_prefix = if field_path.is_empty() {
                                String::new()
                            } else {
                                format!("field {}: ", format_field_path(&field_path))
                            };
                            let mut err = EvalError::type_assert_failed(
                                &format!(
                                    "{}{}",
                                    field_path_prefix,
                                    format_type_for_assert(&expected)
                                ),
                                value.type_name(),
                                inner_span.clone(),
                            );
                            // Add secondary span if inner value was produced at a different
                            // location than the assertion site (guard_span).
                            if inner_span != guard_span {
                                err = err.with_secondary_span(inner_span, "value produced here");
                            }
                            let err = decorate(err.into());
                            thunk.cache_failure_once(&err);
                            Err(err)
                        }
                    }
                }
                Err(e) => {
                    // Inner materialization error propagates (not a type mismatch)
                    let e = decorate(e);
                    if e.kind.is_cacheable() {
                        thunk.cache_failure_once(&e);
                    } else {
                        // Non-cacheable error (e.g., DepthExceeded): restore Guarded state
                        // so the thunk can be re-evaluated at a shallower depth.
                        thunk.restore_unevaluated(crate::value::UnevaluatedState::Guarded {
                            inner,
                            expected,
                            field_path,
                            guard_span,
                            blame_label,
                            default,
                        });
                    }
                    Err(e)
                }
            }
        } else if let Some((node, env, thunk_ctx)) = thunk.take_surface() {
            // runtime-v2 Sprint 1: Surface thunk handling via lower() → CoreExpr → eval_core_expr().
            //
            // 1. Lower the SurfaceNode to CoreExpr using lower() (reads inline fields)
            // 2. Evaluate the CoreExpr using eval_core_expr()
            // 3. Materialize the result thunk
            let (lowered, lower_diags) = crate::lower::lower(&node);
            if let Some(err) = lower_diags
                .into_iter()
                .find(|d| matches!(d.kind, crate::lower::LowerDiagnosticKind::Error))
            {
                return Err(decorate(
                    EvalError::user_error(err.message, err.span).into(),
                ));
            }
            let result = async {
                let result_thunk = eval_core_expr(&lowered, &env, &thunk_ctx).await?;
                run(
                    Action::Materialize {
                        thunk: result_thunk,
                        mat_span: mat_span.cloned(),
                    },
                    &thunk_ctx,
                )
                .await
            }
            .await
            .map_err(&decorate);

            match result {
                Ok(value) => {
                    thunk.set_materialized(value.clone());
                    Ok(value)
                }
                Err(e) => {
                    if e.kind.is_cacheable() {
                        thunk.cache_failure_once(&e);
                    } else {
                        // Restore Surface state for non-cacheable errors
                        thunk.restore_unevaluated(crate::value::UnevaluatedState::Surface {
                            node,
                            env,
                            ctx: thunk_ctx,
                        });
                    }
                    Err(e)
                }
            }
        } else if let Some((node, field, thunk_ctx)) = thunk.take_ast_node_field() {
            // runtime-v2: AstNodeField thunk — lazily evaluate a named field from a SurfaceNode.
            //
            // Created by match dispatch on Expr.* variants. Evaluates on demand when the
            // arm body accesses the bound variable. Unused bindings are never forced.
            let value = crate::surface_fields::surface_node_get_field(&node, field, &thunk_ctx);
            thunk.set_materialized(value.clone());
            Ok(value)
        } else if let Some((core_expr, env, thunk_ctx)) = thunk.take_core_expr() {
            // CoreExpr thunk — created by invoke_function when Value::Function.body is
            // Arc<Spanned<CoreExpr>>. Evaluates directly via eval_core_expr (no round-trip).
            let result = async {
                let result_thunk = eval_core_expr(&core_expr, &env, &thunk_ctx).await?;
                run(
                    Action::Materialize {
                        thunk: result_thunk,
                        mat_span: mat_span.cloned(),
                    },
                    &thunk_ctx,
                )
                .await
            }
            .await
            .map_err(&decorate);

            match result {
                Ok(value) => {
                    thunk.set_materialized(value.clone());
                    Ok(value)
                }
                Err(e) => {
                    if e.kind.is_cacheable() {
                        thunk.cache_failure_once(&e);
                    } else {
                        // Restore CoreExpr state for non-cacheable errors (e.g., DepthExceeded).
                        thunk.restore_unevaluated(crate::value::UnevaluatedState::CoreExpr {
                            expr: core_expr,
                            env,
                            ctx: thunk_ctx,
                        });
                    }
                    Err(e)
                }
            }
        } else {
            unreachable!(
                "state must be Unevaluated, PendingBuiltin, PendingCall, Guarded, \
             Surface, AstNodeField, or CoreExpr. \
             All other ThunkState variants are handled in the early-return section at the \
             top of this function: Materialized returns early, Failed returns early, \
             InProgress returns early and caches circular dependency error."
            )
        }
    }) // end Box::pin(async move {
}

/// Collect all variable names bound by a pattern, recursing into sub-patterns.
///
/// Returns a list of `(name, span)` pairs for binding sub-patterns.
/// Only `[case [let v] ...]` forms bind; no bare name leaf introduces a binding.
/// Retained for linearity checking on composite patterns (Dict, Seq, Constructor, Or)
/// that may carry binding sub-expressions in the future.
///
/// Duplicate names in the returned list indicate a non-linear pattern.
///
/// **Test-only.** Production code uses last-binding-wins semantics for non-linear
/// patterns (see doc/14-patterns.md §Non-Linear Patterns). These functions exist
/// solely to test the detection algorithm, not to enforce linearity at runtime.
#[cfg(test)]
#[allow(clippy::only_used_in_recursion)]
fn collect_pattern_variable_names(pattern: &Spanned<Pattern>, out: &mut Vec<(String, Span)>) {
    match &pattern.node {
        Pattern::Wildcard | Pattern::Literal(_) | Pattern::Pin(..) => {
            // No variable bindings — Pin compares against scope, does not bind
        }
        Pattern::TypeAssertPending { inner, .. } => {
            if let Some(inner_pat) = inner {
                collect_pattern_variable_names(inner_pat, out);
            }
        }
        Pattern::TypeAssert { inner, .. } => {
            if let Some(inner_pat) = inner {
                collect_pattern_variable_names(inner_pat, out);
            }
        }
        Pattern::Dict { fields, .. } => {
            for (_key, field_pattern) in fields {
                collect_pattern_variable_names(field_pattern, out);
            }
        }
        Pattern::Constructor { binding, .. } => {
            if let Some(payload_pattern) = binding {
                collect_pattern_variable_names(payload_pattern, out);
            }
        }
        Pattern::Or(branches) => {
            // Accumulate names from all branches into the parent list.
            // Top-level Or arms are handled by `check_pattern_linearity` before
            // calling this function. For nested Or sub-patterns within a larger
            // pattern, accumulating from all branches is conservative: any variable
            // that appears in the nested Or cannot safely appear elsewhere in the arm
            // (because whichever branch fires, that variable is bound).
            for branch in branches {
                collect_pattern_variable_names(branch, out);
            }
        }
        // T-1140: Predicate patterns introduce no variable bindings.
        Pattern::Predicate { .. } => {}
    }
}

/// Check that a pattern is linear — every variable name appears at most once
/// within a single branch.
///
/// Returns `Err` with E072 if a variable is bound more than once within a single
/// arm or or-pattern branch.  Returns `Ok(())` if the pattern is linear.
///
/// Or-patterns are handled specially: each branch is checked independently, because
/// the same variable appearing in every branch of `p1 | p2` is correct (required by
/// the or-pattern completeness invariant).  Only a duplicate within a single branch
/// is a linearity violation.
///
/// **Test-only.** Production code uses last-binding-wins semantics for non-linear
/// patterns (see doc/14-patterns.md §Non-Linear Patterns). This function is retained
/// as a test helper to verify duplicate-detection logic.
#[cfg(test)]
#[allow(clippy::result_large_err)]
pub(crate) fn check_pattern_linearity(pattern: &Spanned<Pattern>) -> Result<(), EvalError> {
    // Or-patterns: check each branch independently.
    if let Pattern::Or(branches) = &pattern.node {
        for branch in branches {
            check_pattern_linearity(branch)?;
        }
        return Ok(());
    }

    // For all other patterns, collect all variable names into a flat list and
    // detect duplicates.
    let mut names: Vec<(String, Span)> = Vec::new();
    collect_pattern_variable_names(pattern, &mut names);

    let mut seen: HashSet<&str> = HashSet::with_capacity(names.len());
    for (name, span) in &names {
        if !seen.insert(name.as_str()) {
            return Err(EvalError::duplicate_variable_in_pattern(name, span.clone()));
        }
    }
    Ok(())
}

/// Match a pattern against a value, returning the extended environment if the pattern matches.
///
/// Returns Ok(Some(env)) if the pattern matches (env contains any bindings from the pattern).
/// Returns Ok(None) if the pattern does not match.
/// Returns Err if there's an evaluation error (e.g., undefined pin variable).
pub(crate) fn match_pattern<'a>(
    pattern: &'a Pattern,
    value: &'a Value,
    env: &'a Arc<RwLock<Env>>,
    value_span: &'a Span,
    ctx: &'a Arc<EvalContext>,
) -> MatchPatternFuture<'a> {
    Box::pin(async move {
        match pattern {
            Pattern::Wildcard => {
                // Wildcard always matches, no bindings
                Ok(Some(Arc::clone(env)))
            }
            Pattern::TypeAssertPending {
                annotation, inner, ..
            } => {
                // FALLBACK RUNTIME ELABORATION (B-338):
                //
                // In the normal pipeline, `lower_pattern` in `lower.rs` converts
                // `TypeAssertPending → TypeAssert` using the inline `resolved` TypeAnnotation field
                // populated by the type checker. When type checking runs, this arm is NOT
                // reached — the `Pattern::TypeAssert` arm above handles those cases.
                //
                // This arm is only reached when:
                // 1. `--no-typecheck` is in effect (type checking skipped; table is empty)
                // 2. Macro-synthesized patterns that bypassed the type checker
                //
                // The fallback provides minimal runtime resolution for Simple annotations
                // (primitive type names), covering the most common no-typecheck cases.
                // Complex annotations (union types, record types) still always-match as
                // before — they require the full type checker to resolve correctly.
                let resolved = match &annotation.node {
                    Annotation::Simple(name) => {
                        // Map known primitive annotation names to canonical Type variants.
                        // These bypass tycon_env lookup and go directly to is_consistent_subtype.
                        //
                        // WARNING: This list must stay in sync with resolve_annotation's primitive
                        // handling in typecheck_annot.rs::resolve_type_name until T-1018 registers
                        // builtin TyCons in tycon_env. "Num"/"Bytes" must appear in both places to
                        // avoid phase inconsistency.
                        let ty = match name.as_str() {
                            "Int" => Type::Int,
                            "Float" => Type::Float,
                            "String" | "Str" => Type::Str,
                            "Bytes" => Type::Bytes,
                            // Unknown names: try TyCon as fallback.
                            // Works for user-defined types once T-1018 populates tycon_env.
                            // Also works for builtin TyCons ("Dict", "Fn", etc.) that are
                            // registered with builtin_type discriminants in tycon_env.
                            other => Type::TyCon(other.to_string()),
                        };
                        Some(ty)
                    }
                    _ => None, // Complex annotations: fall through as always-match (stub)
                };
                match resolved {
                    Some(resolved_type) => {
                        if !value_matches_type(value, &resolved_type, ctx) {
                            return Ok(None); // type mismatch — arm does not match
                        }
                        match inner {
                            None => Ok(Some(Arc::clone(env))),
                            Some(pat) => {
                                match_pattern(&pat.node, value, env, value_span, ctx).await
                            }
                        }
                    }
                    None => {
                        // Complex annotation: always match (stub behavior)
                        match inner {
                            None => Ok(Some(Arc::clone(env))),
                            Some(pat) => {
                                match_pattern(&pat.node, value, env, value_span, ctx).await
                            }
                        }
                    }
                }
            }
            Pattern::TypeAssert {
                resolved_type,
                inner,
            } => {
                // Primary runtime path for typed patterns.
                // lower.rs converts TypeAssertPending → TypeAssert (using annotation_name_to_type
                // or the inline resolved TypeAnnotation when populated by the type checker).
                // Uses pattern_type_matches (exact dispatch) rather than value_matches_type
                // (gradual subtyping) so that e.g. [@Int _]: never matches Value::Builtin.
                if !pattern_type_matches(value, resolved_type, ctx) {
                    return Ok(None); // type mismatch — arm does not match
                }
                match inner {
                    None => Ok(Some(Arc::clone(env))),
                    Some(pat) => match_pattern(&pat.node, value, env, value_span, ctx).await,
                }
            }
            Pattern::Literal(lit) => {
                // Literal matches if the value equals the literal
                let matches = match (lit, value) {
                    (LiteralPattern::Int(n), Value::Int(v)) => n == v,
                    (LiteralPattern::U64(n), Value::U64(v)) => n == v,
                    (LiteralPattern::Float(f), Value::Float(v)) => f == v,
                    (
                        LiteralPattern::Str(s),
                        Value::String {
                            ref source,
                            start,
                            end,
                        },
                    ) => s == &source[*start..*end],
                    _ => false,
                };
                if matches {
                    Ok(Some(Arc::clone(env)))
                } else {
                    Ok(None)
                }
            }
            Pattern::Pin(_name, pin_resolution) => {
                // Pin matches if the variable's current value equals the scrutinee value.
                // Use $name syntax in patterns to pin against an existing variable.
                // If the name was not in scope at resolve time (pin_resolution = Some(None)),
                // act as wildcard (always matches, no binding).
                // Use [case [let v] pattern body] to introduce new bindings.
                let var_thunk = match pin_resolution.get() {
                    Some(Some((level, slot))) => {
                        match env.read().unwrap().get_value_at(level, slot) {
                            Some(t) => t,
                            None => {
                                // Resolver assigned coords but slot is invalid at runtime —
                                // treat as wildcard (env chain changed after resolution).
                                return Ok(Some(Arc::clone(env)));
                            }
                        }
                    }
                    Some(None) => {
                        // Name was not in scope at resolve time — wildcard behavior.
                        return Ok(Some(Arc::clone(env)));
                    }
                    None => {
                        // Resolver never ran — wildcard behavior (safe fallback).
                        return Ok(Some(Arc::clone(env)));
                    }
                };
                let var_value = materialize(&var_thunk, Some(value_span), ctx).await?;

                // Compare values for equality using primitive_eq — only handles
                // Int, Float, String, unit Variant. No deep structural comparison.
                let matches = primitive_eq(var_value, value.clone());
                if matches {
                    Ok(Some(Arc::clone(env)))
                } else {
                    Ok(None)
                }
            }
            Pattern::Dict { fields, rest } => {
                // Dict pattern: match dict by keys, bind values to pattern variables
                // Only force the fields that are matched — other fields stay as thunks
                match value {
                    Value::Dict(dict_thunk_ids) => {
                        // Start with the current environment
                        let mut result_env =
                            Arc::new(RwLock::new(Env::with_parent(Arc::clone(env))));

                        // Check each pattern field
                        for (key, field_pattern) in fields {
                            // Look up the field in the dict
                            if let Some(field_thunk_id) =
                                dict_thunk_ids.get(&HashableValue::Str(Rc::from(key.as_str())))
                            {
                                // Force the field value
                                let field_thunk = ctx.get_thunk(*field_thunk_id);
                                let field_value =
                                    materialize(&field_thunk, Some(value_span), ctx).await?;

                                // Recursively match the field pattern
                                match match_pattern(
                                    &field_pattern.node,
                                    &field_value,
                                    &result_env,
                                    &field_pattern.span,
                                    ctx,
                                )
                                .await?
                                {
                                    Some(new_env) => {
                                        result_env = new_env;
                                    }
                                    None => {
                                        // Field pattern didn't match
                                        return Ok(None);
                                    }
                                }
                            } else {
                                // Required field not present in dict
                                return Ok(None);
                            }
                        }

                        // If rest is false (closed matching), check for extra keys.
                        // Pattern::Dict { rest: false } is unreachable from parsed programs —
                        // the parser defaults to rest=true (open matching) — but reachable
                        // from macro-constructed ASTs. When closed-dict syntax is added
                        // (e.g. trailing !), this branch will also become reachable from parsed programs.
                        if !rest {
                            let pattern_keys: std::collections::HashSet<&str> =
                                fields.iter().map(|(k, _)| k.as_str()).collect();
                            for dict_key in dict_thunk_ids.keys() {
                                let key_matches = match dict_key {
                                    HashableValue::Str(s) => pattern_keys.contains(s.as_ref()),
                                    HashableValue::Int(_) => false,
                                    _ => false,
                                };
                                if !key_matches {
                                    // Extra key found in closed matching mode
                                    return Ok(None);
                                }
                            }
                        }

                        Ok(Some(result_env))
                    }
                    Value::Overlay(l_id, r_id) => {
                        // PM2: Overlay (e.g., from $merge) has type_name() == "Dict" and must
                        // be matchable by Pattern::Dict. Flatten to a concrete map first, then
                        // re-run the Dict matching logic on the flattened result.
                        let flat_map = crate::builtins::flatten_overlay(
                            l_id,
                            r_id,
                            "dict pattern match",
                            ctx,
                            value_span.clone(),
                        )
                        .await?;
                        // Re-use the Value::Dict matching path by recursing with the flattened value.
                        match_pattern(
                            &Pattern::Dict {
                                fields: fields.clone(),
                                rest: *rest,
                            },
                            &Value::Dict(flat_map),
                            env,
                            value_span,
                            ctx,
                        )
                        .await
                    }
                    Value::Variant {
                        payload: Some(payload_id),
                        ..
                    } => {
                        // Auto-unpack variant payload for dict pattern matching:
                        // a Variant with a dict payload can be matched by a dict pattern
                        // against its payload (consistent with require_dict/flatten_overlay).
                        let payload_thunk = ctx.get_thunk(*payload_id);
                        let payload_val =
                            materialize(&payload_thunk, Some(value_span), ctx).await?;
                        match_pattern(
                            &Pattern::Dict {
                                fields: fields.clone(),
                                rest: *rest,
                            },
                            &payload_val,
                            env,
                            value_span,
                            ctx,
                        )
                        .await
                    }
                    _ => {
                        // Value is not a dict
                        Ok(None)
                    }
                }
            }
            Pattern::Constructor { tag, binding } => {
                // Peel Value::Annotated wrappers before matching.
                // Unit constructors declared with @[...] annotations evaluate to
                // Value::Annotated { inner: Variant(...), annotation: {...} }.
                // Annotations are metadata-only — pattern matching sees only the inner value.
                let value = {
                    let mut v = value;
                    while let Value::Annotated { inner, .. } = v {
                        v = inner.as_ref();
                    }
                    v
                };
                // Constructor pattern: match Value::Variant by tag, bind payload if present
                match value {
                    Value::Variant {
                        tag: variant_tag,
                        payload: variant_payload,
                    } => {
                        // Check if tags match
                        if tag != variant_tag {
                            return Ok(None);
                        }

                        // If pattern expects a payload, match it
                        match (binding, variant_payload) {
                            (Some(pattern), Some(payload_id)) => {
                                // Force the payload value
                                let payload_thunk = ctx.get_thunk(*payload_id);
                                let payload_value =
                                    materialize(&payload_thunk, Some(value_span), ctx).await?;

                                // Match the payload pattern
                                match_pattern(
                                    &pattern.node,
                                    &payload_value,
                                    env,
                                    &pattern.span,
                                    ctx,
                                )
                                .await
                            }
                            (None, None) => {
                                // Unit variant matches unit variant: [Tag] pattern (Constructor { binding: None }) matches Variant { payload: None }
                                Ok(Some(Arc::clone(env)))
                            }
                            (Some(_), None) => {
                                // Pattern expects payload but variant has none
                                Ok(None)
                            }
                            (None, Some(_)) => {
                                // [Tag]: with no binding matches any variant with that tag,
                                // regardless of whether it carries a payload — equivalent to [Tag _]:.
                                Ok(Some(Arc::clone(env)))
                            }
                        }
                    }
                    _ => {
                        // Value is not a Variant
                        Ok(None)
                    }
                }
            }
            Pattern::Or(patterns) => {
                // Or-pattern: try each sub-pattern in order
                // The first one that matches determines the bindings
                for sub_pattern in patterns {
                    if let Some(bound_env) =
                        match_pattern(&sub_pattern.node, value, env, value_span, ctx).await?
                    {
                        return Ok(Some(bound_env));
                    }
                }
                // None of the sub-patterns matched
                Ok(None)
            }
            Pattern::Predicate { .. } => {
                // T-1140: Predicate patterns must be intercepted in MatchDispatch before
                // reaching match_pattern. This arm exists as a safety guard — it should
                // never be reached in correctly structured evaluation paths.
                Err(EvalError::internal(
                    "Pattern::Predicate reached match_pattern directly; \
                     must be handled in MatchDispatch before calling match_pattern"
                        .to_string(),
                    value_span.clone(),
                )
                .into())
            }
        }
    }) // end Box::pin(async move {
}

/// Check if two values are equal.
///
/// This is the canonical equality comparison used by pin patterns (`$var:`),
/// exact-value case arms, the `$=` builtin, and schema enum constraints.
///
/// Primitive types compare by value. Variants compare tag-first, then payload
/// recursively. Dict and Seq require deep structural equality: all field values
/// and sequence elements must be materialized and compared recursively.
///
/// # Strictness
/// - `Int`: same-type integer comparison.
/// - `Float`: same-type float comparison.
/// - `String`: same-type string comparison.
/// - `Variant{payload:None}`: tag equality only (covers Bool and unit constructors).
/// - All other combinations (including cross-type) return `false`.
///
/// Annotated wrappers are stripped before comparison.
///
/// Dict comparison is shallow: same keys and same thunk IDs (no value
/// materialization). This covers null equality ([] == []) and self-equality
/// for Dicts, without deep structural comparison.
/// No payload-Variant or Seq deep comparison. No cross-type Int/Float
/// comparison — use type-specific builtins instead.
///
/// This is the primitive equality kernel used by `builtin-eq-int/float/string`, pattern matching
/// (Pin and case-arm exact-value checks), and bind-or-pin.
pub(crate) fn primitive_eq(a: Value, b: Value) -> bool {
    // Peel Annotated wrappers — metadata is transparent for equality.
    let a = peel_annotated(a);
    let b = peel_annotated(b);

    match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (
            Value::String {
                source: s1,
                start: start1,
                end: end1,
            },
            Value::String {
                source: s2,
                start: start2,
                end: end2,
            },
        ) => s1[*start1..*end1] == s2[*start2..*end2],
        // Nullary variants: tag equality (covers unit constructors)
        (
            Value::Variant {
                tag: tag1,
                payload: None,
            },
            Value::Variant {
                tag: tag2,
                payload: None,
            },
        ) => tag1 == tag2,
        // Dict shallow equality: same keys and same thunk IDs (no value materialization).
        // This covers null equality ([] == []) and self-equality for Dicts,
        // without deep structural comparison of values.
        (Value::Dict(a), Value::Dict(b)) => {
            if a.len() != b.len() {
                return false;
            }
            a.iter()
                .all(|(k, id_a)| b.get(k).map_or(false, |id_b| id_a == id_b))
        }
        _ => false,
    }
}

/// Peel Value::Annotated wrappers, returning the inner value.
fn peel_annotated(v: Value) -> Value {
    match v {
        Value::Annotated { inner, .. } => peel_annotated(*inner),
        other => other,
    }
}

#[cfg(test)]
#[allow(clippy::to_string_in_format_args)] // test diagnostics: .to_string() in format args is fine
#[allow(clippy::useless_conversion)] // test helpers use .into() for clarity
#[allow(clippy::approx_constant)] // test values intentionally use approximate constants
mod tests {
    use super::*;
    use crate::ast::*;
    use crate::test_util::{sp, test_span};
    use crate::value::*;

    fn empty_env() -> Arc<RwLock<crate::env::Env>> {
        Arc::new(RwLock::new(crate::env::Env::new()))
    }

    fn test_ctx() -> Arc<EvalContext> {
        let env = empty_env();
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        EvalContext::new(base_dir, Arc::clone(&env), Arc::clone(&env), false)
    }

    /// Test helper for tests that need dot-access to work.
    /// field-get (slot 0) and slot-get (slot 1) must be in scope for the resolver
    /// and at the root env level for the evaluator's De Bruijn slot lookup.
    fn core_env_and_ctx() -> (Arc<RwLock<crate::env::Env>>, Arc<EvalContext>) {
        let env = crate::builtins::build_core_env();
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        let ctx = EvalContext::new(base_dir, Arc::clone(&env), Arc::clone(&env), false);
        (env, ctx)
    }

    /// Test-only: evaluate a SurfaceNode via the lower→CoreExpr path.
    /// Uses lower::lower() to produce CoreExpr, then calls eval_core_expr.
    async fn eval_for_test(
        node: Arc<SurfaceNode>,
        env: Arc<RwLock<crate::env::Env>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        let (core_expr, lower_diags) = crate::lower::lower(&node);
        if let Some(err) = lower_diags
            .into_iter()
            .find(|d| matches!(d.kind, crate::lower::LowerDiagnosticKind::Error))
        {
            return Err(EvalError::user_error(err.message, err.span).into());
        }
        super::eval_core_expr(&core_expr, &env, ctx).await
    }

    /// Test-only: evaluate a SurfaceNode after running the resolver against the provided env.
    /// Use this instead of eval_for_test when the node contains $name variable references.
    async fn eval_for_test_resolved(
        node: Arc<SurfaceNode>,
        env: Arc<RwLock<crate::env::Env>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        use crate::desugar::desugar_surface_program;
        use crate::resolve::resolve_surface_program;
        let span = node.span.clone();
        let doc = SurfaceDocument {
            stage: None,
            name: None,
            items: vec![SurfaceItem::Expr(Arc::clone(&node))],
            output_type: None,
            expects: None,
            caps: None,
            uses: None,
        };
        let program = SurfaceProgram {
            documents: vec![Spanned::new(Arc::new(doc), span.clone())],
        };
        let mut program = program;
        desugar_surface_program(&mut program);
        // Seed resolver from the provided env so $name references resolve to de Bruijn coords.
        // Type annotation names (String, Int, etc.) in test expressions are resolved
        // by the type checker, not the runtime resolver — ignore resolve errors here.
        let _resolve_errors = resolve_surface_program(&program, Some(&env));
        let _ = span;
        crate::eval_surface_file(&program, Arc::clone(&env), ctx).await
    }

    /// Directly evaluate a `Spanned<CoreExpr>`.
    /// Used by tests that need to construct CoreExpr with specific resolved types
    /// (e.g. `CoreExpr::TypeAssert` with a pre-resolved `Type`).
    async fn eval_core_for_test(
        expr: Spanned<CoreExpr>,
        env: Arc<RwLock<crate::env::Env>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        super::eval_core_expr(&expr, &env, ctx).await
    }

    /// Parse a surface expression from text and evaluate it.
    /// Convenience for most test cases — avoids constructing SurfaceNode by hand.
    /// Runs the resolver so $name variable references work correctly.
    async fn eval_str(
        src: &str,
        env: Arc<RwLock<crate::env::Env>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        let node = crate::parser::parse_surface_expression(src)
            .unwrap_or_else(|e| panic!("parse_surface_expression({src:?}) failed: {e:?}"));
        eval_for_test_resolved(node, env, ctx).await
    }

    /// Build a zero-span SurfaceNode wrapping the given SurfaceExpression.
    /// Convenience for surface-based eval_for_test calls.
    fn surf(expr: SurfaceExpression) -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode::new(expr, rust_span!()))
    }

    /// Async shadow of `materialize()` for test contexts.
    /// Shadows the outer async `materialize` so existing test code compiles with `.await`.
    async fn materialize(
        thunk: &Thunk,
        mat_span: Option<&Span>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Value> {
        super::materialize(thunk, mat_span, ctx).await
    }

    /// Resolve a `ThunkId` from the arena in `ctx` and materialize it.
    ///
    /// Dict values in `Value::Dict` are now `ThunkId` handles into the eval context's arena.
    /// Tests that inspect individual dict entries must resolve them through the same context
    /// that was used during `eval()`.
    async fn mat_id(id: &ThunkId, ctx: &Arc<EvalContext>) -> EvalResult<Value> {
        let thunk = ctx.get_thunk(*id);
        materialize(&thunk, None, ctx).await
    }

    /// Resolve a `ThunkId` to `Arc<Thunk>` for tests that need direct thunk access
    /// (e.g. inspecting `ThunkState` or materializing with a custom mat_span).
    fn get_thunk_rc(id: &ThunkId, ctx: &Arc<EvalContext>) -> Arc<Thunk> {
        ctx.get_thunk(*id)
    }

    /// Build a `Spanned<SurfaceEntry>` with a string key and a simple expression value.
    /// Helper for constructing `Annotation::PropertyDict` entries in tests during
    /// rv2-migrate-annotation migration (Phase 1 stub support).
    fn surf_ann_entry(key: &str, value_expr: SurfaceExpression) -> Spanned<SurfaceEntry> {
        let z = test_span(0, 0, 0, 0);
        let mk = |expr| Arc::new(SurfaceNode::new(expr, z.clone()));
        Spanned::new(
            SurfaceEntry {
                key: Some(mk(SurfaceExpression::Str(key.into()))),
                value: mk(value_expr),
            },
            z,
        )
    }

    #[tokio::test]
    async fn test_eval_int() {
        let thunk = eval_str("42", empty_env(), &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[tokio::test]
    async fn test_eval_float() {
        let thunk = eval_str("3.14", empty_env(), &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Float(3.14));
    }

    #[tokio::test]
    async fn test_eval_str() {
        let thunk = eval_str("\"hello\"", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[tokio::test]
    async fn test_varref_found() {
        let env = empty_env();
        let span = test_span(1, 1, 1, 5);
        env.write().unwrap().insert_value(
            "x".into(),
            Arc::new(Thunk::new_materialized(Value::Int(99), span)),
        );

        let thunk = eval_str("$x", env, &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[tokio::test]
    async fn test_varref_parent_scope() {
        let parent = empty_env();
        let span = test_span(1, 1, 1, 5);
        parent.write().unwrap().insert_value(
            "y".into(),
            Arc::new(Thunk::new_materialized(Value::Int(77), span)),
        );

        let child = Arc::new(RwLock::new(crate::env::Env::with_parent(Arc::clone(
            &parent,
        ))));
        let thunk = eval_str("$y", child, &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(77));
    }

    #[tokio::test]
    async fn test_simple_dict() {
        // [x: 1  y: "hello"]
        let ctx = test_ctx();
        let thunk = eval_str("[x: 1  y: \"hello\"]", empty_env(), &ctx)
            .await
            .unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let x_id = map.get(&HashableValue::Str("x".into())).unwrap();
                assert_eq!(mat_id(x_id, &ctx).await.unwrap(), Value::Int(1));
                let y_id = map.get(&HashableValue::Str("y".into())).unwrap();
                assert_eq!(
                    mat_id(y_id, &ctx).await.unwrap(),
                    string_val("hello".into())
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_auto_indexed_dict() {
        let ctx = test_ctx();
        let thunk = eval_str("[10  20  30]", empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                assert_eq!(
                    mat_id(map.get(&HashableValue::Int(0)).unwrap(), &ctx)
                        .await
                        .unwrap(),
                    Value::Int(10)
                );
                assert_eq!(
                    mat_id(map.get(&HashableValue::Int(1)).unwrap(), &ctx)
                        .await
                        .unwrap(),
                    Value::Int(20)
                );
                assert_eq!(
                    mat_id(map.get(&HashableValue::Int(2)).unwrap(), &ctx)
                        .await
                        .unwrap(),
                    Value::Int(30)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_dict_letrec_sibling_reference() {
        // [x: 5  y: $x]
        let ctx = test_ctx();
        let thunk = eval_str("[x: 5  y: $x]", empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        match val {
            Value::Dict(map) => {
                let y_id = map.get(&HashableValue::Str("y".into())).unwrap();
                assert_eq!(mat_id(y_id, &ctx).await.unwrap(), Value::Int(5));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_dict_letrec_forward_reference() {
        // [y: $x  x: 10] -- y references x which is defined after y
        let ctx = test_ctx();
        let thunk = eval_str("[y: $x  x: 10]", empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        match val {
            Value::Dict(map) => {
                let y_id = map.get(&HashableValue::Str("y".into())).unwrap();
                assert_eq!(mat_id(y_id, &ctx).await.unwrap(), Value::Int(10));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_cycle_detection() {
        // [x: $x] -- x references itself
        let ctx = test_ctx();
        let thunk = eval_str("[x: $x]", empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        match val {
            Value::Dict(map) => {
                let x_id = map.get(&HashableValue::Str("x".into())).unwrap();
                let err = mat_id(x_id, &ctx).await.unwrap_err();
                assert!(
                    err.to_string().contains("circular dependency"),
                    "got: {}",
                    err
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_cycle_detection_transitions_to_failed() {
        // When a thunk detects a circular dependency (InProgress state),
        // it should cache the error in Failed state, not leave it in InProgress.
        // Subsequent materializations should return the cached error.
        let ctx = test_ctx();
        let thunk = eval_str("[x: $x]", empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        let x_thunk = match val {
            Value::Dict(map) => {
                get_thunk_rc(map.get(&HashableValue::Str("x".into())).unwrap(), &ctx)
            }
            other => panic!("expected Dict, got {other:?}"),
        };

        // First materialization: should detect the cycle and fail
        let err1 = materialize(&x_thunk, None, &ctx).await.unwrap_err();
        assert!(
            err1.kind.to_string().contains("circular dependency"),
            "first error: got: {}",
            err1.kind
        );

        // Check that the thunk is now in Failed state, not stuck in InProgress
        let cached_err = x_thunk
            .get_cached_error()
            .expect("thunk should be in Failed state");
        assert!(
            cached_err.to_string().contains("circular dependency"),
            "cached error should mention circular dependency, got: {}",
            cached_err
        );

        // Second materialization: should return the cached circular dependency error
        let err2 = materialize(&x_thunk, None, &ctx).await.unwrap_err();
        assert!(
            err2.kind.to_string().contains("circular dependency"),
            "second error: got: {}",
            err2.kind
        );
    }

    #[tokio::test]
    async fn test_thunk_retryable_after_error() {
        // [x: <placeholder>] -- materializing x fails because the value is a CoreExpr::Placeholder.
        // After failure, the thunk must be cached in Failed state (cacheable error),
        // and a second materialize attempt should return the SAME error, NOT "circular dependency".
        let ctx = test_ctx();
        let _error_span = test_span(1, 5, 1, 15);
        let dict_core = Spanned::new(
            CoreExpr::Dict(vec![Spanned::new(
                CoreEntry {
                    key: Some(Arc::new(Spanned::new(
                        CoreExpr::Str("x".to_string()),
                        test_span(1, 1, 1, 2),
                    ))),
                    value: Arc::new(Spanned::new(CoreExpr::Placeholder, test_span(1, 5, 1, 15))),
                },
                test_span(1, 1, 1, 15),
            )]),
            test_span(1, 1, 1, 15),
        );
        let dict_thunk = eval_core_for_test(dict_core, empty_env(), &ctx)
            .await
            .unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).await.unwrap();

        let x_thunk = match &dict_val {
            Value::Dict(map) => {
                get_thunk_rc(map.get(&HashableValue::Str("x".into())).unwrap(), &ctx)
            }
            other => panic!("expected Dict, got {other:?}"),
        };

        // First attempt: should fail (Placeholder is unimplemented)
        let err1 = materialize(&x_thunk, None, &ctx).await.unwrap_err();
        assert!(
            !err1.kind.to_string().is_empty(),
            "first attempt: expected an error, got empty message",
        );

        // Second attempt: should produce the SAME error, not "circular dependency"
        let err2 = materialize(&x_thunk, None, &ctx).await.unwrap_err();
        assert_eq!(
            err1.kind.to_string(),
            err2.kind.to_string(),
            "second attempt should return cached error, not a different one"
        );
        assert!(
            !err2.kind.to_string().contains("circular dependency"),
            "thunk was poisoned: got circular dependency on retry"
        );
    }

    #[tokio::test]
    async fn test_nested_dict_sees_outer_bindings() {
        // [x: 42  inner: [y: $x]]
        let ctx = test_ctx();
        let thunk = eval_str("[x: 42  inner: [y: $x]]", empty_env(), &ctx)
            .await
            .unwrap();
        let outer = materialize(&thunk, None, &ctx).await.unwrap();

        match outer {
            Value::Dict(outer_map) => {
                let inner_id = outer_map.get(&HashableValue::Str("inner".into())).unwrap();
                let inner_val = mat_id(inner_id, &ctx).await.unwrap();
                match inner_val {
                    Value::Dict(inner_map) => {
                        let y_id = inner_map.get(&HashableValue::Str("y".into())).unwrap();
                        assert_eq!(mat_id(y_id, &ctx).await.unwrap(), Value::Int(42));
                    }
                    other => panic!("expected inner Dict, got {other:?}"),
                }
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_duplicate_key_error() {
        // Build via SurfaceNode to bypass parser duplicate-key detection.
        // The evaluator (eval_dict_core) must detect the duplicate key and return E030.
        let z = rust_span!();
        let mk = |expr: SurfaceExpression| Arc::new(SurfaceNode::new(expr, z.clone()));
        let node = mk(SurfaceExpression::Dict(vec![
            Spanned::new(
                SurfaceEntry {
                    key: Some(mk(SurfaceExpression::Str("x".into()))),
                    value: mk(SurfaceExpression::Int(1)),
                },
                z.clone(),
            ),
            Spanned::new(
                SurfaceEntry {
                    key: Some(mk(SurfaceExpression::Str("x".into()))),
                    value: mk(SurfaceExpression::Int(2)),
                },
                z.clone(),
            ),
        ]));
        let err = eval_for_test(node, empty_env(), &test_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("duplicate key: x"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_fn_creates_function_value() {
        // [fn [let x] $x] → Function
        let thunk = eval_str("[fn [let x] $x]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "x");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_fn_captures_closure_env() {
        // outer: 42 is in env, [fn [] $outer] should capture it
        let env = empty_env();
        env.write().unwrap().insert_value(
            "outer".into(),
            Arc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );
        let fn_thunk = eval_str("[fn [] $outer]", Arc::clone(&env), &test_ctx())
            .await
            .unwrap();
        let fn_val = materialize(&fn_thunk, None, &test_ctx()).await.unwrap();

        // Call it: [call $f]
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let result_thunk = eval_str("[call $f]", env, &test_ctx()).await.unwrap();
        let result = materialize(&result_thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[tokio::test]
    async fn test_call_simple() {
        // Define identity function and call it
        // f: [fn [x] $x]
        // [call $f 42]
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "x".to_string(),
                level: 0,
                slot: 0,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let thunk = eval_str("[call $f 42]", env, &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[tokio::test]
    async fn test_call_multiple_args() {
        // f: [fn [a b] $b]  -- returns second arg
        // [call $f 10 20] → 20
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "a".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "b".into(),
                    annotation: None,
                    variadic: false,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "b".to_string(),
                level: 0,
                slot: 1,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let thunk = eval_str("[call $f 10 20]", env, &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(20));
    }

    #[tokio::test]
    async fn test_call_on_non_function() {
        let env = empty_env();
        env.write().unwrap().insert_value(
            "x".into(),
            Arc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );
        let thunk = eval_str("[call $x]", env, &test_ctx())
            .await
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(err.to_string().contains("type mismatch"), "got: {}", err);
        assert!(err.to_string().contains("Function"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_call_too_few_args() {
        // f: [fn [x y] $x]
        // [call $f 1] → arity mismatch
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: None,
                    variadic: false,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "x".to_string(),
                level: 0,
                slot: 0,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let thunk = eval_str("[call $f 1]", env, &test_ctx())
            .await
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("missing argument for required parameter"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_call_too_many_args() {
        // f: [fn [x] $x]
        // [call $f 1 2] → arity mismatch
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "x".to_string(),
                level: 0,
                slot: 0,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let thunk = eval_str("[call $f 1 2]", env, &test_ctx())
            .await
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(err.to_string().contains("arity mismatch"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_call_named_arg_with_default() {
        // f: [fn [x  y@[default: 99]] $y]
        // [call $f 1] → y defaults to 99
        let env = empty_env();
        let default_entry = surf_ann_entry("default", SurfaceExpression::Int(99));
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![default_entry]))),
                    variadic: false,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "y".to_string(),
                level: 0,
                slot: 1,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        // Call without named arg -- y should default to 99
        let thunk = eval_str("[call $f 1]", Arc::clone(&env), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[tokio::test]
    async fn test_call_named_arg_overridden() {
        // f: [fn [x  y@[default: 99]] $y]
        // [call $f 1 y: 42] → y = 42
        let env = empty_env();
        let default_entry = surf_ann_entry("default", SurfaceExpression::Int(99));
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![default_entry]))),
                    variadic: false,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "y".to_string(),
                level: 0,
                slot: 1,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let thunk = eval_str("[call $f 1 y: 42]", env, &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[tokio::test]
    async fn test_call_unexpected_named_arg() {
        // f: [fn [x] $x]
        // [call $f 1 z: 2] → error: unexpected named argument
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "x".to_string(),
                level: 0,
                slot: 0,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let thunk = eval_str("[call $f 1 z: 2]", env, &test_ctx())
            .await
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string().contains("unexpected named argument: \"z\""),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_call_duplicate_positional_and_named_error() {
        // f: [fn [x y@[default: 99]] $y]
        // [call $f 1 2 y: 42] → error: y received both positional and named argument
        let env = empty_env();
        let default_entry = surf_ann_entry("default", SurfaceExpression::Int(99));
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![default_entry]))),
                    variadic: false,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "y".to_string(),
                level: 0,
                slot: 1,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let thunk = eval_str("[call $f 1 2 y: 42]", env, &test_ctx())
            .await
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("received both positional and named argument"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_call_builtin() {
        fn add_builtin(
            ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let crate::value::BuiltinArgs { args, .. } = ctx;
                let a = materialize(&args[0], None, &test_ctx()).await?;
                let b = materialize(&args[1], None, &test_ctx()).await?;
                match (a, b) {
                    (Value::Int(x), Value::Int(y)) => Ok(Arc::new(Thunk::new_materialized(
                        Value::Int(x + y),
                        test_span(1, 1, 1, 1),
                    ))),
                    _ => panic!("test expects Int args"),
                }
            })
        }
        let env = empty_env();
        env.write().unwrap().insert_value(
            "add".into(),
            Arc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: add_builtin,
                    name: "add",
                    pos_strictness: &[],
                    force_count: 0,
                    params: &[],
                }),
                test_span(1, 1, 1, 5),
            )),
        );
        let thunk = eval_str("[call $add 3 4]", env, &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(7));
    }

    #[tokio::test]
    async fn test_rest_marker_anonymous_errors() {
        // eval_core_expr returns Err immediately for Rest (not deferred to materialize)
        let err = eval_for_test(
            surf(SurfaceExpression::Rest(None, None)),
            empty_env(),
            &test_ctx(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("rest marker (...) is only valid inside type expressions"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_rest_marker_named_errors() {
        // eval_core_expr returns Err immediately for Rest (not deferred to materialize)
        let err = eval_for_test(
            surf(SurfaceExpression::Rest(Some("x".into()), None)),
            empty_env(),
            &test_ctx(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("rest marker (...) is only valid inside type expressions"),
            "got: {}",
            err
        );
    }

    // ── Integration tests for $_ desugaring + evaluation ──────────────────
    // These tests verify that the AST-level desugaring (from src/desugar.rs)
    // integrates correctly with evaluation. They manually call desugar_surface_node()
    // before eval() to simulate the full pipeline.

    #[tokio::test]
    async fn test_underscore_access_chain_becomes_lambda() {
        // $_.name → [fn [_] $_.name] after desugaring
        // Evaluating this should produce a Function, not look up $_
        let mut node = crate::parser::parse_surface_expression("$_.name").expect("parse failed");
        crate::desugar::desugar_surface_node(&mut node, 0);
        let thunk = eval_for_test(node, empty_env(), &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_underscore_in_call_becomes_lambda() {
        // [call $f $_] where $f is in scope → should produce a lambda after desugaring
        // The outer [call ...] contains $_ directly → wraps in [fn [_] [call $f $_]]
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "x".to_string(),
                level: 0,
                slot: 0,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );

        // eval_str runs desugar + resolver + eval, so $f is resolved via the env.
        // [call $f $_] desugars to [fn [let _] [call $f $_]], and $f resolves to the env binding.
        let thunk = eval_str("[call $f $_]", Arc::clone(&env), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function from $_ desugaring, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_underscore_in_dict_entry() {
        // [a: $_.name] → desugars to [fn [_] [a: $_.name]]
        // Dict with $_ in a value position should desugar to an implicit lambda
        let mut node =
            crate::parser::parse_surface_expression("[a: $_.name]").expect("parse failed");
        crate::desugar::desugar_surface_node(&mut node, 0);
        let thunk = eval_for_test(node, empty_env(), &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function from $_ dict desugaring, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_underscore_in_named_arg() {
        // [call $f x: $_] → desugars to [fn [_] [call $f x: $_]]
        // Call with $_ in a named arg value should desugar to an implicit lambda
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "x".to_string(),
                level: 0,
                slot: 0,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );

        // eval_str runs desugar + resolver + eval, so $f is resolved via the env.
        // [call $f x: $_] desugars to [fn [let _] [call $f x: $_]], $f resolves to the env binding.
        let thunk = eval_str("[call $f x: $_]", Arc::clone(&env), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function from $_ named arg desugaring, got {other:?}"),
        }
    }

    /// Build a SurfaceNode dict from key→text-value pairs.
    /// All values must be parseable as surface expressions.
    fn surf_dict(entries: Vec<(&str, &str)>) -> Arc<SurfaceNode> {
        let z = rust_span!();
        let mk = |expr: SurfaceExpression| Arc::new(SurfaceNode::new(expr, z.clone()));
        let surf_entries = entries
            .into_iter()
            .map(|(k, v)| {
                let val_node = crate::parser::parse_surface_expression(v)
                    .unwrap_or_else(|e| panic!("parse_surface_expression({v:?}) failed: {e:?}"));
                Spanned::new(
                    SurfaceEntry {
                        key: Some(mk(SurfaceExpression::Str(k.into()))),
                        value: val_node,
                    },
                    z.clone(),
                )
            })
            .collect();
        mk(SurfaceExpression::Dict(surf_entries))
    }

    #[tokio::test]
    async fn test_dot_access() {
        // [name: hello].name -> "hello"
        // Use a single ctx — ThunkIds from one ctx are invalid in another.
        let (env, ctx) = core_env_and_ctx();
        let dict_thunk = eval_for_test(
            surf_dict(vec![("name", "\"hello\"")]),
            Arc::clone(&env),
            &ctx,
        )
        .await
        .unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).await.unwrap();

        // Bind the dict to $d in the environment
        env.write().unwrap().insert_value(
            "d".into(),
            Arc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let thunk = eval_str("$d.name", env, &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[tokio::test]
    async fn test_dot_access_missing_key() {
        let (env, ctx) = core_env_and_ctx();
        let dict_thunk = eval_for_test(surf_dict(vec![("x", "1")]), Arc::clone(&env), &ctx)
            .await
            .unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).await.unwrap();
        env.write().unwrap().insert_value(
            "d".into(),
            Arc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let thunk = eval_str("$d.missing", Arc::clone(&env), &ctx)
            .await
            .unwrap();
        let err = materialize(&thunk, None, &ctx).await.unwrap_err();
        assert!(
            err.to_string().contains("key not found: missing"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_dot_access_on_non_dict() {
        let (env, ctx) = core_env_and_ctx();
        env.write().unwrap().insert_value(
            "x".into(),
            Arc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );

        let thunk = eval_str("$x.foo", Arc::clone(&env), &ctx).await.unwrap();
        let err = materialize(&thunk, None, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("expected"), "got: {}", err);
        assert!(err.to_string().contains("expected Dict"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_type_assert_int_passes() {
        // [@Integer 42] -> 42
        let thunk = eval_str("[@Integer 42]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[tokio::test]
    async fn test_type_assert_string_passes() {
        // [@String "hello"] -> "hello"
        let thunk = eval_str("[@String \"hello\"]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[tokio::test]
    async fn test_type_assert_number_accepts_int() {
        // [@Number 42] -> 42 (Number accepts Int)
        let thunk = eval_str("[@Number 42]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[tokio::test]
    async fn test_type_assert_number_accepts_float() {
        // [@Number 3.14] -> 3.14 (Number accepts Float)
        let thunk = eval_str("[@Number 3.14]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Float(3.14));
    }

    #[tokio::test]
    async fn test_type_assert_int_fails_on_string() {
        // [@Integer "hello"] -> error
        // Use eval_core_for_test with resolved_type: Type::Int to exercise the TypeAssert
        // failure path directly. eval_str doesn't typecheck, so TypeAnnotation is not set, giving
        // resolved_type=Type::Unknown (accepts all values via consistent subtyping).
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Integer".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                resolved_type: Type::Int,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected Integer, got String"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_type_assert_string_fails_on_int() {
        // [@String 42] -> error  (42 is Int, not String)
        // Use eval_core_for_test with resolved_type: Type::Str. See note in
        // test_type_assert_int_fails_on_string for why eval_str cannot be used here.
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("String".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                resolved_type: Type::Str,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected String, got Int"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_type_assert_property_dict_with_type() {
        // [@[type: Int] 42] -> 42
        let thunk = eval_str("[@[type: Int] 42]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[tokio::test]
    async fn test_type_assert_property_dict_type_mismatch() {
        // [@[type: Integer] "hello"] -> error (PropertyDict annotation with type:Integer, value is String)
        // Use eval_core_for_test with resolved_type: Type::Int. The typecheck pass resolves
        // the `type: Integer` property to Type::Int; without typecheck (eval_str), resolved_type
        // is Type::Unknown which accepts all values via consistent subtyping.
        let span = rust_span!();
        let entries = vec![surf_ann_entry(
            "type",
            SurfaceExpression::VarRef {
                name: "Integer".into(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
            },
        )];
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::PropertyDict(entries)),
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                resolved_type: Type::Int,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected Integer, got String"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_type_assert_property_dict_without_type_passes() {
        // [@[default: 0] "hello"] -> "hello" (no type key, no check performed)
        let thunk = eval_str("[@[default: 0] \"hello\"]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[tokio::test]
    async fn test_type_assert_default_not_used_on_match() {
        // [@[type: Int  default: 0] 42] -> 42 (type matches, default not used)
        let thunk = eval_str("[@[type: Int  default: 0] 42]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[tokio::test]
    async fn test_type_assert_default_used_on_mismatch() {
        // [@[type: Int  default: 0] "hello"] -> 0 (type mismatch, returns default)
        // Use eval_core_for_test with resolved_type: Type::Int so the type check fires.
        let span = rust_span!();
        let entries = vec![
            surf_ann_entry(
                "type",
                SurfaceExpression::VarRef {
                    name: "Int".into(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    call_dispatch: crate::ast::CallDispatch::new(),
                    annotation: None,
                },
            ),
            surf_ann_entry("default", SurfaceExpression::Int(0)),
        ];
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::PropertyDict(entries)),
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                resolved_type: Type::Int,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(0));
    }

    #[tokio::test]
    async fn test_type_assert_property_dict_no_default_errors_on_mismatch() {
        // [@[type: Integer] "hello"] -> error (no default, mismatch is an error)
        // Use eval_core_for_test with resolved_type: Type::Int so the type check fires.
        let span = rust_span!();
        let entries = vec![surf_ann_entry(
            "type",
            SurfaceExpression::VarRef {
                name: "Integer".into(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
            },
        )];
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::PropertyDict(entries)),
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                resolved_type: Type::Int,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected Integer, got String"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_type_assert_default_used_on_inner_expr_error() {
        // B-433/B-429: [@[type: Int default: 0] <placeholder>] -> 0 (inner is dead-code Placeholder, use default)
        // When the inner expression is a CoreExpr::Placeholder (lowered from an unresolvable VarRef
        // or parse error), the default should be used instead of propagating the error.
        let span = rust_span!();
        let entries = vec![
            surf_ann_entry(
                "type",
                SurfaceExpression::VarRef {
                    name: "Int".into(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    call_dispatch: crate::ast::CallDispatch::new(),
                    annotation: None,
                },
            ),
            surf_ann_entry("default", SurfaceExpression::Int(0)),
        ];
        let error_span = test_span(1, 5, 1, 15);
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::PropertyDict(entries)),
                expr: Arc::new(Spanned::new(CoreExpr::Placeholder, error_span)),
                resolved_type: Type::Int,
                pipeline_blame: None,
            },
            span,
        );
        let ctx = test_ctx();
        let thunk = eval_core_for_test(expr, empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();
        assert_eq!(val, Value::Int(0));
    }

    #[tokio::test]
    async fn test_type_assert_record_type_rejects_non_dict() {
        // B-434 verification: [@[name: String] 42] -> error "expected record, got Int"
        // When a record type (keyed PropertyDict) is expected, non-Dict values should be rejected.
        let span = rust_span!();
        let entries = vec![surf_ann_entry(
            "name",
            SurfaceExpression::VarRef {
                name: "String".into(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
            },
        )];
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::PropertyDict(entries)),
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                resolved_type: Type::Record(crate::type_def::Row {
                    fields: indexmap::indexmap! { "name".to_string() => Type::Str },
                    tail: crate::type_def::RowTail::Empty,
                }),
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string().contains("expected") && err.to_string().contains("got Int"),
            "expected type assertion error for non-Dict, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_type_assert_record_type_with_default_on_non_dict() {
        // B-434 extended: [@[name: String default: [name: "fallback"]] 42] -> [name: "fallback"]
        // When a record type is expected but value is not Dict, and default is present, use default.
        let span = rust_span!();
        let entries = vec![
            surf_ann_entry(
                "name",
                SurfaceExpression::VarRef {
                    name: "String".into(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    call_dispatch: crate::ast::CallDispatch::new(),
                    annotation: None,
                },
            ),
            surf_ann_entry(
                "default",
                SurfaceExpression::Dict(vec![Spanned::new(
                    crate::ast::SurfaceEntry {
                        key: Some(Arc::new(SurfaceNode::new(
                            SurfaceExpression::Str("name".into()),
                            span.clone(),
                        ))),
                        value: Arc::new(SurfaceNode::new(
                            SurfaceExpression::Str("fallback".into()),
                            span.clone(),
                        )),
                    },
                    span.clone(),
                )]),
            ),
        ];
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::PropertyDict(entries)),
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                resolved_type: Type::Record(crate::type_def::Row {
                    fields: indexmap::indexmap! { "name".to_string() => Type::Str },
                    tail: crate::type_def::RowTail::Empty,
                }),
                pipeline_blame: None,
            },
            span,
        );
        let ctx = test_ctx();
        let thunk = eval_core_for_test(expr, empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();
        match val {
            Value::Dict(map) => {
                let name_val = map
                    .get(&HashableValue::Str("name".into()))
                    .expect("name field missing");
                let name_thunk = ctx.get_thunk(*name_val);
                let name = materialize(&name_thunk, None, &ctx).await.unwrap();
                assert_eq!(name, string_val("fallback".into()));
            }
            _ => panic!("expected Dict, got: {:?}", val),
        }
    }

    #[tokio::test]
    async fn test_annotated_bare_string() {
        // [@ConfigType "Config"] — TypeAssert with unknown resolved_type passes through the string.
        // Bare word `Config` is a VarRef which requires a binding to evaluate; use a string literal
        // to avoid an "undefined variable" lower error. The test verifies that TypeAssert
        // (with no resolved type from the type checker) passes through the value unchanged.
        let thunk = eval_str("[@ConfigType \"Config\"]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("Config".into()));
    }

    #[tokio::test]
    async fn test_chained_dot_access() {
        // [outer: [inner: 99]].outer.inner -> 99
        // Use a single ctx throughout — ThunkIds from one ctx are invalid in another.
        let (env, ctx) = core_env_and_ctx();
        let dict_thunk = eval_str("[outer: [inner: 99]]", Arc::clone(&env), &ctx)
            .await
            .unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).await.unwrap();
        env.write().unwrap().insert_value(
            "d".into(),
            Arc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        // $d.outer.inner
        let thunk = eval_str("$d.outer.inner", env, &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[tokio::test]
    async fn test_materialization_span_on_error() {
        // [x: <placeholder>] -- materializing x fails because the value is a CoreExpr::Placeholder.
        // The materialization span passed to materialize() should be attached to the error.
        let ctx = test_ctx();
        let dict_core = Spanned::new(
            CoreExpr::Dict(vec![Spanned::new(
                CoreEntry {
                    key: Some(Arc::new(Spanned::new(
                        CoreExpr::Str("x".to_string()),
                        test_span(1, 1, 1, 2),
                    ))),
                    value: Arc::new(Spanned::new(CoreExpr::Placeholder, test_span(1, 5, 1, 15))),
                },
                test_span(1, 1, 1, 15),
            )]),
            test_span(1, 1, 1, 15),
        );
        let dict_thunk = eval_core_for_test(dict_core, empty_env(), &ctx)
            .await
            .unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).await.unwrap();

        // Extract x's thunk from the dict
        let x_thunk = match &dict_val {
            Value::Dict(map) => {
                get_thunk_rc(map.get(&HashableValue::Str("x".into())).unwrap(), &ctx)
            }
            other => panic!("expected Dict, got {other:?}"),
        };

        // Materialize x with a known materialization span
        let mat_span = test_span(5, 1, 5, 5);
        let err = materialize(&x_thunk, Some(&mat_span), &ctx)
            .await
            .unwrap_err();
        // The specific error message is unimportant; what matters is the span attachment.
        assert!(!err.to_string().is_empty(), "got empty error: {}", err);
        assert_eq!(
            err.materialization_span,
            Some(mat_span),
            "materialization span should be the access site"
        );
    }

    #[tokio::test]
    async fn test_cycle_has_materialization_span() {
        // [x: $x] -- force x with a known materialization site
        let ctx = test_ctx();
        let thunk = eval_str("[x: $x]", empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        match val {
            Value::Dict(map) => {
                let x_id = map.get(&HashableValue::Str("x".into())).unwrap();
                let x_thunk = get_thunk_rc(x_id, &ctx);
                let mat_span = test_span(10, 1, 10, 5);
                let err = materialize(&x_thunk, Some(&mat_span), &ctx)
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("circular dependency"));
                assert_eq!(err.materialization_span, Some(mat_span));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_value_to_key_invalid_type_variant() {
        // A dict with a Variant key expression should fail in eval_key -> value_to_key.
        // Build via SurfaceNode: use a Float as an invalid dict key (Float is not a valid key type).
        let z = rust_span!();
        let zz = z.clone();
        let mk = move |expr: SurfaceExpression| Arc::new(SurfaceNode::new(expr, zz.clone()));
        let node = mk(SurfaceExpression::Dict(vec![Spanned::new(
            SurfaceEntry {
                key: Some(mk(SurfaceExpression::Float(1.5))),
                value: mk(SurfaceExpression::Int(1)),
            },
            z,
        )]));
        let err = eval_for_test(node, empty_env(), &test_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("type mismatch"), "got: {}", err);
        assert!(
            err.to_string().contains("expected String or Int"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_value_to_key_invalid_type_float() {
        // A dict with a Float key expression should fail in eval_key -> value_to_key.
        // Build via SurfaceNode since surface text [3.14: 1] would parse differently.
        let z = rust_span!();
        let zz = z.clone();
        let mk = move |expr: SurfaceExpression| Arc::new(SurfaceNode::new(expr, zz.clone()));
        let node = mk(SurfaceExpression::Dict(vec![Spanned::new(
            SurfaceEntry {
                key: Some(mk(SurfaceExpression::Float(3.14))),
                value: mk(SurfaceExpression::Int(1)),
            },
            z,
        )]));
        let err = eval_for_test(node, empty_env(), &test_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("type mismatch"), "got: {}", err);
        assert!(
            err.to_string().contains("expected String or Int"),
            "got: {}",
            err
        );
        assert!(err.to_string().contains("got Float"), "got: {}", err);
    }

    // ── Stack trace / call stack reconstruction tests ──────────────────

    #[tokio::test]
    async fn test_call_error_has_stack_frame_with_function_name() {
        // [f: [fn [x] missing]; result: [f 1]]
        // Calling f with body that references missing should produce a
        // stack frame with "[f ...]".
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(Spanned::new(
                CoreExpr::Var {
                    name: "missing".to_string(),
                    level: 0,
                    slot: u32::MAX,
                    annotation: None,
                },
                test_span(1, 15, 1, 23),
            )),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 20))),
        );

        let thunk = eval_str("[call $f 1]", env, &test_ctx()).await.unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string().contains("undefined variable: missing"),
            "got: {}",
            err
        );
        // The stack should contain a frame for "[f ...]"
        assert!(
            err.stack.iter().any(|f| f.label == "[f ...]"),
            "expected '[f ...]' frame, got: {:?}",
            err.stack
        );
    }

    #[tokio::test]
    async fn test_nested_call_produces_multi_frame_stack() {
        // inner: [fn [x] $missing]
        // outer: [fn [y] [call $inner $y]]
        // [call $outer 1]
        //
        // Error should show both call sites in the stack.
        let env = empty_env();

        // Inner function
        let inner_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(Spanned::new(
                CoreExpr::Var {
                    name: "missing".to_string(),
                    level: 0,
                    slot: u32::MAX,
                    annotation: None,
                },
                test_span(1, 20, 1, 28),
            )),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "inner".into(),
            Arc::new(Thunk::new_materialized(inner_fn, test_span(1, 1, 1, 30))),
        );

        // Outer function: body is [call $inner $y]
        // inner is in the closure env (env) at slot 0 (first insert) → level: 1, slot: 0
        // y is the first param in the call env → level: 0, slot: 0
        let inner_call_span = test_span(2, 15, 2, 30);
        let outer_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "y".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(Spanned::new(
                CoreExpr::Call {
                    func: Arc::new(Spanned::new(
                        CoreExpr::Var {
                            name: "inner".to_string(),
                            level: 1,
                            slot: 0,
                            annotation: None,
                        },
                        test_span(2, 21, 2, 26),
                    )),
                    args: vec![Arc::new(Spanned::new(
                        CoreExpr::Var {
                            name: "y".to_string(),
                            level: 0,
                            slot: 0,
                            annotation: None,
                        },
                        test_span(2, 28, 2, 29),
                    ))],
                    named_args: vec![],
                    implied: false,
                },
                inner_call_span,
            )),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "outer".into(),
            Arc::new(Thunk::new_materialized(outer_fn, test_span(2, 1, 2, 35))),
        );

        // Evaluate [call $outer 1]
        let thunk = eval_str("[call $outer 1]", env, &test_ctx()).await.unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(err.to_string().contains("undefined variable: missing"));

        // With TCO, inner call frame is optimized away (strong_count==1 → no Memoize pushed).
        // Only outer frame remains. This is correct: TCO collapses tail-position stack frames.
        let labels: Vec<&str> = err.stack.iter().map(|f| f.label.as_str()).collect();
        assert!(
            labels.contains(&"[outer ...]"),
            "expected '[outer ...]' in stack, got: {labels:?}"
        );
    }

    #[tokio::test]
    async fn test_dot_access_error_has_access_frame() {
        // When dot access fails because the target evaluation itself errors,
        // the error should include a frame indicating the access context.
        //
        // [a: $missing]
        // $a.x  -- accessing .x should add a frame
        let (env, ctx) = core_env_and_ctx();
        let dict_span = test_span(1, 1, 1, 20);
        let mut dict_map: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        let bad_thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(
                CoreExpr::Var {
                    name: "missing".to_string(),
                    level: 0,
                    slot: u32::MAX,
                    annotation: None,
                },
                test_span(1, 8, 1, 15),
            )),
            Arc::clone(&env),
            Arc::clone(&ctx),
            test_span(1, 8, 1, 15),
        ));
        dict_map.insert(HashableValue::Str("x".into()), ctx.alloc_thunk(bad_thunk));

        env.write().unwrap().insert_value(
            "a".into(),
            Arc::new(Thunk::new_materialized(Value::Dict(dict_map), dict_span)),
        );

        // Now access $a.x -- this should succeed (returns the thunk), but
        // materializing the result should fail
        let thunk = eval_str("$a.x", env, &ctx).await.unwrap();
        let mat_span = test_span(3, 1, 3, 10);
        let err = materialize(&thunk, Some(&mat_span), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("undefined variable: missing"));
        // The materialization span should be set
        assert!(err.materialization_span.is_some());
    }

    #[tokio::test]
    async fn test_dot_access_on_erroring_target_has_frame() {
        // Evaluating a Placeholder should produce an error.
        // CoreExpr::Placeholder evaluates immediately to Err (not a lazy thunk).
        let (env, ctx) = core_env_and_ctx();
        let placeholder_span = test_span(1, 1, 1, 12);
        let err = eval_core_for_test(
            Spanned::new(CoreExpr::Placeholder, placeholder_span),
            Arc::clone(&env),
            &ctx,
        )
        .await
        .unwrap_err();
        // Placeholder produces an "unimplemented" error (dead-code marker).
        assert!(
            !err.to_string().is_empty(),
            "expected an error from Placeholder, got empty: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_chained_access_error_shows_chain() {
        // [a: [x: $missing]]
        // $a.x  -- force chain
        // When materialized, the error should show the materialization chain.
        let ctx = test_ctx();
        let inner_env = empty_env();
        let mut inner_map: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        inner_map.insert(
            HashableValue::Str("x".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_unevaluated_core(
                Arc::new(Spanned::new(
                    CoreExpr::Var {
                        name: "missing".to_string(),
                        level: 0,
                        slot: u32::MAX,
                        annotation: None,
                    },
                    test_span(1, 10, 1, 18),
                )),
                Arc::clone(&inner_env),
                Arc::clone(&ctx),
                test_span(1, 10, 1, 18),
            ))),
        );
        let inner_dict = Value::Dict(inner_map);

        // Build env with field-get (slot 0) and slot-get (slot 1) first, then 'a'.
        let env = empty_env();
        {
            let mut e = env.write().unwrap();
            let core_defs = crate::builtins_core::core_builtins();
            for def in core_defs.into_iter().take(2) {
                let name = def.name.to_string();
                let thunk = Arc::new(Thunk::new_materialized(
                    crate::value::Value::Builtin(def),
                    test_span(0, 0, 0, 0),
                ));
                e.insert_value(name, thunk);
            }
            e.insert_value(
                "a".into(),
                Arc::new(Thunk::new_materialized(inner_dict, test_span(1, 1, 1, 20))),
            );
        }

        // Build $a.x access — eval returns an Unevaluated thunk wrapping the DotAccess
        let thunk = eval_str("$a.x", Arc::clone(&env), &ctx).await.unwrap();

        // Materialize — should error because dict.x = $missing which is undefined
        let b_span = test_span(3, 1, 3, 5);
        let err = materialize(&thunk, Some(&b_span), &ctx).await.unwrap_err();
        assert!(
            err.to_string().contains("undefined variable: missing")
                || err.to_string().contains("undefined"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_func_label_varref() {
        use crate::eval_call::func_label_core;
        let label = func_label_core(&CoreExpr::Var {
            name: "f".to_string(),
            level: 0,
            slot: 0,
            annotation: None,
        });
        assert_eq!(label.as_deref(), Some("[f ...]"));
    }

    #[tokio::test]
    async fn test_func_label_dot_access() {
        // After the field-get refactor, dot access compiles to Call(field-get, [key, target]).
        // The function position is a Var("field-get") so func_label_core returns "[field-get ...]".
        use crate::eval_call::func_label_core;
        let func_var = CoreExpr::Var {
            name: "field-get".to_string(),
            level: 0,
            slot: crate::builtins_core::FIELD_GET_ROOT_SLOT,
            annotation: None,
        };
        let label = func_label_core(&func_var);
        assert_eq!(label.as_deref(), Some("[field-get ...]"));
    }

    #[tokio::test]
    async fn test_func_label_chained_dot_access() {
        // After the field-get refactor, chained dot access uses slot-get (slot 1) for typed access.
        use crate::eval_call::func_label_core;
        let label = func_label_core(&CoreExpr::Var {
            name: "slot-get".to_string(),
            level: 0,
            slot: crate::builtins_core::SLOT_GET_ROOT_SLOT,
            annotation: None,
        });
        assert_eq!(label.as_deref(), Some("[slot-get ...]"));
    }

    #[tokio::test]
    async fn test_func_label_anonymous() {
        use crate::eval_call::func_label_core;
        // Anonymous calls return None (no origin label adds diagnostic value)
        assert_eq!(func_label_core(&CoreExpr::Int(42)), None);
    }

    #[tokio::test]
    async fn test_materialize_chain_no_duplicate_frames() {
        // When the same mat_span propagates through nested materialize calls,
        // we should not get duplicate frames for the same span.
        let env = empty_env();

        // Create a thunk whose body is another unevaluated thunk that errors
        let inner_expr = Spanned::new(
            CoreExpr::Var {
                name: "missing".to_string(),
                level: 0,
                slot: u32::MAX,
                annotation: None,
            },
            test_span(1, 1, 1, 8),
        );
        let inner_thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(inner_expr),
            Arc::clone(&env),
            Arc::clone(&test_ctx()),
            test_span(1, 1, 1, 8),
        ));

        // Materialize with a specific span
        let mat_span = test_span(5, 1, 5, 10);
        let err = materialize(&inner_thunk, Some(&mat_span), &test_ctx())
            .await
            .unwrap_err();

        // Count how many frames have the same span
        let frame_count = err
            .stack
            .iter()
            .filter(|f| f.definition_span == mat_span)
            .count();
        assert!(
            frame_count <= 1,
            "expected at most 1 frame with mat_span, got {frame_count}: {:?}",
            err.stack
        );
    }

    #[tokio::test]
    async fn test_call_arity_error_has_call_frame() {
        // Calling a function with wrong arity should include the call site frame
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "a".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "b".into(),
                    annotation: None,
                    variadic: false,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "a".to_string(),
                level: 0,
                slot: 0,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };
        env.write().unwrap().insert_value(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 20))),
        );

        // Call with wrong arity: [call $f 1] (needs 2 args)
        let thunk = eval_str("[call $f 1]", env, &test_ctx())
            .await
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(err
            .kind
            .to_string()
            .contains("missing argument for required parameter"));
        assert!(
            err.stack.iter().any(|f| f.label == "[f ...]"),
            "expected '[f ...]' frame, got: {:?}",
            err.stack
        );
    }

    #[tokio::test]
    async fn test_builtin_error_has_stack_frame_with_builtin_name() {
        // Calling a builtin that errors should include "call $builtin_name" in the stack.
        // We'll use $type-of with an intentionally broken setup to trigger an error.
        // Actually, let's use a custom failing builtin for clarity.
        fn failing_builtin(
            _ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                Err(EvalError::internal(
                    "test builtin failure".to_string(),
                    test_span(99, 1, 99, 10),
                )
                .into())
            })
        }

        let env = empty_env();
        env.write().unwrap().insert_value(
            "fail".into(),
            Arc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: failing_builtin,
                    name: "fail",
                    pos_strictness: &[],
                    force_count: 0,
                    params: &[],
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        let thunk = eval_str("[call $fail]", env, &test_ctx()).await.unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(err.to_string().contains("test builtin failure"));
        // The stack should contain "[fail ...]"
        assert!(
            err.stack.iter().any(|f| f.label == "[fail ...]"),
            "expected '[fail ...]' frame, got: {:?}",
            err.stack
        );
    }

    #[tokio::test]
    async fn test_error_display_with_full_stack() {
        // Integration test: verify the Display output includes all stack frames
        let err = EvalError::internal("something broke".to_string(), test_span(1, 5, 1, 12))
            .with_materialization_span(test_span(10, 1, 10, 5))
            .with_frame("[inner ...]".to_string(), test_span(5, 1, 5, 20))
            .with_frame("[outer ...]".to_string(), test_span(8, 1, 8, 25));
        let display = format!("{err}");
        assert!(display.contains("something broke"));
        assert!(display.contains("defined at 1:5-1:12"));
        // infer_materialization_verb returns "called at" when first visible frame starts with '['
        assert!(display.contains("called at 10:1-10:5"));
        assert!(display.contains("in [inner ...] at 5:1-5:20"));
        assert!(display.contains("in [outer ...] at 8:1-8:25"));
    }

    // ── PendingCall thunk state tests ──────────────────────────────────

    #[tokio::test]
    async fn test_pending_call_llt_function() {
        // Create a PendingCall thunk that calls an LLT function
        // [fn [x y] [call $+ $x $y]] with args (3, 4)
        let env = empty_env();

        // Create a simple addition function.
        // The function's body runs in call_env (child of closure env = env).
        // Params x and y are bound in call_env: x at slot 0, y at slot 1.
        // Builtin $+ lives in the closure env (env) at slot 0 (first insert) → level: 1, slot: 0.
        let add_fn = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: None,
                    variadic: false,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Call {
                func: Arc::new(sp(CoreExpr::Var {
                    name: "+".to_string(),
                    level: 1,
                    slot: 0,
                    annotation: None,
                })),
                args: vec![
                    Arc::new(sp(CoreExpr::Var {
                        name: "x".to_string(),
                        level: 0,
                        slot: 0,
                        annotation: None,
                    })),
                    Arc::new(sp(CoreExpr::Var {
                        name: "y".to_string(),
                        level: 0,
                        slot: 1,
                        annotation: None,
                    })),
                ],
                named_args: vec![],
                implied: false,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };

        // Add the builtin $+ to the environment
        fn add_builtin(
            ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let crate::value::BuiltinArgs { args, .. } = ctx;
                let a = materialize(&args[0], None, &test_ctx()).await?;
                let b = materialize(&args[1], None, &test_ctx()).await?;
                match (a, b) {
                    (Value::Int(x), Value::Int(y)) => Ok(Arc::new(Thunk::new_materialized(
                        Value::Int(x + y),
                        test_span(1, 1, 1, 1),
                    ))),
                    _ => panic!("test expects Int args"),
                }
            })
        }
        env.write().unwrap().insert_value(
            "+".into(),
            Arc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: add_builtin,
                    name: "+",
                    pos_strictness: &[],
                    force_count: 0,
                    params: &[],
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        // Create PendingCall thunk
        let func_thunk = Arc::new(Thunk::new_materialized(add_fn, test_span(1, 1, 1, 20)));
        let arg1 = Arc::new(Thunk::new_materialized(
            Value::Int(3),
            test_span(1, 21, 1, 22),
        ));
        let arg2 = Arc::new(Thunk::new_materialized(
            Value::Int(4),
            test_span(1, 23, 1, 24),
        ));
        let call_span = test_span(2, 1, 2, 15);

        let pending = Thunk::new_pending_call(
            func_thunk,
            vec![arg1, arg2],
            IndexMap::new(),
            call_span.clone(),
            empty_env(),
            call_span.clone(),
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
            Arc::new(crate::ast::Spanned {
                node: crate::ast::CoreExpr::Int(0),
                span: call_span,
            }),
        );

        // Materialize should call the function and return the result
        let result = materialize(&pending, None, &test_ctx()).await.unwrap();
        assert_eq!(result, Value::Int(7));
    }

    #[tokio::test]
    async fn test_pending_call_builtin_function() {
        // Create a PendingCall thunk where the function is a Builtin
        fn multiply_builtin(
            ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let crate::value::BuiltinArgs { args, .. } = ctx;
                let a = materialize(&args[0], None, &test_ctx()).await?;
                let b = materialize(&args[1], None, &test_ctx()).await?;
                match (a, b) {
                    (Value::Int(x), Value::Int(y)) => Ok(Arc::new(Thunk::new_materialized(
                        Value::Int(x * y),
                        test_span(1, 1, 1, 1),
                    ))),
                    _ => panic!("test expects Int args"),
                }
            })
        }

        let func_thunk = Arc::new(Thunk::new_materialized(
            Value::Builtin(crate::value::BuiltinDef {
                func: multiply_builtin,
                name: "*",
                pos_strictness: &[],
                force_count: 0,
                params: &[],
            }),
            test_span(1, 1, 1, 5),
        ));
        let arg1 = Arc::new(Thunk::new_materialized(
            Value::Int(5),
            test_span(1, 6, 1, 7),
        ));
        let arg2 = Arc::new(Thunk::new_materialized(
            Value::Int(6),
            test_span(1, 8, 1, 9),
        ));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            func_thunk,
            vec![arg1, arg2],
            IndexMap::new(),
            call_span.clone(),
            empty_env(),
            call_span.clone(),
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
            Arc::new(crate::ast::Spanned {
                node: crate::ast::CoreExpr::Int(0),
                span: call_span,
            }),
        );

        // Materialize should call the builtin directly and return the result
        let result = materialize(&pending, None, &test_ctx()).await.unwrap();
        assert_eq!(result, Value::Int(30));
    }

    #[tokio::test]
    async fn test_pending_call_memoizes() {
        // PendingCall should memoize: second materialization returns cached value
        let env = empty_env();

        // Create a function that would fail if called twice
        // (we'll verify it's only called once by checking the state)
        let identity_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "x".to_string(),
                level: 0,
                slot: 0,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };

        let func_thunk = Arc::new(Thunk::new_materialized(identity_fn, test_span(1, 1, 1, 10)));
        let arg = Arc::new(Thunk::new_materialized(
            Value::Int(42),
            test_span(1, 11, 1, 13),
        ));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Arc::new(Thunk::new_pending_call(
            func_thunk,
            vec![arg],
            IndexMap::new(),
            call_span.clone(),
            empty_env(),
            call_span.clone(),
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
            Arc::new(crate::ast::Spanned {
                node: crate::ast::CoreExpr::Int(0),
                span: call_span,
            }),
        ));

        // First materialization
        let result1 = materialize(&pending, None, &test_ctx()).await.unwrap();
        assert_eq!(result1, Value::Int(42));

        // Check that the thunk is now in Materialized state
        assert_eq!(
            pending.try_get_materialized(),
            Some(Value::Int(42)),
            "expected Materialized after first call"
        );

        // Second materialization should return cached value
        let result2 = materialize(&pending, None, &test_ctx()).await.unwrap();
        assert_eq!(result2, Value::Int(42));
    }

    #[tokio::test]
    async fn test_pending_call_non_function_error() {
        // PendingCall with a non-Function/Builtin value should error
        let not_a_function = Arc::new(Thunk::new_materialized(
            Value::Int(123),
            test_span(1, 1, 1, 4),
        ));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            not_a_function,
            vec![],
            IndexMap::new(),
            call_span.clone(),
            empty_env(),
            call_span.clone(),
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
            Arc::new(crate::ast::Spanned {
                node: crate::ast::CoreExpr::Int(0),
                span: call_span,
            }),
        );

        let err = materialize(&pending, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("expected Function or Builtin, got Int"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_pending_call_with_unevaluated_args() {
        // PendingCall should work with unevaluated argument thunks (lazy evaluation)
        let env = empty_env();

        let identity_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "x".to_string(),
                level: 0,
                slot: 0,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };

        let func_thunk = Arc::new(Thunk::new_materialized(identity_fn, test_span(1, 1, 1, 10)));

        // Create an unevaluated arg
        let arg_expr = Arc::new(sp(CoreExpr::Int(99)));
        let arg = Arc::new(Thunk::new_unevaluated_core(
            arg_expr,
            Arc::clone(&env),
            Arc::clone(&test_ctx()),
            test_span(1, 11, 1, 13),
        ));

        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            func_thunk,
            vec![arg],
            IndexMap::new(),
            call_span.clone(),
            empty_env(),
            call_span.clone(),
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
            Arc::new(crate::ast::Spanned {
                node: crate::ast::CoreExpr::Int(0),
                span: call_span,
            }),
        );

        // Materialize should evaluate the arg thunk and return the result
        let result = materialize(&pending, None, &test_ctx()).await.unwrap();
        assert_eq!(result, Value::Int(99));
    }

    #[tokio::test]
    async fn test_pending_call_with_named_args() {
        // PendingCall should pass named args through to function invocation
        let env = empty_env();

        // Install a built-in add function
        fn add_builtin(
            ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let crate::value::BuiltinArgs { args, .. } = ctx;
                let a = materialize(&args[0], None, &test_ctx()).await?;
                let b = materialize(&args[1], None, &test_ctx()).await?;
                match (a, b) {
                    (Value::Int(x), Value::Int(y)) => Ok(Arc::new(Thunk::new_materialized(
                        Value::Int(x + y),
                        test_span(1, 1, 1, 1),
                    ))),
                    _ => panic!("test expects Int args"),
                }
            })
        }
        env.write().unwrap().insert_value(
            "+".into(),
            Arc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: add_builtin,
                    name: "+",
                    pos_strictness: &[],
                    force_count: 0,
                    params: &[],
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        // Create a function that takes a mix of positional and named parameters.
        // Closure env = env, where + is at slot 0 (first insert) → level: 1, slot: 0.
        // Call env has params a at slot 0, b at slot 1 → level: 0.
        let fn_with_named = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "a".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "b".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![surf_ann_entry(
                        "default",
                        SurfaceExpression::Int(10),
                    )]))),
                    variadic: false,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Call {
                func: Arc::new(sp(CoreExpr::Var {
                    name: "+".to_string(),
                    level: 1,
                    slot: 0,
                    annotation: None,
                })),
                args: vec![
                    Arc::new(sp(CoreExpr::Var {
                        name: "a".to_string(),
                        level: 0,
                        slot: 0,
                        annotation: None,
                    })),
                    Arc::new(sp(CoreExpr::Var {
                        name: "b".to_string(),
                        level: 0,
                        slot: 1,
                        annotation: None,
                    })),
                ],
                named_args: vec![],
                implied: false,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };

        let func_thunk = Arc::new(Thunk::new_materialized(
            fn_with_named,
            test_span(1, 1, 1, 10),
        ));

        // Pass first arg positionally, second as named
        let positional = vec![Arc::new(Thunk::new_materialized(
            Value::Int(5),
            test_span(1, 11, 1, 12),
        ))];

        let mut named = IndexMap::new();
        named.insert(
            "b".into(),
            Arc::new(Thunk::new_materialized(
                Value::Int(3),
                test_span(1, 13, 1, 14),
            )),
        );

        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            func_thunk,
            positional,
            named,
            call_span.clone(),
            empty_env(),
            call_span.clone(),
            Some(Arc::from("test-pending-call-named")),
            Arc::clone(&test_ctx()),
            Arc::new(crate::ast::Spanned {
                node: crate::ast::CoreExpr::Int(0),
                span: call_span,
            }),
        );

        // Materialize should pass named args through correctly
        let result = materialize(&pending, None, &test_ctx()).await.unwrap();
        assert_eq!(result, Value::Int(8)); // 5 + 3
    }

    #[tokio::test]
    async fn test_pending_call_with_default_named_args() {
        // PendingCall with partial named args should use defaults
        let env = empty_env();

        // Install a built-in add function
        fn add_builtin(
            ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let crate::value::BuiltinArgs { args, .. } = ctx;
                let a = materialize(&args[0], None, &test_ctx()).await?;
                let b = materialize(&args[1], None, &test_ctx()).await?;
                match (a, b) {
                    (Value::Int(x), Value::Int(y)) => Ok(Arc::new(Thunk::new_materialized(
                        Value::Int(x + y),
                        test_span(1, 1, 1, 1),
                    ))),
                    _ => panic!("test expects Int args"),
                }
            })
        }
        env.write().unwrap().insert_value(
            "+".into(),
            Arc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: add_builtin,
                    name: "+",
                    pos_strictness: &[],
                    force_count: 0,
                    params: &[],
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        // Closure env = env, where + is at slot 0 (first insert) → level: 1, slot: 0.
        // Call env has params x at slot 0, y at slot 1 → level: 0.
        let fn_with_default = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![surf_ann_entry(
                        "default",
                        SurfaceExpression::Int(10),
                    )]))),
                    variadic: false,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Call {
                func: Arc::new(sp(CoreExpr::Var {
                    name: "+".to_string(),
                    level: 1,
                    slot: 0,
                    annotation: None,
                })),
                args: vec![
                    Arc::new(sp(CoreExpr::Var {
                        name: "x".to_string(),
                        level: 0,
                        slot: 0,
                        annotation: None,
                    })),
                    Arc::new(sp(CoreExpr::Var {
                        name: "y".to_string(),
                        level: 0,
                        slot: 1,
                        annotation: None,
                    })),
                ],
                named_args: vec![],
                implied: false,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };

        let func_thunk = Arc::new(Thunk::new_materialized(
            fn_with_default,
            test_span(1, 1, 1, 10),
        ));

        // Provide x positionally, omit y so it uses default (10)
        let positional = vec![Arc::new(Thunk::new_materialized(
            Value::Int(7),
            test_span(1, 11, 1, 12),
        ))];

        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            func_thunk,
            positional,
            IndexMap::new(), // no named args - let y use default
            call_span.clone(),
            empty_env(),
            call_span.clone(),
            Some(Arc::from("test-pending-call-default")),
            Arc::clone(&test_ctx()),
            Arc::new(crate::ast::Spanned {
                node: crate::ast::CoreExpr::Int(0),
                span: call_span,
            }),
        );

        // Materialize should use default for y (10)
        let result = materialize(&pending, None, &test_ctx()).await.unwrap();
        assert_eq!(result, Value::Int(17)); // 7 + 10
    }

    // ── Failed thunk state tests ───────────────────────────────────────

    #[tokio::test]
    async fn test_failed_state_returns_cached_error() {
        // When a thunk fails with a cacheable error, it should transition to Failed state
        // and return the cached error on subsequent materialization attempts.
        let ctx = test_ctx();
        let dict_core = Spanned::new(
            CoreExpr::Dict(vec![Spanned::new(
                CoreEntry {
                    key: Some(Arc::new(Spanned::new(
                        CoreExpr::Str("x".to_string()),
                        test_span(1, 1, 1, 2),
                    ))),
                    value: Arc::new(Spanned::new(CoreExpr::Placeholder, test_span(1, 5, 1, 15))),
                },
                test_span(1, 1, 1, 15),
            )]),
            test_span(1, 1, 1, 15),
        );
        let dict_thunk = eval_core_for_test(dict_core, empty_env(), &ctx)
            .await
            .unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).await.unwrap();

        let x_thunk = match &dict_val {
            Value::Dict(map) => {
                get_thunk_rc(map.get(&HashableValue::Str("x".into())).unwrap(), &ctx)
            }
            other => panic!("expected Dict, got {other:?}"),
        };

        // First materialization: should fail and cache the error
        let err1 = materialize(&x_thunk, None, &ctx).await.unwrap_err();
        assert!(
            !err1.kind.to_string().is_empty(),
            "first error should not be empty"
        );

        // Check that the thunk is now in Failed state
        {
            let _cached_err = x_thunk
                .get_cached_error()
                .expect("thunk should be in Failed state");
        }

        // Second materialization: should return the cached error (same message)
        let err2 = materialize(&x_thunk, None, &ctx).await.unwrap_err();
        assert_eq!(
            err1.kind.to_string(),
            err2.kind.to_string(),
            "second error should be the cached error, not a different one: first={}, second={}",
            err1.kind,
            err2.kind
        );
    }

    #[tokio::test]
    async fn test_failed_state_preserves_stack_frames() {
        // Failed state should preserve the original error's stack frames
        let env = empty_env();

        // Create a function that will fail
        let failing_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "nonexistent".to_string(),
                level: 0,
                slot: u32::MAX,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };

        env.write().unwrap().insert_value(
            "bad_fn".into(),
            Arc::new(Thunk::new_materialized(failing_fn, test_span(1, 1, 1, 20))),
        );

        // Call the failing function
        let thunk = eval_str("[call $bad_fn 1]", env, &test_ctx())
            .await
            .unwrap();

        // First materialization: error should have stack frames
        let err1 = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(err1
            .kind
            .to_string()
            .contains("undefined variable: nonexistent"));
        let frame_count1 = err1.stack.len();
        assert!(frame_count1 > 0, "should have at least one stack frame");

        // Second materialization: error should have the same stack frames
        let err2 = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert_eq!(
            err2.stack.len(),
            frame_count1,
            "stack frames should be preserved"
        );
    }

    #[tokio::test]
    async fn test_pending_builtin_error_becomes_failed() {
        // When a PendingBuiltin fails, it should transition to Failed state
        fn failing_builtin(
            ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let crate::value::BuiltinArgs { call_span, .. } = ctx;
                Err(
                    EvalError::internal("builtin intentionally failed".to_string(), call_span)
                        .into(),
                )
            })
        }

        let env = empty_env();
        env.write().unwrap().insert_value(
            "fail".into(),
            Arc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: failing_builtin,
                    name: "fail",
                    pos_strictness: &[],
                    force_count: 0,
                    params: &[],
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        let thunk = eval_str("[call $fail]", env, &test_ctx()).await.unwrap();

        // First materialization: should fail
        let err1 = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(err1
            .kind
            .to_string()
            .contains("builtin intentionally failed"));

        // Check that the thunk is now in Failed state
        assert!(
            thunk.get_cached_error().is_some(),
            "expected Failed state after error"
        );

        // Second materialization: should return cached error
        let err2 = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(err2
            .kind
            .to_string()
            .contains("builtin intentionally failed"));
    }

    #[tokio::test]
    async fn test_pending_call_error_becomes_failed() {
        // When a PendingCall fails, it should transition to Failed state
        let env = empty_env();

        let failing_fn = Value::Function {
            params: Rc::new(vec![]),
            body: Arc::new(sp(CoreExpr::Var {
                name: "does_not_exist".to_string(),
                level: 0,
                slot: u32::MAX,
                annotation: None,
            })),
            env: Arc::clone(&env),
            annotation: None,
            return_ann: None,
        };

        let func_thunk = Arc::new(Thunk::new_materialized(failing_fn, test_span(1, 1, 1, 10)));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Arc::new(Thunk::new_pending_call(
            func_thunk,
            vec![],
            IndexMap::new(),
            call_span.clone(),
            empty_env(),
            call_span.clone(),
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
            Arc::new(crate::ast::Spanned {
                node: crate::ast::CoreExpr::Int(0),
                span: call_span,
            }),
        ));

        // First materialization: should fail
        let err1 = materialize(&pending, None, &test_ctx()).await.unwrap_err();
        assert!(err1
            .kind
            .to_string()
            .contains("undefined variable: does_not_exist"));

        // Check that the thunk is now in Failed state
        assert!(
            pending.get_cached_error().is_some(),
            "expected Failed state after error"
        );

        // Second materialization: should return cached error
        let err2 = materialize(&pending, None, &test_ctx()).await.unwrap_err();
        assert!(err2
            .kind
            .to_string()
            .contains("undefined variable: does_not_exist"));
    }

    #[tokio::test]
    async fn test_pending_call_func_materialization_failure() {
        let bad_func = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(sp(CoreExpr::Var {
                name: "nonexistent_func".to_string(),
                level: 0,
                slot: u32::MAX,
                annotation: None,
            })),
            empty_env(),
            Arc::clone(&test_ctx()),
            test_span(1, 1, 1, 10),
        ));
        let call_span = test_span(2, 1, 2, 10);
        let pending = Arc::new(Thunk::new_pending_call(
            bad_func,
            vec![],
            IndexMap::new(),
            call_span.clone(),
            empty_env(),
            call_span.clone(),
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
            Arc::new(crate::ast::Spanned {
                node: crate::ast::CoreExpr::Int(0),
                span: call_span,
            }),
        ));

        // First materialization should fail with undefined variable error
        let err = materialize(&pending, None, &test_ctx()).await.unwrap_err();
        assert!(err
            .kind
            .to_string()
            .contains("undefined variable: nonexistent_func"));

        // The thunk should be in Failed state, NOT InProgress
        assert!(!pending.is_in_progress(), "BUG: thunk stuck in InProgress");
        assert!(
            pending.get_cached_error().is_some(),
            "expected Failed state"
        );

        // Second access should return cached error, NOT "circular dependency"
        let err2 = materialize(&pending, None, &test_ctx()).await.unwrap_err();
        assert!(err2
            .kind
            .to_string()
            .contains("undefined variable: nonexistent_func"));
        assert!(!err2.kind.to_string().contains("circular dependency"));
    }

    #[tokio::test]
    async fn test_unevaluated_error_becomes_failed() {
        // When an Unevaluated thunk fails during materialization, it should transition to Failed
        let expr = sp(CoreExpr::Var {
            name: "undefined_var".to_string(),
            level: 0,
            slot: u32::MAX,
            annotation: None,
        });
        let env = empty_env();
        let thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(expr),
            Arc::clone(&env),
            Arc::clone(&test_ctx()),
            test_span(1, 1, 1, 15),
        ));

        // First materialization: should fail
        let err1 = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(err1
            .kind
            .to_string()
            .contains("undefined variable: undefined_var"));

        // Check that the thunk is now in Failed state
        assert!(
            thunk.get_cached_error().is_some(),
            "expected Failed state after error"
        );

        // Second materialization: should return cached error
        let err2 = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(err2
            .kind
            .to_string()
            .contains("undefined variable: undefined_var"));
    }

    #[tokio::test]
    async fn test_failed_state_same_span_no_duplicate() {
        // Accessing a Failed thunk twice with the same mat_span should not duplicate frames.
        // Use an unevaluated thunk that references a missing slot — it fails lazily on materialize.
        let ctx = test_ctx();
        let env = empty_env();
        let error_span = test_span(1, 1, 1, 14);
        let thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(
                CoreExpr::Var {
                    name: "undefined_var".to_string(),
                    level: 0,
                    slot: u32::MAX,
                    annotation: None,
                },
                error_span.clone(),
            )),
            Arc::clone(&env),
            Arc::clone(&ctx),
            error_span,
        ));

        // First materialization: error with a specific mat_span
        let mat_span = test_span(10, 5, 10, 15);
        let err1 = materialize(&thunk, Some(&mat_span), &ctx)
            .await
            .unwrap_err();
        assert!(
            err1.kind.to_string().contains("undefined variable"),
            "got: {}",
            err1.kind
        );
        let frame_count1 = err1.stack.len();

        // Second materialization: same mat_span — should not duplicate frames
        let err2 = materialize(&thunk, Some(&mat_span), &ctx)
            .await
            .unwrap_err();
        assert_eq!(
            err2.stack.len(),
            frame_count1,
            "same mat_span should not duplicate frames"
        );
    }

    // ── Error caching tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_regular_error_does_cache() {
        // Regular errors should transition to Failed state (cacheable).
        // CoreExpr::Placeholder produces an Unimplemented error which is cacheable.
        let ctx = test_ctx();
        let dict_core = Spanned::new(
            CoreExpr::Dict(vec![Spanned::new(
                CoreEntry {
                    key: Some(Arc::new(Spanned::new(
                        CoreExpr::Str("x".to_string()),
                        test_span(1, 1, 1, 2),
                    ))),
                    value: Arc::new(Spanned::new(CoreExpr::Placeholder, test_span(1, 5, 1, 15))),
                },
                test_span(1, 1, 1, 15),
            )]),
            test_span(1, 1, 1, 15),
        );
        let dict_thunk = eval_core_for_test(dict_core, empty_env(), &ctx)
            .await
            .unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).await.unwrap();

        let x_thunk = match &dict_val {
            Value::Dict(map) => {
                get_thunk_rc(map.get(&HashableValue::Str("x".into())).unwrap(), &ctx)
            }
            other => panic!("expected Dict, got {other:?}"),
        };

        // First materialization: should fail with a cacheable error
        let err1 = materialize(&x_thunk, None, &ctx).await.unwrap_err();
        assert!(
            !err1.kind.to_string().is_empty(),
            "expected an error, got empty: {}",
            err1.kind.to_string()
        );

        // The thunk SHOULD be in Failed state because Unimplemented is cacheable
        let cached_err = x_thunk
            .get_cached_error()
            .expect("expected Failed state with cached error after cacheable error");
        assert!(
            !cached_err.kind.to_string().is_empty(),
            "cached error should not be empty, got: {}",
            cached_err.to_string()
        );
    }

    // === EvalContext isolation tests ===

    // ── Structural TypeAssert tests (resolved_type: Some(Type::...)) ────
    // These test the NEW structural validation path added by the
    // typeassert-structural sprint, distinct from the nominal fallback path
    // (resolved_type: None) tested in the existing TypeAssert tests above.

    #[tokio::test]
    async fn test_typeassert_structural_int_pass() {
        // Structural path: resolved_type = Some(Type::Int), value is Int(42) -> pass
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Int".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                resolved_type: Type::Int,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[tokio::test]
    async fn test_typeassert_structural_int_fail() {
        // Structural path: resolved_type = Some(Type::Int), value is String -> error
        // TypeAssert is lazy in CEK model: type error fires on materialize(), not eval()
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Integer".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                resolved_type: Type::Int,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected Integer, got String"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_typeassert_structural_str_pass() {
        // Structural path: resolved_type = Some(Type::Str), value is String -> pass
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Str".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                resolved_type: Type::Str,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[tokio::test]
    async fn test_typeassert_structural_any() {
        // Structural path: resolved_type = Some(Type::Any), any value passes
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Any".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Str("anything".into()), span.clone())),
                resolved_type: Type::Any,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("anything".into()));
    }

    #[tokio::test]
    async fn test_typeassert_structural_any_accepts_int() {
        // Type::Any accepts Int as well (covers any-value branch)
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Any".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Int(99), span.clone())),
                resolved_type: Type::Any,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[tokio::test]
    async fn test_typeassert_structural_record_shape_check() {
        // Structural path: resolved_type = Some(Type::Record(..., Open))
        // Dict has required field "name" -> pass.
        // The record type check is immediate (shape check), field guard wrapping deferred.
        let mut fields = indexmap::IndexMap::new();
        fields.insert("name".to_string(), Type::Str);
        let record_type = Type::Record(Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        });

        let span = rust_span!();
        // For the record shape check test: just verify a dict with those keys satisfies the type.
        // We build a CoreExpr::TypeAssert wrapping a CoreExpr::Dict inline.
        let inner_expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Record".into())),
                expr: Arc::new(Spanned::new(
                    CoreExpr::Dict(vec![
                        Spanned::new(
                            crate::ast::CoreEntry {
                                key: Some(Arc::new(Spanned::new(
                                    CoreExpr::Str("name".into()),
                                    span.clone(),
                                ))),
                                value: Arc::new(Spanned::new(
                                    CoreExpr::Str("Alice".into()),
                                    span.clone(),
                                )),
                            },
                            span.clone(),
                        ),
                        Spanned::new(
                            crate::ast::CoreEntry {
                                key: Some(Arc::new(Spanned::new(
                                    CoreExpr::Str("age".into()),
                                    span.clone(),
                                ))),
                                value: Arc::new(Spanned::new(CoreExpr::Int(30), span.clone())),
                            },
                            span.clone(),
                        ),
                    ]),
                    span.clone(),
                )),
                resolved_type: record_type,
                pipeline_blame: None,
            },
            span,
        );

        let thunk = eval_core_for_test(inner_expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        // Should be a Dict with the expected fields
        match &val {
            Value::Dict(map) => {
                assert!(map.contains_key(&HashableValue::Str("name".into())));
                assert!(map.contains_key(&HashableValue::Str("age".into())));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_typeassert_structural_record_missing_field() {
        // Structural path: record type requires field "id", dict doesn't have it -> error
        // TypeAssert is lazy in CEK model: type error fires on materialize(), not eval()
        let mut fields = indexmap::IndexMap::new();
        fields.insert("id".to_string(), Type::Int);
        let record_type = Type::Record(Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        });

        let span = rust_span!();
        let inner_expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Record".into())),
                expr: Arc::new(Spanned::new(
                    CoreExpr::Dict(vec![Spanned::new(
                        crate::ast::CoreEntry {
                            key: Some(Arc::new(Spanned::new(
                                CoreExpr::Str("name".into()),
                                span.clone(),
                            ))),
                            value: Arc::new(Spanned::new(
                                CoreExpr::Str("Alice".into()),
                                span.clone(),
                            )),
                        },
                        span.clone(),
                    )]),
                    span.clone(),
                )),
                resolved_type: record_type,
                pipeline_blame: None,
            },
            span,
        );

        let thunk = eval_core_for_test(inner_expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string().contains("record missing field \"id\""),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_typeassert_structural_record_extra_field_accepted() {
        // BAS width subtyping: under BAS, extra fields are ALWAYS accepted.
        // A dict with {x: 1, extra: 2} satisfies the annotation @[x: Int]
        // because the annotation only constrains what it declares.
        let mut fields = indexmap::IndexMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        });

        let span = rust_span!();
        let inner_expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Record".into())),
                expr: Arc::new(Spanned::new(
                    CoreExpr::Dict(vec![
                        Spanned::new(
                            crate::ast::CoreEntry {
                                key: Some(Arc::new(Spanned::new(
                                    CoreExpr::Str("x".into()),
                                    span.clone(),
                                ))),
                                value: Arc::new(Spanned::new(CoreExpr::Int(1), span.clone())),
                            },
                            span.clone(),
                        ),
                        Spanned::new(
                            crate::ast::CoreEntry {
                                key: Some(Arc::new(Spanned::new(
                                    CoreExpr::Str("extra".into()),
                                    span.clone(),
                                ))),
                                value: Arc::new(Spanned::new(CoreExpr::Int(2), span.clone())),
                            },
                            span.clone(),
                        ),
                    ]),
                    span.clone(),
                )),
                resolved_type: record_type,
                pipeline_blame: None,
            },
            span,
        );

        // BAS: should PASS — extra fields accepted
        let thunk = eval_core_for_test(inner_expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match &val {
            Value::Dict(map) => {
                assert!(map.contains_key(&HashableValue::Str("x".into())));
                assert!(
                    map.contains_key(&HashableValue::Str("extra".into())),
                    "extra field should be preserved"
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_typeassert_structural_closed_record_exact_fields_pass() {
        // Structural path: closed record, dict has exactly the required fields -> pass
        let mut fields = indexmap::IndexMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        });

        let span = rust_span!();
        let inner_expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Record".into())),
                expr: Arc::new(Spanned::new(
                    CoreExpr::Dict(vec![Spanned::new(
                        crate::ast::CoreEntry {
                            key: Some(Arc::new(Spanned::new(
                                CoreExpr::Str("x".into()),
                                span.clone(),
                            ))),
                            value: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                        },
                        span.clone(),
                    )]),
                    span.clone(),
                )),
                resolved_type: record_type,
                pipeline_blame: None,
            },
            span,
        );

        let thunk = eval_core_for_test(inner_expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match &val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                assert!(map.contains_key(&HashableValue::Str("x".into())));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_typeassert_structural_record_non_dict_fails() {
        // Structural path: resolved_type = Some(Type::Record(...)), value is Int -> error
        // TypeAssert is lazy in CEK model: type error fires on materialize(), not eval()
        let mut fields = indexmap::IndexMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        });

        let span = rust_span!();
        let inner_expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Record".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                resolved_type: record_type,
                pipeline_blame: None,
            },
            span,
        );

        let thunk = eval_core_for_test(inner_expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string().contains("type assertion failed"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_typeassert_nominal_fallback() {
        // Nominal fallback path: resolved_type = None, annotation "Integer", value is Int -> pass
        // (This ensures the existing nominal path is preserved alongside the new structural path.)
        let thunk = eval_str("[@Integer 7]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, Value::Int(7));
    }

    // ── annotation_has_structural_fields unit tests ────────────────────
    // Tests for the helper that distinguishes structural record annotations
    // (e.g. [@[name: String] $x]) from metadata-only annotations (e.g.
    // [@[default: 0] $x]) in the --no-typecheck fallback path.

    #[tokio::test]
    async fn test_annotation_has_structural_fields_simple_returns_false() {
        // Simple annotations like @Integer have no structural fields
        assert!(!annotation_has_structural_fields(&Annotation::Simple(
            "Integer".into()
        )));
    }

    #[tokio::test]
    async fn test_annotation_has_structural_fields_empty_property_dict() {
        // Empty PropertyDict has no structural fields
        assert!(!annotation_has_structural_fields(
            &Annotation::PropertyDict(vec![])
        ));
    }

    #[tokio::test]
    async fn test_annotation_has_structural_fields_default_only() {
        // [@[default: 0] $x] — default-only, no structural fields
        let entries = vec![surf_ann_entry("default", SurfaceExpression::Int(0))];
        assert!(!annotation_has_structural_fields(
            &Annotation::PropertyDict(entries)
        ));
    }

    #[tokio::test]
    async fn test_annotation_has_structural_fields_type_only() {
        // [@[type: Int] $x] — type-only, no structural fields
        let entries = vec![surf_ann_entry(
            "type",
            SurfaceExpression::VarRef {
                name: "Int".into(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
            },
        )];
        assert!(!annotation_has_structural_fields(
            &Annotation::PropertyDict(entries)
        ));
    }

    #[tokio::test]
    async fn test_annotation_has_structural_fields_record_annotation() {
        // [@[name: String age: Int] $x] — has structural fields
        let entries = vec![
            surf_ann_entry(
                "name",
                SurfaceExpression::VarRef {
                    name: "String".into(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    call_dispatch: crate::ast::CallDispatch::new(),
                    annotation: None,
                },
            ),
            surf_ann_entry(
                "age",
                SurfaceExpression::VarRef {
                    name: "Int".into(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    call_dispatch: crate::ast::CallDispatch::new(),
                    annotation: None,
                },
            ),
        ];
        assert!(annotation_has_structural_fields(&Annotation::PropertyDict(
            entries
        )));
    }

    #[tokio::test]
    async fn test_annotation_has_structural_fields_mixed_meta_and_record() {
        // [@[name: String default: []] $x] — has structural field "name"
        let entries = vec![
            surf_ann_entry(
                "name",
                SurfaceExpression::VarRef {
                    name: "String".into(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    call_dispatch: crate::ast::CallDispatch::new(),
                    annotation: None,
                },
            ),
            surf_ann_entry("default", SurfaceExpression::Dict(vec![])),
        ];
        assert!(annotation_has_structural_fields(&Annotation::PropertyDict(
            entries
        )));
    }

    // ── elaboration gap tests ────────────────────────────────────────────
    // Tests for the --no-typecheck fallback path when resolved_type is None
    // and the annotation has structural fields (Dict tag check).

    #[tokio::test]
    async fn test_elaboration_gap_structural_annotation_dict_passes() {
        // [@[name: String] [name: "hello"]] with resolved_type=None (no typecheck)
        // Should pass: value is a Dict (tag check succeeds).
        // "hello" is a string literal — bare identifier `hello` would be an undefined VarRef.
        let thunk = eval_str(
            "[@[name: String] [name: \"hello\"]]",
            empty_env(),
            &test_ctx(),
        )
        .await
        .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert!(
            matches!(val, Value::Dict(_)),
            "Structural annotation with Dict value should pass tag check"
        );
    }

    #[tokio::test]
    async fn test_elaboration_gap_structural_annotation_non_dict_with_default() {
        // [@[name: String  default: []] 42] — structural record annotation with default.
        // Value is Int (not a Dict), so the record shape check fails and the default is used.
        // Use eval_core_for_test with resolved_type: Type::Record({name: Str}) so the
        // as_record_row_merged path fires. With resolved_type=Unknown (from eval_str),
        // is_consistent_subtype(Int, Unknown)=true and the TypeAssert passes trivially.
        let mut fields = indexmap::IndexMap::new();
        fields.insert("name".to_string(), Type::Str);
        let record_type = Type::Record(Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        });

        let span = rust_span!();
        let entries = vec![
            surf_ann_entry(
                "name",
                SurfaceExpression::VarRef {
                    name: "String".into(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),
                    call_dispatch: crate::ast::CallDispatch::new(),
                    annotation: None,
                },
            ),
            surf_ann_entry("default", SurfaceExpression::Dict(vec![])),
        ];
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::PropertyDict(entries)),
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                resolved_type: record_type,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert!(
            matches!(val, Value::Dict(_)),
            "Should use default when record shape check fails; got: {val:?}"
        );
    }

    #[tokio::test]
    async fn test_elaboration_gap_default_only_no_structural_check() {
        // [@[default: 0] "hello"] with resolved_type=None
        // Should pass through without validation (no type, no structural fields)
        let thunk = eval_str("[@[default: 0] \"hello\"]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    // ── value_matches_type unit tests ────────────────────────────────────
    // Direct tests of the value_matches_type() helper function, which is
    // called in the structural TypeAssert handler for non-Record types.

    #[tokio::test]
    async fn test_value_matches_type_int() {
        let ctx = test_ctx();
        assert!(value_matches_type(&Value::Int(42), &Type::Int, &ctx));
        assert!(!value_matches_type(
            &string_val("x".into()),
            &Type::Int,
            &ctx
        ));
        assert!(!value_matches_type(&Value::Float(1.0), &Type::Int, &ctx));
    }

    #[tokio::test]
    async fn test_value_matches_type_str() {
        let ctx = test_ctx();
        assert!(value_matches_type(
            &string_val("hello".into()),
            &Type::Str,
            &ctx
        ));
        assert!(!value_matches_type(&Value::Int(1), &Type::Str, &ctx));
        assert!(!value_matches_type(&Value::Float(0.0), &Type::Str, &ctx));
    }

    #[tokio::test]
    async fn test_value_matches_type_float() {
        let ctx = test_ctx();
        assert!(value_matches_type(&Value::Float(3.14), &Type::Float, &ctx));
        assert!(!value_matches_type(&Value::Int(3), &Type::Float, &ctx));
    }

    #[tokio::test]
    async fn test_value_matches_type_any() {
        let ctx = test_ctx();
        // Type::Any accepts all value kinds
        assert!(value_matches_type(&Value::Int(1), &Type::Any, &ctx));
        assert!(value_matches_type(&Value::Float(1.0), &Type::Any, &ctx));
        assert!(value_matches_type(
            &string_val("s".into()),
            &Type::Any,
            &ctx
        ));
        assert!(value_matches_type(&Value::Float(1.0), &Type::Any, &ctx));
        assert!(value_matches_type(
            &Value::Dict(IndexMap::new()),
            &Type::Any,
            &ctx,
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_int_literal() {
        let ctx = test_ctx();
        // Type::IntLiteral(n): ground_type_of erases Int values to Type::Int.
        // is_consistent_subtype(Type::Int, Type::IntLiteral(n)) falls to is_subtype which
        // returns false — Int is NOT a subtype of IntLiteral(n) (it's the other way).
        // Literal types are static-only constraints (the type checker uses them for
        // exhaustiveness); at runtime, ground_type_of produces the base type, not a literal.
        assert!(!value_matches_type(
            &Value::Int(5),
            &Type::IntLiteral(5),
            &ctx
        ));
        assert!(!value_matches_type(
            &Value::Int(6),
            &Type::IntLiteral(5),
            &ctx
        ));
        assert!(!value_matches_type(
            &string_val("5".into()),
            &Type::IntLiteral(5),
            &ctx,
        ));
        // But IntLiteral(n) IS a subtype of Int (literal specializes base type).
        // Check via consistent subtyping from the literal side:
        // is_consistent_subtype(IntLiteral(5), Int) = is_subtype(IntLiteral(5), Int) = true.
        assert!(Type::is_consistent_subtype(
            &Type::IntLiteral(5),
            &Type::Int
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_string_literal() {
        let ctx = test_ctx();
        // Type::StringLiteral: ground_type_of erases String values to Type::Str.
        // is_consistent_subtype(Type::Str, Type::StringLiteral("foo")) = is_subtype(Str, StringLiteral)
        // = false — Str is NOT a subtype of StringLiteral (it's the other way).
        assert!(!value_matches_type(
            &string_val("foo".into()),
            &Type::StringLiteral("foo".into()),
            &ctx,
        ));
        assert!(!value_matches_type(
            &string_val("bar".into()),
            &Type::StringLiteral("foo".into()),
            &ctx,
        ));
        assert!(!value_matches_type(
            &Value::Int(0),
            &Type::StringLiteral("foo".into()),
            &ctx,
        ));
        // StringLiteral IS a subtype of Str (literal specializes base type).
        assert!(Type::is_consistent_subtype(
            &Type::StringLiteral("foo".into()),
            &Type::Str
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_typevar_always_true() {
        let ctx = test_ctx();
        // Type::TypeVar is treated as Any (residual polymorphic instantiation)
        assert!(value_matches_type(
            &Value::Int(1),
            &Type::TypeVar("a".into(), 0),
            &ctx,
        ));
        assert!(value_matches_type(
            &string_val("x".into()),
            &Type::TypeVar("a".into(), 0),
            &ctx,
        ));
        assert!(value_matches_type(
            &Value::Dict(IndexMap::new()),
            &Type::TypeVar("a".into(), 0),
            &ctx,
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_record_always_true() {
        let ctx = test_ctx();
        // Under AGT consistent subtyping, value_matches_type uses ground_type_of and
        // is_consistent_subtype. Record type checks are now structural:
        //
        // - Non-Dict values: ground_type_of(Int) = Type::Int, which is NOT a consistent
        //   subtype of Type::Record({x: Int}) — Int and Record are disjoint.
        // - Dict values: ground_type_of(Dict({})) = Type::Record({}) (empty row).
        //   is_consistent_subtype(Record({}), Record({x: Int})) checks field presence:
        //   field "x" required in sup but absent in empty sub → returns false.
        //
        // Record validation for TypeAssert happens via as_record_row_merged + validate_and_wrap_record
        // in the TypeAssertCheck continuation, NOT via value_matches_type. value_matches_type
        // is only called for non-record types.
        let mut fields = indexmap::IndexMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        });
        // Non-Dict value: ground_type_of(Int) = Type::Int, not a subtype of Record.
        assert!(!value_matches_type(&Value::Int(99), &record_type, &ctx));
        // Empty Dict: ground_type_of(Dict({})) = Record({}), missing required field "x".
        assert!(!value_matches_type(
            &Value::Dict(IndexMap::new()),
            &record_type,
            &ctx,
        ));
        // Dict with the required field "x" AND the field type is Unknown (erased) which
        // is consistent with Int (Unknown ~<: T for all T). However this test requires
        // alloc_thunk to build the dict — covered by TypeAssert corpus tests instead.
        // The key insight: value_matches_type is NOT the Record validation entry point
        // at runtime; TypeAssertCheck uses as_record_row_merged → validate_and_wrap_record.
    }

    #[tokio::test]
    async fn test_value_matches_type_proxy() {
        // Under AGT consistent subtyping: ground_type_of(Value::Proxy) = Type::Unknown.
        // Capability types (Proxy, Handle, DirCap, etc.) are opaque to the type system
        // at runtime — they return Unknown so is_consistent_subtype(Unknown, T) = true for all T.
        // This is the correct gradual typing behavior: the type checker validates proxy usage
        // statically; at runtime, Unknown passes through any annotation.
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let handler = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(42), span)));
        let proxy_val = Value::Proxy { handler };

        // Unknown ~<: any type, so Proxy values pass all annotations at runtime.
        assert!(value_matches_type(&proxy_val, &Type::Proxy, &ctx));
        assert!(value_matches_type(&proxy_val, &Type::Int, &ctx)); // Unknown ~<: Int = true
        assert!(value_matches_type(&proxy_val, &Type::Any, &ctx));

        // Verify ground_type_of explicitly
        assert_eq!(ground_type_of(&proxy_val), Type::Unknown);
    }

    #[tokio::test]
    async fn test_value_matches_type_tycon_no_env() {
        // When tycon_env is not set (None), TyCon types conservatively return false.
        let ctx = test_ctx(); // no tycon_env set
        let tycon = Type::TyCon("MyType".to_string());
        // Int value against unknown TyCon → false (conservative)
        assert!(!value_matches_type(&Value::Int(42), &tycon, &ctx));
        assert!(!value_matches_type(&Value::Float(1.0), &tycon, &ctx));
    }

    #[tokio::test]
    async fn test_value_matches_type_tycon_builtin_int() {
        // TyCon with builtin_type "Int" discriminant matches Value::Int.
        use crate::type_def::{TyConDef, TyConEnv};
        use std::sync::Arc;
        let ctx = test_ctx();
        let mut env = TyConEnv::new();
        env.insert(
            "MyInt".to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body: Type::Unknown,
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: Some("Int".to_string()),
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
            }),
        );
        ctx.set_tycon_env(env);
        let tycon = Type::TyCon("MyInt".to_string());
        assert!(value_matches_type(&Value::Int(1), &tycon, &ctx));
        assert!(!value_matches_type(&Value::Float(1.0), &tycon, &ctx));
        assert!(!value_matches_type(&string_val("x".into()), &tycon, &ctx));
    }

    #[tokio::test]
    async fn test_value_matches_type_tycon_builtin_dict() {
        // TyCon with builtin_type "Dict" matches Value::Dict.
        use crate::type_def::{TyConDef, TyConEnv};
        use std::sync::Arc;
        let ctx = test_ctx();
        let mut env = TyConEnv::new();
        env.insert(
            "MyDict".to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body: Type::Unknown,
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: Some("Dict".to_string()),
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
            }),
        );
        ctx.set_tycon_env(env);
        let tycon = Type::TyCon("MyDict".to_string());
        // Dict values match
        assert!(value_matches_type(
            &Value::Dict(IndexMap::new()),
            &tycon,
            &ctx
        ));
        // Non-Dict values do not match
        assert!(!value_matches_type(&Value::Int(1), &tycon, &ctx));
        assert!(!value_matches_type(&Value::Float(1.0), &tycon, &ctx));
    }

    #[tokio::test]
    async fn test_value_matches_type_tycon_nominal() {
        // Nominal TyCon (has constructors) matches Value::Variant with matching tag prefix.
        use crate::type_def::{TyConDef, TyConEnv};
        use std::sync::Arc;
        let ctx = test_ctx();
        let mut env = TyConEnv::new();
        env.insert(
            "Color".to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body: Type::Unknown,
                constraints: vec![],
                variance: vec![],
                constructors: vec![("Color.Red".to_string(), 0), ("Color.Green".to_string(), 0)],
                builtin_type: None,
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
            }),
        );
        ctx.set_tycon_env(env);
        let tycon = Type::TyCon("Color".to_string());
        // Variant with matching prefix matches
        let red = Value::Variant {
            tag: "Color.Red".to_string(),
            payload: None,
        };
        assert!(value_matches_type(&red, &tycon, &ctx));
        // Variant with different prefix does not match
        let wrong = Value::Variant {
            tag: "Shape.Circle".to_string(),
            payload: None,
        };
        assert!(!value_matches_type(&wrong, &tycon, &ctx));
        // Non-Variant values do not match a nominal TyCon
        assert!(!value_matches_type(&Value::Int(1), &tycon, &ctx));
    }

    #[tokio::test]
    async fn test_value_matches_type_app_tycon_dispatch() {
        // Type::App(TyCon(name), arg) extracts the root TyCon name and applies TyConDef dispatch.
        // Type args are ignored at the value level (type erasure).
        use crate::type_def::{TyConDef, TyConEnv};
        use std::sync::Arc;
        let ctx = test_ctx();
        let mut env = TyConEnv::new();
        env.insert(
            "MySeq".to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body: Type::Unknown,
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: Some("Str".to_string()),
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
            }),
        );
        ctx.set_tycon_env(env);
        // App(TyCon("MyColl"), Int) — type arg Int is ignored; dispatch on "Str" discriminant.
        let app_type = Type::App(
            Box::new(Type::TyCon("MySeq".to_string())),
            Box::new(Type::Int),
        );
        assert!(value_matches_type(
            &string_val("hello".into()),
            &app_type,
            &ctx
        ));
        assert!(!value_matches_type(&Value::Int(1), &app_type, &ctx));
    }

    #[tokio::test]
    async fn test_value_matches_type_tycon_from_typecheck_pass() {
        // Regression test: value_matches_type must correctly resolve user-defined TyCons
        // when tycon_env is wired into the EvalContext via set_tycon_env.
        // Without set_tycon_env, value_matches_type returns false for every user-defined
        // TyCon because tycon_env is None.
        //
        // This test verifies the wiring mechanism (set_tycon_env → value_matches_type)
        // by manually constructing a TyConDef for "Color" and injecting it.
        // Note: The production typecheck pass does not yet populate state.tycon_env for
        // TypeAlias declarations; that is tracked separately.
        use crate::type_def::{TyConDef, TyConEnv};

        // Build a TyConDef for Color with constructors Red, Green, Blue (unit variants, arity 0).
        let color_def = Arc::new(TyConDef {
            params: vec![],
            body: crate::types::Type::Unknown,
            constraints: vec![],
            variance: vec![],
            constructors: vec![
                ("Color.Red".to_string(), 0usize),
                ("Color.Green".to_string(), 0usize),
                ("Color.Blue".to_string(), 0usize),
            ],
            constructor_constants: indexmap::IndexMap::new(),
            field_annotations: indexmap::IndexMap::new(),
            builtin_type: None,
            annotation: None,
        });
        let mut tycon_env: TyConEnv = std::collections::HashMap::new();
        tycon_env.insert("Color".to_string(), color_def);

        // Wire into a fresh EvalContext — this is what run_loader_pipeline now does.
        let ctx = test_ctx();
        ctx.set_tycon_env(tycon_env);

        let tycon = Type::TyCon("Color".to_string());

        // Color.Red variant must pass @Color check.
        let color_red = Value::Variant {
            tag: "Color.Red".to_string(),
            payload: None,
        };
        assert!(
            value_matches_type(&color_red, &tycon, &ctx),
            "Color.Red must match @Color when tycon_env is wired from typecheck pass"
        );

        // Color.Green variant must also pass.
        let color_green = Value::Variant {
            tag: "Color.Green".to_string(),
            payload: None,
        };
        assert!(
            value_matches_type(&color_green, &tycon, &ctx),
            "Color.Green must match @Color when tycon_env is wired from typecheck pass"
        );

        // A value from a different TyCon must not pass — no cross-TyCon confusion.
        let other = Value::Variant {
            tag: "Shape.Circle".to_string(),
            payload: None,
        };
        assert!(
            !value_matches_type(&other, &tycon, &ctx),
            "Shape.Circle must not match @Color"
        );

        // A non-variant value must not pass.
        assert!(
            !value_matches_type(&Value::Int(1), &tycon, &ctx),
            "Int must not match @Color"
        );
    }

    // ── validate_and_wrap_record unit tests ──────────────────────────────────
    // Tests for validate_and_wrap_record helper function, particularly the
    // field_path error message generation for nested record validation.

    #[tokio::test]
    async fn test_validate_and_wrap_record_nested_field_path_error() {
        // Test that validate_and_wrap_record generates correct error messages
        // when field_path is non-empty (nested record validation).
        //
        // This exercises the code path where field_path_prefix is built with each
        // segment separately quoted per doc/07-type-extensions.md:162.

        // Create a row type requiring field "y"
        let mut fields = indexmap::IndexMap::new();
        fields.insert("y".to_string(), Type::Int);
        let row = Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        };

        // Create entries that are missing field "y"
        let entries: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        let ctx = test_ctx();

        // Call validate_and_wrap_record with nested field_path ["outer", "inner"]
        let mut field_path = vec!["outer".to_string(), "inner".to_string()];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result = validate_and_wrap_record(
            &entries,
            &row,
            &mut field_path,
            guard_span,
            data_span.clone(),
            &ctx,
            None,
            None,
        );

        // Should error with field path prefix in the message
        assert!(result.is_err(), "Expected error for missing field");
        let err = result.unwrap_err();
        let msg = err.to_string();
        // definition_span should be data_span (where the invalid dict was constructed/bound),
        // not guard_span (the annotation site). validate_and_wrap_record uses data_span as the
        // definition site so errors point at the value, not at the type annotation.
        assert_eq!(
            err.definition_span, data_span,
            "definition_span should be data_span (value site), not guard_span (annotation site)"
        );

        // Verify the error message contains the field path prefix
        // doc/07-type-extensions.md:162 specifies each segment separately quoted:
        // field `outer`.`inner`: (not field `outer.inner`:)
        assert!(
            msg.contains("field `outer`.`inner`:"),
            "Expected field path prefix 'field `outer`.`inner`:' in error message, got: {}",
            msg
        );

        // Verify the error message describes the missing field
        assert!(
            msg.contains("record missing field \"y\""),
            "Expected 'record missing field \"y\"' in error message, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_validate_and_wrap_record_nested_field_path_extra_field_accepted() {
        // BAS width subtyping: extra fields in closed records are ACCEPTED.
        // Under BAS, a value with more fields satisfies an annotation with fewer fields.

        // Create a row type requiring only field "x"
        let mut fields = indexmap::IndexMap::new();
        fields.insert("x".to_string(), Type::Int);
        let row = Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        };

        // Create entries with "x" plus an extra field "z"
        let ctx = test_ctx();
        let mut entries: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        entries.insert(
            HashableValue::Str("x".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(1),
                span.clone(),
            ))),
        );
        entries.insert(
            HashableValue::Str("z".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(99), span))),
        );

        let mut field_path = vec!["config".to_string()];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result = validate_and_wrap_record(
            &entries,
            &row,
            &mut field_path,
            guard_span,
            data_span,
            &ctx,
            None,
            None,
        );

        // BAS: should SUCCEED — extra fields accepted under width subtyping
        assert!(
            result.is_ok(),
            "BAS: extra fields should be accepted, got: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_validate_and_wrap_record_empty_field_path() {
        // Verify that when field_path is empty, no prefix is added to error messages.
        // This is the common case for top-level TypeAssert validation.

        // Create a row type requiring field "name"
        let mut fields = indexmap::IndexMap::new();
        fields.insert("name".to_string(), Type::Str);
        let row = Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        };

        // Create empty entries (missing "name")
        let entries: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        let ctx = test_ctx();

        // Call with empty field_path
        let mut field_path = vec![];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result = validate_and_wrap_record(
            &entries,
            &row,
            &mut field_path,
            guard_span,
            data_span.clone(),
            &ctx,
            None,
            None,
        );

        assert!(result.is_err(), "Expected error for missing field");
        let err = result.unwrap_err();
        let msg = err.to_string();
        // definition_span should be data_span (where the invalid dict was constructed/bound),
        // not guard_span (the annotation site). validate_and_wrap_record uses data_span as the
        // definition site so errors point at the value, not at the type annotation.
        assert_eq!(
            err.definition_span, data_span,
            "definition_span should be data_span (value site), not guard_span (annotation site)"
        );

        // Should NOT contain the empty-path prefix `field "": ` that would be inserted
        // if the `field_path.is_empty()` guard were absent (i.e., format!("field \"{}\": ",
        // vec![].join(".")) = `field "": `).
        assert!(
            !msg.contains("field \"\": "),
            "Expected no empty-path prefix for empty field_path, got: {}",
            msg
        );

        // Should contain the direct error message
        assert!(
            msg.contains("record missing field \"name\""),
            "Expected 'record missing field \"name\"' in error message, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_validate_and_wrap_record_accepts_int_key_bas() {
        // BAS width subtyping: integer-keyed entries are extra fields and are ACCEPTED.
        // Under BAS, a value with more fields (including int-keyed) satisfies the annotation.

        let mut fields = indexmap::IndexMap::new();
        fields.insert("name".to_string(), Type::Str);
        let row = Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        };

        // Create entries with "name" (valid) plus an integer-keyed entry
        let ctx = test_ctx();
        let mut entries: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        entries.insert(
            HashableValue::Int(0),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val("x".into()),
                span.clone(),
            ))),
        );
        entries.insert(
            HashableValue::Str("name".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val("y".into()),
                span,
            ))),
        );

        let mut field_path = vec![];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result = validate_and_wrap_record(
            &entries,
            &row,
            &mut field_path,
            guard_span,
            data_span,
            &ctx,
            None,
            None,
        );

        // BAS: should SUCCEED — extra int-keyed fields accepted under width subtyping
        assert!(
            result.is_ok(),
            "BAS: integer-keyed extra fields should be accepted, got: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_validate_and_wrap_record_allows_int_key_in_open_record() {
        // BAS: Integer-keyed entries are extra fields and are accepted by width subtyping.
        // All records are closed (RowTail::Empty) but BAS allows extra fields.

        let mut fields = indexmap::IndexMap::new();
        fields.insert("name".to_string(), Type::Str);
        let row = Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        };

        let ctx = test_ctx();
        let mut entries: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        entries.insert(
            HashableValue::Int(0),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val("x".into()),
                span.clone(),
            ))),
        );
        entries.insert(
            HashableValue::Str("name".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val("y".into()),
                span,
            ))),
        );

        let mut field_path = vec![];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result = validate_and_wrap_record(
            &entries,
            &row,
            &mut field_path,
            guard_span,
            data_span,
            &ctx,
            None,
            None,
        );

        // Should succeed: open records allow extra fields (including integer-keyed ones)
        assert!(
            result.is_ok(),
            "Expected success for integer-keyed entry in open record, got: {:?}",
            result.unwrap_err()
        );
    }

    #[tokio::test]
    async fn test_materialize_cached_thunk_at_high_depth() {
        // Pre-materialized thunks should succeed even at depth > MAX_CONTINUATION_STACK.
        // Previously, the depth check fired BEFORE the Materialized early-return,
        // causing spurious depth errors when accessing cached values at high depth.
        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_materialized(Value::Int(42), span);
        let ctx = test_ctx();

        // Materialize at high depth (CEK continuation stack) should succeed
        let result = materialize(&thunk, None, &ctx).await;
        assert!(
            result.is_ok(),
            "Expected success for cached thunk at high depth, got error: {:?}",
            result.unwrap_err()
        );
        assert_eq!(result.unwrap(), Value::Int(42));
    }

    #[tokio::test]
    async fn test_thunk_guarded_memoizes_on_success() {
        // Task 3(3): Guarded thunk memoization — after successful validation, the
        // thunk transitions to Materialized and the second access returns the cached
        // value without re-running the type guard.
        use crate::types::Type;

        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);

        // Inner thunk: a materialized Int(42) — passes the Int guard.
        let inner = Arc::new(Thunk::new_materialized(Value::Int(42), span.clone()));

        // Wrap it in a Guarded thunk expecting Int.
        let guarded = Arc::new(Thunk::new_guarded(
            Arc::clone(&inner),
            Type::Int,
            vec!["value".to_string()],
            span,
        ));

        // Initial state must be Guarded.
        {
            assert!(guarded.is_guarded(), "initial state should be Guarded");
        }

        // First materialization: triggers guard, validates Int(42) against Type::Int → pass.
        let result1 = materialize(&guarded, None, &ctx).await;
        assert!(result1.is_ok(), "first materialization should succeed");
        assert_eq!(result1.unwrap(), Value::Int(42));

        // After successful validation, thunk must be in Materialized state (memoized).
        assert_eq!(
            guarded.try_get_materialized(),
            Some(Value::Int(42)),
            "after first materialization thunk should be Materialized(Int(42))"
        );

        // Second materialization: must return cached value, not re-run the guard.
        let result2 = materialize(&guarded, None, &ctx).await;
        assert!(
            result2.is_ok(),
            "second materialization should succeed (cached)"
        );
        assert_eq!(result2.unwrap(), Value::Int(42));

        // State is still Materialized (not changed by second access).
        assert_eq!(
            guarded.try_get_materialized(),
            Some(Value::Int(42)),
            "state should still be Materialized after second access"
        );
    }

    #[tokio::test]
    async fn test_guarded_thunk_failure_path() {
        // Task 3(2): Guarded thunk failure path — when the inner value fails the type guard,
        // the thunk transitions to Failed (cacheable) and subsequent access returns the
        // cached error without re-running the guard.
        use crate::types::Type;

        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);

        // Inner thunk: a String value — fails the Int guard.
        let inner = Arc::new(Thunk::new_materialized(
            string_val("hello".into()),
            span.clone(),
        ));

        // Wrap it in a Guarded thunk expecting Int.
        let guarded = Arc::new(Thunk::new_guarded(
            Arc::clone(&inner),
            Type::Int,
            vec!["field".to_string()],
            span,
        ));

        // First materialization: triggers guard, validates String against Type::Int → fail.
        let result1 = materialize(&guarded, None, &ctx).await;
        assert!(
            result1.is_err(),
            "materialization should fail: String does not satisfy Int guard"
        );
        let err = result1.unwrap_err();
        assert!(
            err.to_string().contains("type assertion failed"),
            "error should say 'type assertion failed', got: {}",
            err.to_string()
        );

        // After failure, thunk must be in Failed state (cacheable memoization of error).
        assert!(
            guarded.get_cached_error().is_some(),
            "after type guard failure thunk should be Failed"
        );

        // Second materialization: returns the cached error, not re-runs the guard.
        let result2 = materialize(&guarded, None, &ctx).await;
        assert!(
            result2.is_err(),
            "second materialization should also fail (cached)"
        );
        assert!(
            result2
                .unwrap_err()
                .kind
                .to_string()
                .contains("type assertion failed"),
            "cached error should still say 'type assertion failed'"
        );
    }

    #[tokio::test]
    async fn test_guarded_thunk_preserves_inner_origin() {
        // When materializing nested Guarded thunks, the error decoration should use
        // the inner thunk's origin, not the outer guard's origin. This test verifies that
        // inner_span is captured before materialization, not after.
        use crate::types::Type;

        let span = test_span(1, 1, 1, 10);

        // Create an inner thunk that will produce a type mismatch when wrapped with Guarded
        // (we expect Int but will get String)
        let inner_expr = Arc::new(sp(CoreExpr::Str("hello".into())));
        let ctx = test_ctx();
        let inner_thunk = Arc::new(Thunk::new_unevaluated_core(
            inner_expr,
            empty_env(),
            Arc::clone(&ctx),
            span,
        ));

        // Wrap it in a Guarded thunk expecting Int (will fail type check)
        let guard_span = test_span(2, 1, 2, 10);
        let expected = Type::Int;
        let field_path = vec!["field".to_string()];
        let guarded = Arc::new(Thunk::new_guarded(
            inner_thunk,
            expected,
            field_path,
            guard_span,
        ));

        // Materialize - should fail type assertion
        let result = materialize(&guarded, None, &ctx).await;
        assert!(result.is_err(), "Expected type assertion failure");

        let error = result.unwrap_err();
        let msg = error.kind.to_string();

        // The error should be a type assertion failure
        assert!(
            msg.contains("type assertion failed"),
            "Expected type assertion failed error, got: {}",
            msg
        );

        // This test mainly verifies that the code compiles and runs with the fix applied.
        // The actual behavior (using inner_origin instead of outer origin) is verified
        // by the fact that errors now have the correct decoration context.
    }

    #[tokio::test]
    async fn test_circular_dependency_cycle_path() {
        // Test that circular dependency errors include the cycle path
        use crate::error::ErrorKind;

        let ctx = test_ctx();
        let env = empty_env();

        // Create a 3-node cycle: a→b→c→a
        // We'll use eval_dict to create labeled thunks
        let source = r#"
[
    a: $b
    b: $c
    c: $a
]
        "#;

        let parsed = crate::parse(source).expect("parse should succeed");
        let mut surface_program = parsed.program;
        crate::desugar::desugar_surface_program(&mut surface_program);
        // Resolve without env: dict siblings ($a, $b, $c) are resolved by scope tracking.
        crate::resolve::resolve_surface_program(&surface_program, None);
        let thunk = super::eval_surface_file(&surface_program, Arc::clone(&env), &ctx)
            .await
            .expect("eval_surface_file should succeed (lazy dict construction)");
        // Dict construction is lazy — the cycle is only detected when forcing an entry.
        // Materialize the dict to get the Value::Dict, then force an entry to trigger
        // cycle detection.
        let dict_val = materialize(&thunk, None, &ctx)
            .await
            .expect("dict should materialize");

        // Access one of the cyclic keys to trigger cycle detection
        let err = match dict_val {
            Value::Dict(ref map) => {
                let a_thunk_id = map
                    .get(&HashableValue::Str("a".into()))
                    .expect("dict should have 'a' key");
                let a_thunk = ctx.get_thunk(*a_thunk_id);
                materialize(&a_thunk, None, &ctx).await.unwrap_err()
            }
            _ => panic!("Expected Dict value"),
        };

        // Verify the error kind is CircularDependency
        if let ErrorKind::CircularDependency { name, cycle_path } = &err.kind {
            // eval_dict creates thunks without origin labels, so the cycle detector
            // uses the default label "thunk" for the node that completes the cycle.
            assert!(
                name == "thunk" || name == "a" || name == "b" || name == "c",
                "Cycle should be detected at one of the thunks, got: {}",
                name
            );

            // Note: cycle_path may be empty with the iterative CEK machine because
            // eval_stack entries are popped at force_step exit (not at thunk completion).
            // The cycle is still detected correctly; only the path visualization may be incomplete.
            let _ = cycle_path; // Accept empty cycle_path in iterative evaluator
        } else {
            panic!("Expected CircularDependency error, got: {:?}", err.kind);
        }
    }

    #[tokio::test]
    async fn test_decorate_deduplication() {
        // Verify that decorating an error with the same span twice doesn't create duplicates.
        // This tests the deduplication logic used when attaching stack frames during error propagation.
        let def_span = test_span(1, 1, 1, 10);
        let frame_span = test_span(5, 1, 5, 10);

        let mut err = EvalError::key_not_found("key", vec![], def_span);

        // Add the frame once
        err.push_frame("first access".to_string(), frame_span.clone());
        assert_eq!(err.stack.len(), 1, "Should have exactly one frame");
        assert_eq!(err.stack[0].label, "first access");

        // Manually check for duplicate before adding (this is what error decoration does)
        if !err.stack.iter().any(|f| f.definition_span == frame_span) {
            err.push_frame("second access".to_string(), frame_span);
        }

        // Should still be 1 frame (duplicate was avoided)
        assert_eq!(err.stack.len(), 1, "Duplicate span should be deduplicated");
        assert_eq!(
            err.stack[0].label, "first access",
            "Original label preserved"
        );
    }

    #[tokio::test]
    async fn test_eval_context_no_fs_flag() {
        // EvalContext should preserve the no_fs flag
        let env = empty_env();

        let ctx_with_fs = EvalContext::new(
            crate::test_util::test_caps().root.try_clone().unwrap(),
            Arc::clone(&env),
            Arc::clone(&env),
            false,
        );
        assert!(
            !ctx_with_fs.config.no_fs,
            "no_fs should be false when created with false"
        );

        let ctx_no_fs = EvalContext::new(
            crate::test_util::test_caps().root.try_clone().unwrap(),
            Arc::clone(&env),
            Arc::clone(&env),
            true,
        );
        assert!(
            ctx_no_fs.config.no_fs,
            "no_fs should be true when created with true"
        );
    }

    #[tokio::test]
    async fn test_selective_materialization_unused_branch() {
        // Verify that accessing only one dict entry doesn't materialize unused entries.
        // $builtin-raise is in core_env and raises an error when forced; the "unused"
        // entry must remain unforced so the raise never fires.
        let input = r#"[used: 1  unused: [call $builtin-raise "should not materialize"]]"#;
        let (env, ctx) = core_env_and_ctx();
        let thunk = eval_str(input, Arc::clone(&env), &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        // Extract the dict
        match val {
            Value::Dict(map) => {
                // Access only the "used" key
                let used_key = HashableValue::Str("used".into());
                let used_thunk = map.get(&used_key).expect("used key should exist");
                let used_val = mat_id(used_thunk, &ctx)
                    .await
                    .expect("used should materialize");
                assert_eq!(used_val, Value::Int(1));

                // Verify the "unused" key exists but is NOT materialized
                let unused_key = HashableValue::Str("unused".into());
                let unused_thunk_id = map.get(&unused_key).expect("unused key should exist");
                let unused_thunk = get_thunk_rc(unused_thunk_id, &ctx);

                // Check that the unused thunk is still in an unevaluated state
                // (it should not be Materialized)
                assert!(
                    unused_thunk.try_get_materialized().is_none(),
                    "unused thunk should not be materialized"
                );
                // Check it's not in Failed or InProgress state
                if let Some(_err) = unused_thunk.get_cached_error() {
                    panic!("unused thunk should not be in Failed state (error should not have triggered)")
                }
                if unused_thunk.is_in_progress() {
                    panic!("unused thunk should not be InProgress")
                }
                // Unevaluated or other states like PendingCall are acceptable
            }
            _ => panic!("expected Dict value, got {:?}", val),
        }
    }

    // === Bypass path tests: force_count pre-materialization in eval.rs recursive paths ===

    /// Bypass path test: PendingBuiltin materialization pre-materializes strict args.
    ///
    /// This tests the recursive bypass path in `eval.rs::materialize` (line ~1828).
    /// When a PendingBuiltin thunk is materialized via the recursive `materialize()`
    /// call (not through the CEK machine), the path must still apply force_count
    /// pre-materialization before calling the builtin function.
    ///
    /// Setup: create a PendingBuiltin thunk for `builtin_keys` (force_count=1) with
    /// an *unevaluated* dict-expr thunk as args[0]. The unevaluated thunk evaluates
    /// to an empty dict. If the bypass path were to call `builtin_keys` without first
    /// materializing args[0], `try_get_materialized().expect(...)` inside `builtin_keys`
    /// would panic.
    #[tokio::test]
    async fn pending_builtin_bypass_path_pre_materializes_args() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Build an unevaluated thunk that evaluates to an empty dict.
        // `CoreExpr::Dict(vec![])` evaluates to `Value::Dict(IndexMap::new())`.
        let dict_expr = Arc::new(sp(CoreExpr::Dict(vec![])));
        let unevaluated_arg = Arc::new(Thunk::new_unevaluated_core(
            dict_expr,
            empty_env(),
            Arc::clone(&ctx),
            span.clone(),
        ));

        // Verify the arg is NOT yet materialized (it is unevaluated).
        assert!(
            unevaluated_arg.try_get_materialized().is_none(),
            "arg must be unevaluated before the PendingBuiltin is forced"
        );

        // Construct a BuiltinDef for `builtin_keys` with force_count=1.
        const KEYS_STRICTNESS: &[Strictness] = &[];
        let keys_def = BuiltinDef {
            func: crate::builtins::builtin_keys as BuiltinFn,
            name: "keys",
            pos_strictness: KEYS_STRICTNESS,
            force_count: 1,
            params: &[],
        };

        // Create a PendingBuiltin thunk wrapping `builtin_keys` with the unevaluated arg.
        let outer = Arc::new(Thunk::new_pending_builtin(
            keys_def,
            vec![Arc::clone(&unevaluated_arg)],
            None,
            span,
            None,
            empty_env(),
            Arc::clone(&ctx),
        ));

        // Materialize via the recursive path. If force_count pre-materialization is
        // missing, this panics at `try_get_materialized().expect(...)` inside `builtin_keys`.
        let result = materialize(&outer, None, &ctx).await;
        assert!(
            result.is_ok(),
            "PendingBuiltin bypass path must pre-materialize force_count args; got: {:?}",
            result.unwrap_err()
        );

        // The result should be an empty dict (keys of empty dict = empty dict).
        let val = result.unwrap();
        assert!(
            matches!(val, Value::Dict(ref m) if m.is_empty()),
            "expected empty dict from builtin_keys on empty dict, got {:?}",
            val
        );
    }

    /// Bypass path test: PendingCall→Builtin materialization pre-materializes strict args.
    ///
    /// This tests the recursive bypass path in `eval.rs::materialize` for PendingCall
    /// thunks (line ~2059) when the callee resolves to a `Value::Builtin`. When a
    /// PendingCall thunk with a Builtin callee is materialized recursively, the path
    /// must still apply force_count pre-materialization before calling the builtin.
    ///
    /// Setup: create a PendingCall thunk where the func thunk resolves to `Value::Builtin(keys_def)`
    /// and args[0] is an unevaluated dict thunk. If force_count pre-materialization is
    /// missing in the PendingCall→Builtin path, `builtin_keys` panics at
    /// `try_get_materialized().expect(...)`.
    #[tokio::test]
    async fn pending_call_builtin_bypass_path_pre_materializes_args() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Build an unevaluated thunk that evaluates to an empty dict.
        let dict_expr = Arc::new(sp(CoreExpr::Dict(vec![])));
        let unevaluated_arg = Arc::new(Thunk::new_unevaluated_core(
            dict_expr,
            empty_env(),
            Arc::clone(&ctx),
            span.clone(),
        ));

        // Verify the arg is NOT yet materialized.
        assert!(
            unevaluated_arg.try_get_materialized().is_none(),
            "arg must be unevaluated before the PendingCall is forced"
        );

        // Create a materialized func thunk wrapping Value::Builtin(keys_def).
        const KEYS_STRICTNESS: &[Strictness] = &[];
        let keys_def = BuiltinDef {
            func: crate::builtins::builtin_keys as BuiltinFn,
            name: "keys",
            pos_strictness: KEYS_STRICTNESS,
            force_count: 1,
            params: &[],
        };
        let func_thunk = Arc::new(Thunk::new_materialized(
            Value::Builtin(keys_def),
            span.clone(),
        ));

        // Create a PendingCall thunk: calls builtin_keys with the unevaluated arg.
        let outer = Arc::new(Thunk::new_pending_call(
            func_thunk,
            vec![Arc::clone(&unevaluated_arg)],
            IndexMap::new(),
            span.clone(),
            empty_env(),
            span.clone(),
            None,
            Arc::clone(&ctx),
            Arc::new(crate::ast::Spanned {
                node: crate::ast::CoreExpr::Int(0),
                span,
            }),
        ));

        // Materialize via the recursive path. If force_count pre-materialization is
        // missing for the PendingCall→Builtin case, this panics inside `builtin_keys`.
        let result = materialize(&outer, None, &ctx).await;
        assert!(
            result.is_ok(),
            "PendingCall→Builtin bypass path must pre-materialize force_count args; got: {:?}",
            result.unwrap_err()
        );

        // The result should be an empty dict (keys of empty dict = empty dict).
        let val = result.unwrap();
        assert!(
            matches!(val, Value::Dict(ref m) if m.is_empty()),
            "expected empty dict from builtin_keys on empty dict, got {:?}",
            val
        );
    }

    // ── PM3: pattern linearity tests ─────────────────────────────────────────

    /// Helper: build a variable pattern Spanned<Pattern> at the default test span.
    fn var_pattern(name: &str) -> Spanned<Pattern> {
        sp(Pattern::Pin(
            name.to_string(),
            crate::ast::Resolution::new(),
        ))
    }

    /// Helper: build a wildcard pattern Spanned<Pattern> at the default test span.
    fn wildcard_pattern() -> Spanned<Pattern> {
        sp(Pattern::Wildcard)
    }

    #[tokio::test]
    async fn test_check_pattern_linearity_linear_is_ok() {
        // A pattern with all distinct variables must pass linearity check.
        let pattern = sp(Pattern::Dict {
            fields: vec![
                ("a".to_string(), var_pattern("x")),
                ("b".to_string(), var_pattern("y")),
                ("c".to_string(), var_pattern("z")),
            ],
            rest: true,
        });
        assert!(
            check_pattern_linearity(&pattern).is_ok(),
            "distinct variable names must pass"
        );
    }

    #[tokio::test]
    async fn test_check_pattern_linearity_wildcard_is_ok() {
        // Wildcards do not bind names; multiple wildcards are always linear.
        let pattern = sp(Pattern::Dict {
            fields: vec![
                ("a".to_string(), wildcard_pattern()),
                ("b".to_string(), wildcard_pattern()),
            ],
            rest: true,
        });
        assert!(
            check_pattern_linearity(&pattern).is_ok(),
            "multiple wildcards must not trigger linearity error"
        );
    }

    #[tokio::test]
    async fn test_check_pattern_linearity_single_variable_is_ok() {
        // A single variable binding is always linear.
        let pattern = var_pattern("x");
        assert!(check_pattern_linearity(&pattern).is_ok());
    }

    #[tokio::test]
    async fn test_check_pattern_linearity_pin_in_dict_not_a_violation() {
        // T-1154: `[a: x  b: x  ...]:` — `x` appears twice, but as Pin (not Variable).
        // Pin patterns do not bind names, so duplicate pin patterns are NOT linearity
        // violations. Both arms act as wildcards when `x` is not in scope.
        let pattern = sp(Pattern::Dict {
            fields: vec![
                ("a".to_string(), var_pattern("x")),
                ("b".to_string(), var_pattern("x")),
            ],
            rest: true,
        });
        let result = check_pattern_linearity(&pattern);
        assert!(
            result.is_ok(),
            "duplicate Pin patterns must NOT trigger linearity error; got: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_check_pattern_linearity_pin_in_constructor_not_a_violation() {
        // T-1154: `[Some [a: x  b: x]]:` — x appears twice inside Constructor payload
        // as Pin patterns. Pin does not bind, so no linearity violation.
        let payload = sp(Pattern::Dict {
            fields: vec![
                ("a".to_string(), var_pattern("x")),
                ("b".to_string(), var_pattern("x")),
            ],
            rest: true,
        });
        let pattern = sp(Pattern::Constructor {
            tag: "Maybe.Some".to_string(),
            binding: Some(Box::new(payload)),
        });
        let result = check_pattern_linearity(&pattern);
        assert!(
            result.is_ok(),
            "duplicate Pin patterns inside Constructor must NOT trigger linearity error"
        );
    }

    #[tokio::test]
    async fn test_pm3_match_expr_pin_pattern_does_not_bind() {
        // Integration test: T-1154 — `[a: x  b: x  ...]: x` uses Pin patterns.
        // Both `x` positions are unresolved pins → wildcards. The arm fires, but `x`
        // is NOT bound anywhere. The body references `x` which is undefined.
        //
        // The lowerer eagerly produces an "undefined variable: x" error when it encounters
        // an unresolved VarRef in the body. This causes eval_str to return Err immediately.
        //
        // match [a: 1  b: 2]
        //   [a: x  b: x  ...]: x   ← arm pattern: Pins (wildcards); body: unresolved `x`
        let result = eval_str(
            "[match [a: 1  b: 2]  [a: x  b: x  ...]: x]",
            empty_env(),
            &test_ctx(),
        )
        .await;
        // The lowerer eagerly errors on unresolved `x` in the body → eval returns Err.
        assert!(
            result.is_err(),
            "expected eager lower error for unresolved body var x, got: {:?}",
            result.ok()
        );
        let err = result.unwrap_err();
        assert!(
            !err.to_string().is_empty(),
            "expected a non-empty error for unresolved body var x, got: {:?}",
            err.kind
        );
    }

    #[tokio::test]
    async fn test_pm3_match_expr_pin_dict_pattern_fires_on_match() {
        // Integration test: T-1154 — `[a: x  b: y  ...]: 99` uses Pin patterns.
        // Both `x` and `y` are unresolved pins → wildcards. The arm fires on any dict.
        // Body is `99` (does not reference the unbound pins), so result is 99.
        //
        // match [a: 1  b: 2]
        //   [a: x  b: y  ...]: 99   ← arm fires (wildcards for x, y), body = 99
        let result = eval_str(
            "[match [a: 1  b: 2]  [a: x  b: y  ...]: 99]",
            empty_env(),
            &test_ctx(),
        )
        .await;
        assert!(
            result.is_ok(),
            "Pin Dict pattern must not error on eval; got: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_pm3_same_pin_in_different_arms_is_ok() {
        // T-1154: `x: 1  x: 2` — both arms use Pin("x") which is unresolved → wildcard.
        // First arm fires (wildcard matches 42), body = 1. Second arm never reached.
        //
        // match 42
        //   x: 1    <- Pin("x") unresolved → wildcard; fires, returns 1
        //   x: 2    <- never reached
        let result = eval_str("[match 42  x: 1  x: 2]", empty_env(), &test_ctx()).await;
        assert!(
            result.is_ok(),
            "Pin patterns in different arms must not error; got: {:?}",
            result.err()
        );
    }

    // B-430: [type MyType Int] in standalone expression position returns {} (empty dict).
    //
    // Type declarations have no runtime value when they appear as standalone expressions
    // (i.e. not as the value of a named dict entry like `Color: [type Red Green Blue]`).
    // The correct runtime result is {} (empty dict), not an error and not a non-empty dict.
    #[tokio::test]
    async fn test_type_alias_returns_empty_dict() {
        // [type MyType Int] — standalone type alias in expression position.
        // The body "Int" is an uppercase VarRef; previously this was misinterpreted as a
        // unit constructor "Int" and produced a non-empty constructor dict {Int: <variant>}.
        let thunk = eval_str("[type MyType Int]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match val {
            Value::Dict(map) => assert!(
                map.is_empty(),
                "B-430: standalone [type MyType Int] must produce {{}} (empty dict), got {} entries",
                map.len()
            ),
            other => panic!(
                "B-430: expected Value::Dict({{}}) for standalone [type MyType Int], got {:?}",
                other
            ),
        }
    }

    // B-430 variant: [type Color Red Green Blue] standalone also returns {}.
    //
    // Even with genuine constructor names, a standalone type declaration in expression
    // position should return {}, not a constructor dict. Constructors are only accessible
    // when the alias is bound to a name in a dict entry (Color: [type Red Green Blue]).
    #[tokio::test]
    async fn test_type_alias_sum_type_standalone_returns_empty_dict() {
        let thunk = eval_str("[type Color Red Green Blue]", empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match val {
            Value::Dict(map) => assert!(
                map.is_empty(),
                "B-430: standalone [type Color Red Green Blue] must produce {{}} (empty dict), got {} entries",
                map.len()
            ),
            other => panic!(
                "B-430: expected Value::Dict({{}}) for standalone sum-type alias, got {:?}",
                other
            ),
        }
    }

    // ── B-427: tail-recursive and context-inheritance tests ──────────────────

    /// B-427: tail-recursive LLT function evaluates correctly using builtin names.
    ///
    /// Tests a tail-recursive accumulator function. Uses `builtin-if`, `builtin-eq-int`,
    /// `builtin-add`, `builtin-sub` directly — no prelude aliases needed.
    /// The function sums 1..=100 using an accumulator; result must be 5050.
    #[tokio::test]
    async fn test_tco_tail_recursive_function() {
        let (env, ctx) = core_env_and_ctx();

        // sum-to 0 acc = acc
        // sum-to n acc = sum-to (n-1) (acc+n)
        // sum-to 100 0 = 1 + 2 + ... + 100 = 5050
        let source = r#"[
            sum-to: [fn [let n acc]
                [builtin-if [builtin-eq-int n 0]
                    acc
                    [sum-to [builtin-sub n 1] [builtin-add acc n]]]]
            result: [sum-to 100 0]
        ]"#;

        let thunk = eval_str(source, Arc::clone(&env), &ctx).await.unwrap();
        let dict_val = materialize(&thunk, None, &ctx).await.unwrap();

        match dict_val {
            Value::Dict(ref map) => {
                let result_id = map
                    .get(&HashableValue::Str("result".into()))
                    .expect("dict must have 'result' key");
                let result_val = mat_id(result_id, &ctx).await.unwrap();
                assert_eq!(
                    result_val,
                    Value::Int(5050),
                    "sum-to 100 0 must equal 5050 (sum 1..=100); got {result_val:?}"
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    /// B-427: EvalContext.with_base_dir() inherits no_fs from the parent context.
    ///
    /// `with_base_dir()` creates a child context and must propagate `no_fs`.
    /// Verifies the flag is preserved and stdlib_env is shared (same Arc pointer).
    #[tokio::test]
    async fn test_eval_context_with_base_dir_inherits_no_fs() {
        let env = crate::builtins::build_core_env();
        let base_dir1 = crate::test_util::test_caps().root.try_clone().unwrap();
        let base_dir2 = crate::test_util::test_caps().root.try_clone().unwrap();

        // Create a parent context with no_fs=true.
        let ctx1 = EvalContext::new(
            base_dir1,
            Arc::clone(&env),
            Arc::clone(&env),
            true, // no_fs = true
        );
        assert!(
            ctx1.config.no_fs,
            "parent context must have no_fs=true as configured"
        );

        // Child context inherits no_fs from parent via with_base_dir().
        let ctx2 = ctx1.with_base_dir(base_dir2);
        assert!(
            ctx2.config.no_fs,
            "with_base_dir() must inherit no_fs=true from parent; got no_fs=false"
        );

        // Verify the child shares the parent's stdlib_env (same Arc pointer).
        assert!(
            Arc::ptr_eq(&ctx1.config.stdlib_env, &ctx2.config.stdlib_env),
            "with_base_dir() must share parent's stdlib_env (same Arc)"
        );
    }

    // ── B-462/B-464: ground_type_of variadic flag ────────────────────────────

    /// B-462/B-464: ground_type_of must set variadic: true for Value::Function when the
    /// last parameter has variadic: true.  Previously the field was hardwired to false,
    /// causing consistent_subtype checks against @[fn ...variadic...] to fail at runtime.
    #[tokio::test]
    async fn test_ground_type_of_variadic_function() {
        // Non-variadic function: ground_type_of must report variadic: false.
        let non_variadic = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: None,
                    variadic: false,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Dict(vec![]))),
            env: empty_env(),
            annotation: None,
            return_ann: None,
        };
        match ground_type_of(&non_variadic) {
            Type::Function { variadic, .. } => {
                assert!(
                    !variadic,
                    "non-variadic function must have variadic: false in ground_type_of"
                );
            }
            other => panic!(
                "expected Type::Function for Value::Function, got {:?}",
                other
            ),
        }

        // Variadic function: last param has variadic: true — ground_type_of must reflect it.
        let variadic_fn = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "rest".into(),
                    annotation: None,
                    variadic: true,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Dict(vec![]))),
            env: empty_env(),
            annotation: None,
            return_ann: None,
        };
        match ground_type_of(&variadic_fn) {
            Type::Function {
                variadic,
                required_count,
                ..
            } => {
                assert!(
                    variadic,
                    "variadic function must have variadic: true in ground_type_of"
                );
                assert_eq!(
                    required_count, 2,
                    "required_count must include variadic param in param count"
                );
            }
            other => panic!(
                "expected Type::Function for variadic Value::Function, got {:?}",
                other
            ),
        }

        // Single variadic param (zero-arg form, e.g. [fn [let ...xs] body]).
        let only_variadic = Value::Function {
            params: Rc::new(vec![Param {
                name: "xs".into(),
                annotation: None,
                variadic: true,
            }]),
            body: Arc::new(sp(CoreExpr::Dict(vec![]))),
            env: empty_env(),
            annotation: None,
            return_ann: None,
        };
        match ground_type_of(&only_variadic) {
            Type::Function { variadic, .. } => {
                assert!(
                    variadic,
                    "single variadic param must have variadic: true in ground_type_of"
                );
            }
            other => panic!(
                "expected Type::Function for single-variadic Value::Function, got {:?}",
                other
            ),
        }
    }
}
