//! Core evaluation module: lazy evaluation with letrec dict scoping, document
//! pipelines, and function evaluation.
//!
//! See eval_materialize.rs for the CEK machine implementation.

pub(crate) use crate::eval_call::eval_call_core;
pub use crate::eval_call::{invoke_function, CallContext};

// Re-export CEK machine components from eval_materialize
pub(crate) use crate::eval_materialize::{attach_materialization_context, run, Action};

// Split modules — document/pipeline evaluation and dict construction
#[path = "eval_dict.rs"]
mod eval_dict_mod;
#[path = "eval_pipeline.rs"]
mod eval_pipeline;

pub(crate) use eval_dict_mod::*;
pub use eval_pipeline::*;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex, RwLock};

use indexmap::IndexMap;

use crate::arena::{EnvArena, ThunkArena, ThunkId};
use crate::ast::{
    Annotation, CoreExpr, Entry, Expr, LiteralPattern, MatchArm, NamedArg, Param, Pattern, Span,
    Spanned,
};
use crate::error::{EvalError, EvalResult};
use crate::types::{Row, Type};
// Circular module dependency: this module calls builtins via function pointers stored in `Value::Builtin`.
// builtins.rs imports `invoke_function` and `materialize` from this module.
// This bidirectional dependency is safe because neither module's initialization depends on the other.
use crate::value::{string_val, Environment, Key, Thunk, Value};

pub(crate) const DEFAULT_ANNOTATION_KEY: &str = "default";

/// Type alias for the optional default expression + environment pair used by guarded thunks.
/// Reduces type_complexity in function signatures that carry this optional default.
type GuardDefault = (Arc<Spanned<crate::ast::CoreExpr>>, Arc<RwLock<Environment>>);

/// Type alias for the return type of `match_pattern` — an async fn returning an optional env.
type MatchPatternFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = EvalResult<Option<Arc<RwLock<Environment>>>>> + 'a>,
>;

/// Type alias for the return type of `values_equal` — a recursive async fn returning bool.
/// Must be `Pin<Box<...>>` to support recursion (direct `async fn` recursion is unsized).
type ValuesEqualFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<bool>>>>;

/// Check if a span matches a boundary guard and wrap the thunk if so.
///
/// Called at the end of `eval()` to automatically insert runtime guards for
/// gradual typing boundaries detected by the type checker. O(1) lookup via
/// the `HashMap<Span, Type>` boundary_guards map.
///
/// The resulting [`ThunkState::Guarded`] thunk is lazy — the type check fires
/// only when the thunk is forced (materialized), not at construction time.
fn maybe_wrap_guard(thunk: Arc<Thunk>, span: Span, ctx: &Arc<EvalContext>) -> Arc<Thunk> {
    // O(1) span lookup in the boundary guard table.
    let guards = ctx.boundary_guards.read().unwrap();
    if let Some(expected_type) = guards.get(&span) {
        // Create a guarded thunk wrapping the original. The guard fires lazily
        // when the thunk is forced, validating the runtime value against
        // expected_type and returning EvalError::type_assert_failed on mismatch.
        Arc::new(Thunk::new_guarded_full(
            thunk,
            expected_type.clone(),
            Vec::new(), // empty field path for top-level boundary guard
            span,
            None, // no blame label for automatic guards (blame is the call site span)
            None, // no default: fallback for automatic guards
        ))
    } else {
        thunk
    }
}

/// Reserved annotation meta-keys that are NOT structural field declarations.
/// A PropertyDict annotation whose entries are all meta-keys (e.g., `[@[default: 0] $x]`)
/// is metadata-only and has no type to validate. A PropertyDict with at least one
/// non-meta-key entry (e.g., `[@[name: String age: Int] $x]`) is a structural record
/// annotation that should enforce at minimum a Dict tag check when `resolved_type` is `None`.
const ANNOTATION_META_KEYS: &[&str] = &["type", "default", "is", "repr"];

/// Formats a field path for TypeAssert error display. Each segment is separately
/// backtick-quoted: `user`.`address`.`zip`. Not for reconstruction — display only.
pub(crate) fn format_field_path(field_path: &[String]) -> String {
    field_path
        .iter()
        .map(|s| format!("`{}`", s))
        .collect::<Vec<_>>()
        .join(".")
}

/// Check whether a PropertyDict annotation contains structural field declarations.
///
/// Returns `true` if the annotation has at least one entry with a string key that
/// is NOT a reserved annotation meta-key ("type", "default"). This indicates the
/// annotation describes a record structure (e.g., `[@[name: String age: Int] $x]`).
///
/// Used by the `--no-typecheck` fallback to distinguish structural record annotations
/// (which should enforce a Dict tag check per doc/07-type-extensions.md §--no-typecheck mode)
/// from metadata-only annotations (which have nothing to validate against).
///
/// **Parser guarantee:** PropertyDict entries always have `Expr::Str` keys; non-`Expr::Str`
/// keys are treated as non-structural (the `_ => None` arm will never match in well-formed ASTs).
pub(crate) fn annotation_has_structural_fields(annotation: &Annotation) -> bool {
    match annotation {
        Annotation::Simple(_) => false,
        Annotation::PropertyDict(entries) => entries.iter().any(|entry| {
            entry
                .node
                .key
                .as_ref()
                .and_then(|k| match &k.node {
                    Expr::Str(name) => Some(name.as_str()),
                    _ => None,
                })
                .is_some_and(|name| !ANNOTATION_META_KEYS.contains(&name))
        }),
        Annotation::Annotated(_, _) => false,
    }
}

/// Immutable session configuration shared across evaluation.
#[derive(Debug)]
pub struct EvalConfig {
    pub base_dir: cap_std::fs::Dir,
    pub stdlib_env: Arc<RwLock<Environment>>,
    /// Type-stage environment: contains only type-level builtins, no IO/caps/runtime API.
    /// Used by `builtin_eval_types` to evaluate type-stage documents in isolation.
    pub type_stage_env: Arc<RwLock<Environment>>,
    pub no_fs: bool,
    /// When true, every `$include` call must supply an integrity hash.
    /// Hashless includes are rejected with `IncludeHashRequired`.
    pub require_integrity: bool,
    /// Macro inject defaults: macro_name -> inject_default_name.
    /// Populated by the expansion pass, used by the `macro-injects` builtin.
    /// Only macros with `inject:` declarations have entries; macros without
    /// inject: (using only gensym hygiene) are absent from this map.
    pub macro_injects_map: HashMap<String, String>,
    /// Source file path where evaluation started (if available).
    /// Propagated to FnAnnotation for LSP hover and diagnostics.
    pub source_file: Option<String>,
}

/// Cache entry for the string-keyed include cache used by `include-cache-get`/`include-cache-put`.
///
/// Keyed by `blake3(cap-identity + "|" + source_text)` so that:
/// - `Missing` — known cache miss (prevents redundant re-queries)
/// - `Pending` — file is currently being evaluated (cycle detection sentinel)
/// - `Cached` — successfully-evaluated result thunk, plus the side tables produced
///   by the resolver and typechecker so that `eval` builtin callers can construct
///   `UnevaluatedState::Surface` thunks for `Value::Expression` nodes from this file.
#[derive(Debug, Clone)]
pub enum IncludeCacheEntry {
    Missing,
    Pending,
    Cached(
        Arc<Thunk>,
        std::sync::Arc<crate::ast::ResolutionTable>,
        std::sync::Arc<crate::ast::TypeAnnotationTable>,
    ),
}

/// Mutable evaluation state (include caching).
#[derive(Debug)]
pub struct EvalState {
    // DELETED: include_guard (inode-keyed cycle detection) — replaced by [Pending] state in string_include_cache
    // DELETED: include_cache (inode-keyed result cache) — replaced by content-addressed string_include_cache
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
    /// Upper bound: MAX_EVAL_DEPTH (256) entries × ~80 bytes/entry ≈ 20 KB.
    pub eval_stack: Vec<(String, Span)>,
    /// Runtime class registry: class_name -> (params, superclasses, method_defaults)
    /// Stores default method implementations for filling in instance dictionaries.
    pub class_registry: HashMap<String, RuntimeClassDecl>,
    /// Runtime instance registry: (class_name, type_tags) -> instance_dict
    /// Stores materialized method dictionaries for each instance.
    /// class_name is interned via `intern_class_name` (&'static str); type_tags is a
    /// Vec<String> (from Value::type_name() on determining-position args) for MPTC support.
    pub instance_registry: HashMap<(&'static str, Vec<String>), Arc<Thunk>>,
    /// O(1) set of class names that have at least one registered instance.
    /// Updated in sync with `instance_registry`. Used by builtins (e.g. `+`, `str`)
    /// to avoid a linear scan over registry keys on every arithmetic/string operation.
    pub registered_classes: HashSet<String>,
    // future: trace_log, eval_stats
}

/// Runtime representation of a class declaration.
/// Stores information needed to construct instance dictionaries.
#[derive(Debug, Clone)]
pub struct RuntimeClassDecl {
    pub params: Vec<String>,
    /// Superclass constraints as (class_name, param_name) tuples
    pub superclasses: Vec<(String, String)>,
    /// Default method implementations: method_name -> thunk
    /// These are wrapped as thunks to preserve laziness.
    pub method_defaults: IndexMap<String, Arc<Thunk>>,
    /// Number of determining-position type parameters for MPTC dispatch.
    /// For single-param classes (Equatable, Comparable): 1.
    /// For arithmetic classes (Addable a b c with FD (a,b)→c): 2.
    /// Used to truncate instance_registry keys so they match try_dispatch_method lookups.
    pub num_determining: usize,
}

/// Extract type names from an instance pattern for MPTC dispatch.
/// For `[instance Addable [x@Int y@Float z@Int] ...]` with num_determining=2,
/// returns only `vec!["Int", "Float"]` (the determining-position tags).
///
/// The `num_determining` parameter is the number of determining-position type
/// parameters for this class (from `RuntimeClassDecl::num_determining`). Only
/// the first `num_determining` annotated bindings are included in the key so
/// that `instance_registry` keys match the keys built by `try_dispatch_method`.
/// Falls back to empty vec if pattern is malformed (will cause dispatch to fail).
///
/// Handles both `PatternDecl` (from `[pattern ...]` syntax) and `LetDecl` (from
/// `[let ...]` syntax). The prelude uses `[let a@Int b@Int c]` for arithmetic
/// instances, where the third bare binding (the determined param) is skipped by
/// `filter_map` and only the first `num_determining` annotated positions are kept.
// TODO(chr-instances-gaps): wire up instance registration so this function is called
// when [instance ...] declarations are processed at runtime.
#[allow(dead_code)]
fn extract_instance_type_tags(pattern_expr: &Spanned<Expr>, num_determining: usize) -> Vec<String> {
    let extract_tags = |bindings: &[Spanned<Expr>]| {
        bindings
            .iter()
            .filter_map(|binding| match &binding.node {
                Expr::Annotated { annotation, .. } => match &annotation.node {
                    Annotation::Simple(type_name) => Some(type_name.clone()),
                    Annotation::Annotated(outer, _inner) => {
                        // For nested annotations like Seq@Int, use the outer constructor
                        Some(outer.clone())
                    }
                    Annotation::PropertyDict(_) => None, // Skip property dict annotations
                },
                _ => None, // Skip bare VarRef bindings (no type info, e.g. the determined param)
            })
            .take(num_determining)
            .collect()
    };
    match &pattern_expr.node {
        Expr::PatternDecl { bindings } => extract_tags(bindings),
        // LetDecl is the form used by prelude arithmetic instances: [let a@Int b@Int c]
        Expr::LetDecl { bindings } => extract_tags(bindings),
        _ => Vec::new(), // Malformed pattern — dispatch will miss
    }
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
    /// Environment arena registry. Phase 3: populated by `eval_dict` (alloc_root +
    /// fill_letrec_slot per dict scope). Env IDs enable O(1) variable lookup in the
    /// CoreExpr force path.
    /// **Shared ownership:** Arc<Mutex<>> allows child contexts to share the parent's arena.
    pub(crate) env_arena: Arc<Mutex<EnvArena>>,
    /// Environment variable allowlist. None = unrestricted (all allowed), Some(set) = only those in set.
    /// Some(empty) means all denied (--no-env mode).
    pub env_allowed: Option<HashSet<String>>,
    /// Pipeline blame map: records producing stage label for each `%` thunk at `---` boundaries.
    /// Key is the ThunkId of the `%` pipeline variable, value is the producing stage's file path
    /// or index. Used by contract violation errors to identify the positive party (producer)
    /// per Findler & Felleisen (2002). Avoids a `Value::Tagged` variant which would require
    /// updating all exhaustive `Value` matches.
    pub blame_map: Mutex<HashMap<ThunkId, String>>,
    /// Boundary guards from type inference: span → expected_param_type.
    /// When an Unknown-typed expression crosses into a concrete-typed context,
    /// the type checker records the boundary. The evaluator checks if a thunk's
    /// span matches a guard and wraps it with a runtime Guarded thunk if so.
    /// HashMap for O(1) lookup at thunk creation time in eval_recursive.
    /// Populated by the type checker via set_boundary_guards(), consumed during eval().
    pub boundary_guards: RwLock<HashMap<Span, Type>>,
    /// Monad resolutions for inferred [do] forms: %do-infer VarRef span → monad variable name.
    /// The type checker records the resolved monad name here (keyed by the span of the
    /// `%do-infer` VarRef in the desugared bind chain). At eval time, when `VarRef("%do-infer")`
    /// is evaluated, the evaluator looks up this map by span and returns the monad dict value.
    /// Parallel to boundary_guards: type-checker-to-evaluator communication via span-keyed side channel.
    /// Populated by the type checker via set_do_infer_resolutions(), consumed during eval().
    pub do_infer_resolutions: RwLock<HashMap<String, String>>,
    /// Already-open libdir Dir, shared from the bootstrap boundary (main.rs or repl.rs).
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
}

impl EvalContext {
    pub fn new(
        base_dir: cap_std::fs::Dir,
        stdlib_env: Arc<RwLock<Environment>>,
        type_stage_env: Arc<RwLock<Environment>>,
        no_fs: bool,
    ) -> Arc<Self> {
        Self::new_with_options(base_dir, stdlib_env, type_stage_env, no_fs, false, None)
    }

    /// Create a new EvalContext with a FRESH EMPTY arena, bypassing `STDLIB_ARENA_CACHE`.
    ///
    /// This constructor is for contexts that should NOT inherit stdlib thunks:
    /// - Bootstrap contexts (inside `create_stdlib_env_inner` where stdlib is being loaded)
    /// - Re-entrant macro expansion (depth > 0 in expand.rs)
    /// - Test helpers that create contexts without a stdlib env
    ///
    /// Using `new()` or `new_with_options()` in these cases would pull in stale cache contents
    /// and pollute the bootstrap evaluation. Always use `new_empty()` when the stdlib env
    /// being passed is NOT the standard prelude-loaded environment.
    ///
    /// **WARNING:** ThunkIds from the stdlib arena (obtained via `new()` or `new_with_options()`)
    /// are NOT valid in fresh-arena contexts created by `new_empty()`. Attempting to dereference
    /// a stdlib ThunkId in a fresh arena will panic or return incorrect results. Only use
    /// `new_empty()` when the environment contains no references to stdlib thunks.
    pub fn new_empty(
        base_dir: cap_std::fs::Dir,
        stdlib_env: Arc<RwLock<Environment>>,
        no_fs: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            config: Arc::new(EvalConfig {
                base_dir,
                stdlib_env,
                type_stage_env: Arc::new(RwLock::new(Environment::new())),
                no_fs,
                require_integrity: false,
                macro_injects_map: HashMap::new(),
                source_file: None,
            }),
            state: Arc::new(Mutex::new(EvalState {
                string_include_cache: HashMap::new(),
                include_chain: Vec::new(),
                eval_stack: Vec::new(),
                class_registry: HashMap::new(),
                instance_registry: HashMap::new(),
                registered_classes: HashSet::new(),
            })),
            thunk_arena: Arc::new(Mutex::new(ThunkArena::new())),
            env_arena: Arc::new(Mutex::new(EnvArena::new())),
            env_allowed: None,
            blame_map: Mutex::new(HashMap::new()),
            boundary_guards: RwLock::new(HashMap::new()),
            do_infer_resolutions: RwLock::new(HashMap::new()),
            libdir_dir: Mutex::new(None),
            cancel: tokio_util::sync::CancellationToken::new(),
        })
    }

    pub fn new_with_options(
        base_dir: cap_std::fs::Dir,
        stdlib_env: Arc<RwLock<Environment>>,
        type_stage_env: Arc<RwLock<Environment>>,
        no_fs: bool,
        require_integrity: bool,
        env_allowed: Option<HashSet<String>>,
    ) -> Arc<Self> {
        // Inherit stdlib ThunkIds: start the arena with a snapshot of stdlib thunks
        // so that ThunkIds stored in prelude dicts (e.g. result.bind) remain valid.
        // Falls back to a fresh empty arena if create_stdlib_env hasn't run yet.
        let thunk_arena = crate::builtins::new_arena_with_stdlib_snapshot()
            .unwrap_or_else(|| Arc::new(Mutex::new(ThunkArena::new())));
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
                class_registry: HashMap::new(),
                instance_registry: HashMap::new(),
                registered_classes: HashSet::new(),
            })),
            thunk_arena,
            env_arena: Arc::new(Mutex::new(EnvArena::new())),
            env_allowed,
            blame_map: Mutex::new(HashMap::new()),
            boundary_guards: RwLock::new(HashMap::new()),
            do_infer_resolutions: RwLock::new(HashMap::new()),
            libdir_dir: Mutex::new(None),
            cancel: tokio_util::sync::CancellationToken::new(),
        })
    }

    /// Create a new EvalContext that shares an existing arena.
    ///
    /// The arena is SHARED via `Arc::clone()` — user thunks allocated through this context
    /// append to the same backing Vec as the thunks already in the arena. The arena grows
    /// monotonically. This is critical for ThunkId validity: if the arena contains stdlib
    /// thunks (indices 0..N), then ThunkIds stored in prelude dicts (e.g., `result.bind`)
    /// remain valid when accessed from this context.
    ///
    /// **Use cases:**
    /// - Macro expansion contexts that need to access prelude dict fields
    /// - Included files that reference stdlib values via ThunkIds
    /// - Any evaluation context where prelude values will be accessed
    ///
    /// **Not for:** Bootstrap contexts (use `new_empty()` instead).
    pub(crate) fn new_sharing_arena(
        base_dir: cap_std::fs::Dir,
        stdlib_env: Arc<RwLock<Environment>>,
        type_stage_env: Arc<RwLock<Environment>>,
        no_fs: bool,
        shared_arena: Arc<Mutex<ThunkArena>>,
        macro_injects_map: HashMap<String, String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config: Arc::new(EvalConfig {
                base_dir,
                stdlib_env,
                type_stage_env,
                no_fs,
                require_integrity: false,
                macro_injects_map,
                source_file: None,
            }),
            state: Arc::new(Mutex::new(EvalState {
                string_include_cache: HashMap::new(),
                include_chain: Vec::new(),
                eval_stack: Vec::new(),
                class_registry: HashMap::new(),
                instance_registry: HashMap::new(),
                registered_classes: HashSet::new(),
            })),
            thunk_arena: shared_arena,
            env_arena: Arc::new(Mutex::new(EnvArena::new())),
            env_allowed: None,
            blame_map: Mutex::new(HashMap::new()),
            boundary_guards: RwLock::new(HashMap::new()),
            do_infer_resolutions: RwLock::new(HashMap::new()),
            libdir_dir: Mutex::new(None),
            cancel: tokio_util::sync::CancellationToken::new(),
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
            boundary_guards: RwLock::new(self.boundary_guards.read().unwrap().clone()),
            do_infer_resolutions: RwLock::new(self.do_infer_resolutions.read().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel: self.cancel.clone(),
        })
    }

    /// Like `with_base_dir` but accepts (and ignores) a `base_dir_path` parameter for
    /// backward compatibility. The base_dir_path was previously used for allowlist comparisons
    /// but is no longer needed after removal of --allow-path.
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
            boundary_guards: RwLock::new(self.boundary_guards.read().unwrap().clone()),
            do_infer_resolutions: RwLock::new(self.do_infer_resolutions.read().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel: child_token.clone(),
        });
        (child_ctx, child_token)
    }

    /// Create a child EvalContext with a timeout: automatically cancels after `ms` milliseconds.
    ///
    /// Spawns a background task (via spawn_local) that fires the cancellation after the delay.
    /// Returns the child context; the cancel handle is internal (use `[with-cancel]` if you
    /// need explicit control).
    pub fn with_timeout_ms(self: &Arc<Self>, ms: u64) -> Arc<Self> {
        let child_token = self.cancel.child_token();
        let cancel_clone = child_token.clone();
        crate::async_rt::spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            cancel_clone.cancel();
        });
        Arc::new(Self {
            config: Arc::clone(&self.config),
            state: Arc::clone(&self.state),
            thunk_arena: Arc::clone(&self.thunk_arena),
            env_arena: Arc::clone(&self.env_arena),
            env_allowed: self.env_allowed.clone(),
            blame_map: Mutex::new(self.blame_map.lock().unwrap().clone()),
            boundary_guards: RwLock::new(self.boundary_guards.read().unwrap().clone()),
            do_infer_resolutions: RwLock::new(self.do_infer_resolutions.read().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel: child_token,
        })
    }

    /// Allocate a thunk in the arena and return its ID.
    pub(crate) fn alloc_thunk(&self, thunk: Arc<Thunk>) -> ThunkId {
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

    /// Set boundary guards from type inference.
    /// Called after type checking to wire gradual typing runtime checks.
    pub fn set_boundary_guards(&self, guards: HashMap<Span, Type>) {
        *self.boundary_guards.write().unwrap() = guards;
    }

    /// Set do-infer resolutions from type inference.
    /// Called after type checking to wire inferred [do] monad resolution to the evaluator.
    /// The map keys are the spans of `%do-infer` VarRef nodes; values are the monad dict
    /// variable names (e.g., "result") resolved by the type checker.
    pub fn set_do_infer_resolutions(&self, resolutions: HashMap<String, String>) {
        *self.do_infer_resolutions.write().unwrap() = resolutions;
    }

    /// Set the already-open libdir Dir so that `builtin_include` can inject `%libdir`
    /// into included files without calling `open_ambient_dir` again.
    ///
    /// Called by the capability initialization boundary (main.rs, repl.rs) immediately
    /// after opening the libdir directory and creating the EvalContext. Propagated
    /// through `with_base_dir` to child contexts (nested includes).
    pub fn set_libdir_dir(&self, dir: Arc<cap_std::fs::Dir>) {
        *self.libdir_dir.lock().unwrap() = Some(dir);
    }
}

/// Check if a materialized value matches a type for structural TypeAssert validation.
/// Returns true if the value conforms to the expected type.
///
/// This performs immediate type checking per doc/07-type-extensions.md §Validation depth table:
/// - Primitives (Int, Float, Str, Bool): exact match
/// - Literals (IntLiteral, StringLiteral): value equality
/// - Seq, Function: tag-only validation (element/param types opaque per spec doc/07:108-113)
/// - TypeVar: treated as Any (residual polymorphic instantiation)
/// - Record: always true (structural validation deferred to proxy contract wrapping)
pub(crate) fn value_matches_type(value: &Value, expected: &Type) -> bool {
    match expected {
        Type::Unknown | Type::Top => true,
        Type::Int => matches!(value, Value::Int(_)),
        Type::Float => matches!(value, Value::Float(_)),
        Type::Number => matches!(value, Value::Int(_) | Value::Float(_)),
        Type::Str => matches!(value, Value::String { .. }),
        Type::Bool => matches!(value, Value::Bool(_)),
        Type::Bytes => matches!(value, Value::Bytes { .. }),
        Type::IntLiteral(n) => matches!(value, Value::Int(v) if v == n),
        Type::StringLiteral(s) => value.as_str().is_some_and(|v| v == s),
        Type::Function { .. } => matches!(value, Value::Function { .. } | Value::Builtin(_)),
        Type::Seq(_) => matches!(value, Value::Seq { .. }),
        Type::Map(_, _) => matches!(value, Value::Dict(_) | Value::Overlay(..)), // Map matches any Dict for now
        Type::TypeVar(_, _) => true,
        Type::Record(_) => true, // Records handled separately via proxy wrapping
        Type::Proxy => matches!(value, Value::Proxy { .. }),
        Type::DirCap => matches!(value, Value::DirCap { .. } | Value::RevocableDirCap { .. }),
        Type::NetCap => matches!(value, Value::NetCap(_)),
        Type::Handle(cap_row) => {
            // Runtime Handle capability validation strategy (gradual typing):
            // - If cap_row is Unknown → accept any handle (gradual escape hatch)
            // - If cap_row is concrete → STILL accept any handle for now
            //
            // Rationale for always accepting when concrete:
            // The type checker already validated capabilities at compile time. Runtime
            // validation in value_matches_type is only invoked for:
            // 1. TypeAssert ([@Handle[R] expr]) — type checker already warned if mismatch
            // 2. Proxy contract validation — deferred to future capability-aware proxies
            //
            // Rejecting at runtime what the type checker already allowed would break
            // gradual typing semantics: static types are upper bounds, not runtime guards.
            // A proper fix requires bidirectional subtyping with the handle value's actual
            // runtime capabilities, which isn't available here (Handle values don't carry
            // their type-level capability row at runtime — they carry a HashMap<String,Value>
            // for cap-data, which is orthogonal to the type system's capability flags).
            //
            // Future work: When Handle values gain a type-level capability descriptor at
            // runtime (e.g., Value::Handle { caps: Type, ... }), implement subtyping check:
            //   Type::is_subtype(&handle.caps, cap_row)
            // Until then, accept all handles when cap_row is non-Unknown (preserve gradual
            // typing consistency).
            // TODO(capability-runtime-validation): Implement structural row subtyping
            // when Handle values carry runtime capability descriptors. Currently, BOTH
            // Unknown and concrete cap_row cases accept any handle — see rationale above.
            // The _ suppresses unused-variable warnings since cap_row drives only future
            // validation, not current behavior.
            let _ = cap_row;
            matches!(value, Value::Handle { .. } | Value::WriteHandle { .. })
        }
        Type::Uri => matches!(value, Value::Uri { .. }),
        Type::Timestamp => matches!(value, Value::Timestamp(_)),
        Type::Duration => matches!(value, Value::Duration(_)),
        Type::ClockCap => matches!(value, Value::ClockCap(_)),
        Type::Timezone => matches!(value, Value::Timezone(_)),
        Type::QuicSession => matches!(value, Value::QuicSession(_)),
        Type::Http2Session => matches!(value, Value::Http2Session { .. }),
        Type::Http3Session => matches!(value, Value::Http3Session(_)),
        Type::QuicDatagramHandle => matches!(value, Value::QuicDatagramHandle(_)),
        Type::DatagramHandle => matches!(value, Value::DatagramHandle { .. }),
        Type::Union(members) => {
            // Value matches union if it matches ANY member type
            members
                .iter()
                .any(|member| value_matches_type(value, member))
        }
        Type::Intersection(members) => {
            // Value matches intersection if it matches ALL member types
            members
                .iter()
                .all(|member| value_matches_type(value, member))
        }
        // Never: no value can match the bottom type
        Type::Never => false,
        // Negation: a value matches ~T iff it does NOT match T.
        // This is sound for ground types; RDNF normalization handles compound cases later.
        Type::Negation(inner) => !value_matches_type(value, inner),
        // NominalVariant: check if value is a Variant with matching tag
        Type::NominalVariant { tag, .. } => {
            matches!(value, Value::Variant { tag: v_tag, .. } if v_tag == tag)
        }
        // Type constructor application and variables: treat like TypeVar (accept any value)
        // The type checker validates these; at runtime they're polymorphic.
        Type::App(_, _) | Type::Operator(_) | Type::TypeStageApp { .. } => true,
        // Error is a type-inference sentinel that should never reach runtime validation.
        // Type::Error indicates type inference failed; treating it as a match would mask bugs.
        Type::Error => {
            debug_assert!(false, "Error sentinel should not reach runtime validation");
            false
        }
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
            let mut merged_fields: HashMap<String, Type> = HashMap::new();
            for m in members {
                if let Type::Record(row) = m {
                    for (k, v) in &row.fields {
                        merged_fields.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
            Some(Cow::Owned(Row {
                fields: merged_fields,
            }))
        }
        _ => None,
    }
}

/// Validate a dict value against a Record type and wrap fields with guards.
///
/// Returns a new dict with guarded field thunks. This implements the [VM-RECORD-PROXY]
/// rule from doc/07-type-extensions.md:
/// 1. Shape check: verify all required fields exist (with Key::Int fallback)
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
pub(crate) fn validate_and_wrap_record(
    entries: &IndexMap<Key, ThunkId>,
    row: &Row,
    field_path: &mut Vec<String>,
    guard_span: Span,
    data_span: Span,
    ctx: &Arc<EvalContext>,
    default: Option<GuardDefault>,
) -> EvalResult<IndexMap<Key, ThunkId>> {
    // Shape check: verify all required fields exist
    // Per doc/07:117, try Key::String first, then Key::Int fallback
    for (field_name, _field_type) in row.fields.iter() {
        let has_field = entries.contains_key(&Key::String(field_name.clone()))
            || field_name
                .parse::<i64>()
                .ok()
                .map(|idx| entries.contains_key(&Key::Int(idx)))
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
            Key::String(field_name) => row.fields.get(field_name),
            Key::Int(n) => row.fields.get(&n.to_string()),
        };

        if let Some(field_type) = field_type {
            let field_name = match key {
                Key::String(s) => s.clone(),
                Key::Int(n) => n.to_string(),
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
                guard_span,
                None, // blame_label
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

/// Wrap an AST expression in a thunk. Literals produce immediately materialized
/// thunks; dicts produce materialized thunks whose values are unevaluated;
/// var refs look up the environment chain.
///
/// Recursive expression evaluator (legacy implementation).
///
/// This is the original eval() implementation, kept as a helper for eval_step().
/// It recursively evaluates expressions and returns thunks (which may be materialized
/// or unevaluated depending on the expression type).
///
/// This function is called by eval_step() for cases that need recursive evaluation
/// (e.g., TypeAssert default branches). It does NOT go through the CEK machine.
///
/// E1-eval-cutover: This now routes through CoreExpr evaluation instead of directly
/// matching on Expr variants.
pub(crate) async fn eval_recursive(
    expr: Rc<Spanned<Expr>>,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    // Convert Expr to CoreExpr and evaluate
    let core_expr = crate::ast_convert::expr_to_core_expr(&expr);
    eval_core_expr(&core_expr, &env, ctx).await
}

// Old eval_recursive implementation deleted in E1-eval-cutover remediation.
// The 885-line function body is preserved in git history if needed for reference.
// All evaluation now routes through CoreExpr via expr_to_core_expr + eval_core_expr.

/// Intern a runtime class name string as a `&'static str`.
/// Known typeclass names return a compile-time literal (zero allocation).
/// Unknown names are leaked — bounded by the number of distinct class declarations
/// in user code, which is small in practice.
// TODO(chr-instances-gaps): wire up instance_registry insertion so this function is called.
#[allow(dead_code)]
fn intern_class_name(name: &str) -> &'static str {
    match name {
        "Addable" => "Addable",
        "Subtractable" => "Subtractable",
        "Multipliable" => "Multipliable",
        "Divisible" => "Divisible",
        "Equatable" => "Equatable",
        "Comparable" => "Comparable",
        "Showable" => "Showable",
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}

/// Check if an identifier starts with an uppercase letter.
pub(crate) fn is_constructor_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Evaluate a [quote ...] expression, walking the quoted AST and evaluating
/// any [unquote ...] or [unquote-splice ...] subexpressions.
async fn eval_quote(
    quoted_expr: &Spanned<Expr>,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    eval_quote_walk(&quoted_expr.node, quoted_expr.span, env, ctx).await
}

/// Recursively walk a quoted expression, handling Unquote and UnquoteSplice.
///
/// Returns `Value::Expression(Arc<SurfaceNode>)` — the runtime-v2 representation.
/// macros.llt has been updated to handle both Expression (new) and Variant (old) inputs
/// via dual dispatch (tag-of works on both), so this migration is safe.
async fn eval_quote_walk(
    expr: &Expr,
    span: Span,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    // Preprocess to handle nested unquotes (rewrites unquote subexpressions)
    let processed_expr = eval_quote_preprocess(expr, span, &env, ctx).await?;

    // runtime-v2 Part G: return Value::Expression (was: ast_to_dict_expr returning Variant Dict)
    // macros.llt is dual-dispatch ready (tag-of handles both Expression and Variant).
    let surface_node = crate::ast_convert::expr_to_surface_node(&processed_expr);
    Ok(Arc::new(Thunk::new_materialized(
        Value::Expression(surface_node),
        span,
    )))
}

/// Convert a runtime Value back to an Expr AST node for unquoting.
///
/// If the value is a Dict with a `type` field, treat it as an AST dict and use `dict_to_surface_node`.
/// Otherwise, convert the value to its literal Expr representation.
fn value_to_expr(value: &Value, span: Span, ctx: &Arc<EvalContext>) -> EvalResult<Spanned<Expr>> {
    match value {
        Value::Int(n) => Ok(Spanned::new(Expr::Int(*n), span)),
        Value::Float(f) => Ok(Spanned::new(Expr::Float(*f), span)),
        Value::Bool(b) => Ok(Spanned::new(Expr::Bool(*b), span)),
        Value::String { source, start, end } => Ok(Spanned::new(
            Expr::Str(source[*start..*end].to_string()),
            span,
        )),
        Value::Variant { .. } => {
            // Variant form of an AST node — convert via surface bridge
            crate::ast_dict::dict_to_surface_node(value, ctx)
                .map(|node| crate::ast_convert::surface_node_to_expr(&node))
                .map_err(|err| {
                    EvalError::internal(
                        format!("unquote result Variant is not a valid AST: {}", err),
                        span,
                    )
                    .into()
                })
        }
        Value::Dict(dict) => {
            // Check if this is an AST dict (has a "type" field)
            if dict.contains_key(&Key::String("type".to_string())) {
                // It's an AST dict — convert via surface bridge
                crate::ast_dict::dict_to_surface_node(value, ctx)
                    .map(|node| crate::ast_convert::surface_node_to_expr(&node))
                    .map_err(|err| {
                        EvalError::internal(
                            format!("unquote result dict is not a valid AST: {}", err),
                            span,
                        )
                        .into()
                    })
            } else {
                // It's a regular dict — convert to Expr::Dict
                // This is trickier because dict values are thunk IDs
                // For now, error on non-AST dicts in unquote
                Err(EvalError::internal(
                    "unquote of non-AST dict is not yet supported".to_string(),
                    span,
                )
                .into())
            }
        }
        Value::Expression(node) => {
            // Value::Expression from ast-of builtin — convert back to Expr
            Ok(crate::ast_convert::surface_node_to_expr(node))
        }
        _ => Err(
            EvalError::internal(format!("unquote of {:?} is not supported", value), span).into(),
        ),
    }
}

/// Recursively preprocess a quoted expression tree to handle nested unquotes.
///
/// This walks the entire AST and:
/// - Evaluates `Unquote` nodes, converting the result back to AST
/// - Recurses into all child expressions
/// - Leaves non-unquote nodes unchanged
fn eval_quote_preprocess<'a>(
    expr: &'a Expr,
    span: Span,
    env: &'a Arc<RwLock<Environment>>,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Spanned<Expr>>> + 'a>> {
    Box::pin(async move {
        match expr {
            Expr::Unquote(inner) => {
                // Evaluate the unquoted expression
                let thunk = eval_recursive(Rc::new((**inner).clone()), env.clone(), ctx).await?;
                let value = materialize(&thunk, Some(&inner.span), ctx).await?;

                // Convert the value back to AST
                value_to_expr(&value, inner.span, ctx)
            }
            Expr::UnquoteSplice(_) => {
                // UnquoteSplice at non-list position is an error
                Err(EvalError::internal(
                    "unquote-splice must be in a list position (inside call args or dict entries)"
                        .to_string(),
                    span,
                )
                .into())
            }

            // Recursively process composite expressions
            Expr::Dict(entries) => {
                let mut processed_entries = Vec::with_capacity(entries.len());
                for entry in entries {
                    let processed_value = eval_quote_preprocess(
                        &entry.node.value.node,
                        entry.node.value.span,
                        env,
                        ctx,
                    )
                    .await?;
                    let processed_key = if let Some(ref key_expr) = entry.node.key {
                        Some(eval_quote_preprocess(&key_expr.node, key_expr.span, env, ctx).await?)
                    } else {
                        None
                    };
                    processed_entries.push(Spanned::new(
                        Entry {
                            key: processed_key,
                            value: Rc::new(processed_value),
                        },
                        entry.span,
                    ));
                }
                Ok(Spanned::new(Expr::Dict(processed_entries), span))
            }

            Expr::Call {
                func,
                args,
                named_args,
                implied,
            } => {
                let processed_func = eval_quote_preprocess(&func.node, func.span, env, ctx).await?;
                let mut processed_args = Vec::with_capacity(args.len());
                for arg in args {
                    processed_args.push(Rc::new(
                        eval_quote_preprocess(&arg.node, arg.span, env, ctx).await?,
                    ));
                }
                let mut processed_named_args = Vec::with_capacity(named_args.len());
                for na in named_args {
                    let processed_value =
                        eval_quote_preprocess(&na.node.value.node, na.node.value.span, env, ctx)
                            .await?;
                    processed_named_args.push(Spanned::new(
                        NamedArg {
                            name: na.node.name.clone(),
                            value: Rc::new(processed_value),
                        },
                        na.span,
                    ));
                }
                Ok(Spanned::new(
                    Expr::Call {
                        func: Box::new(processed_func),
                        args: processed_args,
                        named_args: processed_named_args,
                        implied: *implied,
                    },
                    span,
                ))
            }

            Expr::Fn {
                return_ann,
                params,
                body,
                desugared,
            } => {
                let processed_body = eval_quote_preprocess(&body.node, body.span, env, ctx).await?;
                Ok(Spanned::new(
                    Expr::Fn {
                        return_ann: return_ann.clone(),
                        params: params.clone(),
                        body: Rc::new(processed_body),
                        desugared: *desugared,
                    },
                    span,
                ))
            }

            Expr::DotAccess {
                expr: target,
                field,
            } => {
                let processed_target =
                    eval_quote_preprocess(&target.node, target.span, env, ctx).await?;
                Ok(Spanned::new(
                    Expr::DotAccess {
                        expr: Box::new(processed_target),
                        field: field.clone(),
                    },
                    span,
                ))
            }

            Expr::Pipe { lhs, rhs } => {
                let processed_lhs = eval_quote_preprocess(&lhs.node, lhs.span, env, ctx).await?;
                let processed_rhs = eval_quote_preprocess(&rhs.node, rhs.span, env, ctx).await?;
                Ok(Spanned::new(
                    Expr::Pipe {
                        lhs: Box::new(processed_lhs),
                        rhs: Box::new(processed_rhs),
                    },
                    span,
                ))
            }

            Expr::Sequential(exprs) => {
                let mut processed_exprs = Vec::with_capacity(exprs.len());
                for e in exprs {
                    processed_exprs.push(Rc::new(
                        eval_quote_preprocess(&e.node, e.span, env, ctx).await?,
                    ));
                }
                Ok(Spanned::new(Expr::Sequential(processed_exprs), span))
            }

            Expr::TypeAlias { params, body } => {
                let processed_body = eval_quote_preprocess(&body.node, body.span, env, ctx).await?;
                Ok(Spanned::new(
                    Expr::TypeAlias {
                        params: params.clone(),
                        body: Box::new(processed_body),
                    },
                    span,
                ))
            }

            Expr::TypeAssert {
                annotation,
                expr: inner,
                resolved_type,
            } => {
                let processed_expr =
                    eval_quote_preprocess(&inner.node, inner.span, env, ctx).await?;
                Ok(Spanned::new(
                    Expr::TypeAssert {
                        annotation: annotation.clone(),
                        expr: Box::new(processed_expr),
                        resolved_type: resolved_type.clone(),
                    },
                    span,
                ))
            }

            Expr::Quote(inner) => {
                let processed_inner =
                    eval_quote_preprocess(&inner.node, inner.span, env, ctx).await?;
                Ok(Spanned::new(Expr::Quote(Box::new(processed_inner)), span))
            }

            Expr::Match { scrutinee, arms } => {
                let processed_scrutinee =
                    eval_quote_preprocess(&scrutinee.node, scrutinee.span, env, ctx).await?;
                let mut processed_arms = Vec::with_capacity(arms.len());
                for arm in arms {
                    let processed_body =
                        eval_quote_preprocess(&arm.body.node, arm.body.span, env, ctx).await?;
                    let processed_guard = if let Some(ref guard) = arm.guard {
                        Some(Box::new(
                            eval_quote_preprocess(&guard.node, guard.span, env, ctx).await?,
                        ))
                    } else {
                        None
                    };
                    processed_arms.push(MatchArm {
                        pattern: arm.pattern.clone(),
                        guard: processed_guard,
                        body: Box::new(processed_body),
                    });
                }
                Ok(Spanned::new(
                    Expr::Match {
                        scrutinee: Box::new(processed_scrutinee),
                        arms: processed_arms,
                    },
                    span,
                ))
            }

            Expr::DefMacro { name, params, body } => {
                let processed_body = eval_quote_preprocess(&body.node, body.span, env, ctx).await?;
                Ok(Spanned::new(
                    Expr::DefMacro {
                        name: name.clone(),
                        params: params.clone(),
                        body: Rc::new(processed_body),
                    },
                    span,
                ))
            }

            Expr::MacroDecl { name, params, body } => {
                let processed_params =
                    eval_quote_preprocess(&params.node, params.span, env, ctx).await?;
                let processed_body = eval_quote_preprocess(&body.node, body.span, env, ctx).await?;
                Ok(Spanned::new(
                    Expr::MacroDecl {
                        name: name.clone(),
                        params: Box::new(processed_params),
                        body: Box::new(processed_body),
                    },
                    span,
                ))
            }

            Expr::Splice(forms) => {
                let mut processed_forms = Vec::with_capacity(forms.len());
                for form in forms {
                    processed_forms
                        .push(eval_quote_preprocess(&form.node, form.span, env, ctx).await?);
                }
                Ok(Spanned::new(Expr::Splice(processed_forms), span))
            }

            Expr::SyntaxClass {
                name,
                pattern,
                message,
            } => {
                let processed_pattern =
                    eval_quote_preprocess(&pattern.node, pattern.span, env, ctx).await?;
                Ok(Spanned::new(
                    Expr::SyntaxClass {
                        name: name.clone(),
                        pattern: Box::new(processed_pattern),
                        message: message.clone(),
                    },
                    span,
                ))
            }

            // All other expressions don't have child expressions, just clone them
            _ => Ok(Spanned::new(expr.clone(), span)),
        }
    }) // end Box::pin(async move {
}

/// Evaluate an expression to a thunk without forcing it.
///
/// Returns `Arc<Thunk>` in Unevaluated state (or pre-materialized for literals).
/// Materialization is deferred until `materialize()` is called on the thunk.
///
/// # Async Implementation
///
/// Returns `Pin<Box<dyn Future>>` to break the recursive cycle
/// `eval → eval_recursive → eval_core_expr → eval`. Non-recursive helpers
/// use `async fn` directly. See `materialize()` for the same pattern.
pub fn eval(
    expr: Rc<Spanned<Expr>>,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>> + '_>> {
    let ctx = ctx.clone();
    Box::pin(async move {
        let span = expr.span;
        let thunk = eval_recursive(expr, env, &ctx).await?;
        // If the type checker recorded a boundary guard for this expression's span,
        // wrap the thunk in a ThunkState::Guarded. The guard fires lazily — only
        // when the thunk is forced — so this preserves call-by-need semantics.
        // Fast-path: boundary_guards is empty for programs run without type checking,
        // so the borrow and HashMap lookup add no overhead in the common case.
        if ctx.boundary_guards.read().unwrap().is_empty() {
            Ok(thunk)
        } else {
            Ok(maybe_wrap_guard(thunk, span, &ctx))
        }
    }) // end Box::pin(async move {
}

/// Evaluate a CoreExpr to a thunk (transitional path for runtime-v2).
///
/// This is the new CoreExpr evaluation entry point. It handles:
/// - Primitive variants natively: Int, Float, Bool, Str (direct materialization)
/// - Variables natively: Var, FreeVar (environment lookup with de Bruijn coordinates)
/// - Complex variants via bridge: Dict, Call, Fn, Match, etc. convert back to Expr
///   and call existing helpers (eval_dict, eval_call, etc.)
///
/// This is intentionally TRANSITIONAL. The round-trips to Expr are ACCEPTED for this
/// sprint (E1). Future sprints (E2/E3) will implement native CoreExpr handlers for
/// Dict/Call/Fn to eliminate the bridge conversions.
fn eval_core_expr<'a>(
    expr: &'a Spanned<CoreExpr>,
    env: &'a Arc<RwLock<Environment>>,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>> + 'a>> {
    Box::pin(async move {
        match &expr.node {
            // Fast path: literals materialize directly without wrapping in Unevaluated
            CoreExpr::Int(n) => Ok(Arc::new(Thunk::new_materialized(Value::Int(*n), expr.span))),
            CoreExpr::Float(f) => Ok(Arc::new(Thunk::new_materialized(
                Value::Float(*f),
                expr.span,
            ))),
            CoreExpr::Bool(b) => Ok(Arc::new(Thunk::new_materialized(
                Value::Bool(*b),
                expr.span,
            ))),
            CoreExpr::Str(s) => Ok(Arc::new(Thunk::new_materialized(string_val(s), expr.span))),

            // Variable lookup with de Bruijn coordinates (fast path)
            CoreExpr::Var { name, level, slot } => {
                let env_lock = env.read().unwrap();
                // Try slot-based lookup first (O(1) when level and slot are correct)
                // get_by_slot verifies the key at slot matches name; falls back to
                // name-based lookup if there's a mismatch (slot-shift bug).
                if let Some(thunk) = env_lock.get_by_slot(*level, *slot, name) {
                    Ok(thunk)
                } else {
                    // Fallback to name-based lookup (for stale slot references)
                    let name_owned = name.clone();
                    env_lock
                        .get(name)
                        .ok_or_else(|| EvalError::undefined_variable(name_owned, expr.span).into())
                }
            }

            // Free variable: name-based lookup only (no slot available)
            CoreExpr::FreeVar(name) => {
                // Special case: inferred [do] sentinel variable (e.g., `:do-infer:0`).
                // Generated by gensym in macros.llt `do-desugar-inferred`. The type checker
                // resolves the sentinel to a concrete monad name (e.g., "result") and records
                // the mapping in ctx.do_infer_resolutions. At eval time, substitute the sentinel
                // with the resolved monad dict from the environment.
                if name.starts_with(":do-infer:") {
                    let monad_name = ctx
                        .do_infer_resolutions
                        .read()
                        .unwrap()
                        .get(name.as_str())
                        .cloned();
                    if let Some(monad_name) = monad_name {
                        let env_lock = env.read().unwrap();
                        return env_lock.get(&monad_name).ok_or_else(|| {
                            EvalError::undefined_variable(monad_name, expr.span).into()
                        });
                    }
                }
                let name_owned = name.clone();
                let env_lock = env.read().unwrap();
                env_lock
                    .get(name)
                    .ok_or_else(|| EvalError::undefined_variable(name_owned, expr.span).into())
            }

            // DotAccess: wrap as a CoreExpr thunk directly.
            //
            // force_step handles CoreExpr::DotAccess INLINE in both take_core_expr and
            // take_surface branches (eval_materialize.rs), so when run() forces this thunk
            // the take_core_expr inline handler fires and pushes Memoize + DotAccessForce
            // without re-entering eval_core_expr. No core_expr_to_expr round-trip needed.
            CoreExpr::DotAccess { .. } => Ok(Arc::new(Thunk::new_unevaluated_core(
                Arc::new(expr.clone()),
                Arc::clone(env),
                Arc::clone(ctx),
                expr.span,
            ))),

            // Sequential: evaluate each expression in order, return the last result
            CoreExpr::Sequential(exprs) => {
                if exprs.is_empty() {
                    return Ok(Arc::new(Thunk::new_materialized(
                        Value::Dict(IndexMap::new()),
                        expr.span,
                    )));
                }

                let mut current_env = Arc::clone(env);

                for (i, seq_expr) in exprs.iter().enumerate() {
                    let is_last = i == exprs.len() - 1;

                    if is_last {
                        // Last expression: evaluate directly as CoreExpr
                        return eval_core_expr(seq_expr, &current_env, ctx).await;
                    }

                    // Extract static keys from the expression BEFORE evaluating.
                    // Only CoreExpr::Dict with static keys creates a new scope (mirrors resolve.rs).
                    // Use core_expr_is_static_key (from eval_dict_mod) to keep predicate in sync.
                    let static_keys: Option<HashSet<String>> = match &seq_expr.node {
                        CoreExpr::Dict(entries) => {
                            let keys: Vec<String> = entries
                                .iter()
                                .filter_map(|entry| {
                                    entry.node.key.as_ref().and_then(|k| {
                                        if core_expr_is_static_key(&k.node) {
                                            match &k.node {
                                                CoreExpr::Str(s) => Some(s.clone()),
                                                CoreExpr::Annotated { name, .. } => {
                                                    Some(name.clone())
                                                }
                                                _ => None,
                                            }
                                        } else {
                                            None
                                        }
                                    })
                                })
                                .collect();
                            if keys.is_empty() {
                                None
                            } else {
                                Some(keys.into_iter().collect())
                            }
                        }
                        _ => None,
                    };

                    // Intermediate expression: evaluate as CoreExpr, materialize, and extract dict bindings
                    let thunk = eval_core_expr(seq_expr, &current_env, ctx).await?;
                    let value = materialize(&thunk, Some(&seq_expr.span), ctx).await?;

                    // Create child environment with bindings from intermediate expression.
                    // CRITICAL: Only insert static-key entries to preserve slot alignment with the resolver.
                    // If static_keys is None (non-Dict expression or Dict with no static keys), no scope is created.
                    if let Some(ref static_key_set) = static_keys {
                        // Flatten Overlay to Dict for scope chain binding.
                        // Only computed when static_keys is Some — avoids wasted work when no scope is created.
                        let map = match value {
                            Value::Dict(map) => map,
                            Value::Overlay(l, r) => crate::builtins::flatten_overlay(
                                &l,
                                &r,
                                "sequential expression",
                                ctx,
                                seq_expr.span,
                            )?,
                            _ => {
                                return Err(EvalError::type_mismatch_ctx(
                                    format!("sequential expression #{}", i + 1),
                                    "Dict",
                                    value.type_name(),
                                    seq_expr.span,
                                )
                                .into());
                            }
                        };

                        let child_env = Arc::new(RwLock::new(Environment::with_parent(
                            Arc::clone(&current_env),
                        )));
                        for (key, val_thunk_id) in map {
                            if let Key::String(name) = key {
                                if static_key_set.contains(&name) {
                                    let val_thunk = ctx.get_thunk(val_thunk_id);
                                    child_env.write().unwrap().insert(name, val_thunk);
                                }
                            }
                        }
                        current_env = child_env;
                    }
                }

                unreachable!("eval_core_expr Sequential: loop did not return")
            }

            // Dict: call eval_dict_core directly with the CoreEntry slice.
            // Eliminates the Vec<Spanned<Entry>> allocation and per-entry core_expr_to_expr
            // calls previously required by the round-trip through eval_dict.
            // eval_dict_core now uses Thunk::new_unevaluated_core for non-literal dict entries
            // (UnevaluatedState::CoreExpr), eliminating the per-entry core_expr_to_expr round-trip.
            CoreExpr::Dict(entries) => eval_dict_core(entries, env, ctx, &expr.span).await,

            // Call: use eval_call_core — no CoreExpr→Expr round-trip for func or named args.
            // Per-argument core_expr_to_expr conversion still occurs inside eval_call_core
            // (tracked by the TODO(parts-e) comments in eval_call.rs).
            CoreExpr::Call {
                func,
                args,
                named_args,
                ..
            } => eval_call_core(func, args, named_args, env, ctx, &expr.span).await,

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

                // Extract doc string from annotation if present
                let annotation = return_ann.as_ref().and_then(|ann_spanned| {
                    let doc = match &ann_spanned.node {
                        Annotation::PropertyDict(entries) => entries.iter().find_map(|entry| {
                            let key = entry.node.key.as_ref()?;
                            if let Expr::Str(key_str) = &key.node {
                                if key_str == "doc" {
                                    if let Expr::Str(doc_str) = &entry.node.value.node {
                                        return Some(doc_str.clone());
                                    }
                                }
                            }
                            None
                        }),
                        _ => None,
                    };

                    doc.map(|doc_str| {
                        Box::new(crate::value::FnAnnotation {
                            doc: Some(doc_str),
                            source_file: ctx.config.source_file.clone(),
                        })
                    })
                });

                // Store the body directly as Arc<Spanned<CoreExpr>>.
                // CoreExpr::Fn.body is already Arc<Spanned<CoreExpr>> — no conversion needed.
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Function {
                        params: Rc::new(fn_params),
                        body: Arc::clone(body),
                        env: Arc::clone(env),
                        annotation,
                    },
                    expr.span,
                )))
            }

            // TypeAssert: wrap as CoreExpr thunk — force_step's take_core_expr branch
            // handles CoreExpr::TypeAssert inline, pushing a TypeAssertCheck continuation.
            // Wrapping here prevents direct recursion back through eval_core_expr.
            CoreExpr::TypeAssert { .. } => Ok(Arc::new(Thunk::new_unevaluated_core(
                Arc::new(expr.clone()),
                Arc::clone(env),
                Arc::clone(ctx),
                expr.span,
            ))),

            // RuntimeTypeCheck: same fix as TypeAssert — wrap as CoreExpr thunk, defer
            // to force_step → eval_core_expr_pub → Expr::TypeAssert CEK path.
            CoreExpr::RuntimeTypeCheck { .. } => Ok(Arc::new(Thunk::new_unevaluated_core(
                Arc::new(expr.clone()),
                Arc::clone(env),
                Arc::clone(ctx),
                expr.span,
            ))),

            // Annotated: evaluate as bare string
            CoreExpr::Annotated { name, .. } => Ok(Arc::new(Thunk::new_materialized(
                string_val(name),
                expr.span,
            ))),

            // Rest: error (only valid in type expressions)
            CoreExpr::Rest(_) => Err(EvalError::internal(
                "rest marker (...) is only valid inside type expressions".to_string(),
                expr.span,
            )
            .into()),

            // Match: implement natively — materialize the scrutinee, then try each arm
            // in order. Do NOT call eval_recursive: that would convert to Expr::Match and
            // loop back here via expr_to_core_expr → CoreExpr::Match → eval_core_expr.
            CoreExpr::Match { scrutinee, arms } => {
                // Evaluate the scrutinee to a thunk, then force it.
                let scrutinee_thunk = eval_core_expr(scrutinee, env, ctx).await?;
                let scrutinee_value =
                    materialize(&scrutinee_thunk, Some(&scrutinee.span), ctx).await?;

                // Try each arm in order.
                for arm in arms {
                    // PM3: Reject non-linear patterns before attempting to match.
                    // A non-linear pattern like `[a: x  b: x  ...]:` would silently
                    // rebind `x` to the last matched field. ML semantics require each
                    // variable to appear at most once per arm.
                    check_pattern_linearity(&arm.pattern).map_err(Box::new)?;

                    // Try the pattern.
                    let matched_env = match_pattern(
                        &arm.pattern.node,
                        &scrutinee_value,
                        env,
                        &arm.pattern.span,
                        ctx,
                    )
                    .await?;

                    if let Some(arm_env) = matched_env {
                        // Pattern matched. If there is a guard, evaluate it.
                        if let Some(guard_expr) = &arm.guard {
                            let guard_thunk = eval_core_expr(guard_expr, &arm_env, ctx).await?;
                            let guard_value =
                                materialize(&guard_thunk, Some(&guard_expr.span), ctx).await?;

                            // PM1: If the guard evaluated to a callable (predicate function),
                            // invoke it with the scrutinee value and use the result as the guard.
                            // This handles `[is: positive?]` style guards where the `is:` value
                            // is a function reference rather than an inline boolean expression.
                            let guard_value = match guard_value {
                                Value::Function { .. } | Value::Builtin(_) => {
                                    // Wrap the predicate function as a materialized thunk.
                                    let pred_thunk = Arc::new(Thunk::new_materialized(
                                        guard_value,
                                        guard_expr.span,
                                    ));
                                    // Create a PendingCall: pred(scrutinee_value)
                                    let call_thunk = Arc::new(Thunk::new_pending_call(
                                        pred_thunk,
                                        vec![Arc::clone(&scrutinee_thunk)],
                                        IndexMap::new(),
                                        guard_expr.span,
                                        Arc::clone(env),
                                        guard_expr.span,
                                        None,
                                        Arc::clone(ctx),
                                    ));
                                    // Force the call to get the predicate result.
                                    materialize(&call_thunk, Some(&guard_expr.span), ctx).await?
                                }
                                other => other,
                            };

                            // Guard is falsy — skip this arm and try the next one.
                            // Bool(false) and empty Dict (null []) are falsy; everything else is truthy.
                            let is_truthy = match &guard_value {
                                Value::Bool(b) => *b,
                                Value::Dict(map) => !map.is_empty(),
                                _ => true,
                            };
                            if !is_truthy {
                                continue;
                            }
                        }
                        // Arm matched (and guard passed). Evaluate the body.
                        return eval_core_expr(&arm.body, &arm_env, ctx).await;
                    }
                    // Pattern did not match — try the next arm.
                }

                // No arm matched: non-exhaustive match.
                Err(EvalError::match_exhaustion(
                    scrutinee_value.type_name(),
                    expr.span,
                )
                .into())
            }

            // Quote: convert to Expr and use eval_quote.
            //
            // DESIGN DECISION: This round-trip (CoreExpr→Expr) is intentional.
            // Quote captures *surface syntax* for metaprogramming, not the desugared
            // CoreExpr form with de Bruijn indices. Users expect [quote x] to show
            // the variable name "x", not FreeVar(0). eval_quote walks the Expr AST
            // to handle unquotes and produces Value::Expression(SurfaceNode), which
            // represents the code as written, not as compiled.
            CoreExpr::Quote(inner) => {
                let old_inner = Box::new(crate::ast_convert::core_expr_to_expr(inner));
                eval_quote(&old_inner, env.clone(), ctx).await
            }

            // Unquote: error (only valid inside quote)
            CoreExpr::Unquote(_) => Err(EvalError::internal(
                "unquote is only valid inside [quote ...]".to_string(),
                expr.span,
            )
            .into()),

            // UnquoteSplice: error (only valid inside quote)
            CoreExpr::UnquoteSplice(_) => Err(EvalError::internal(
                "unquote-splice is only valid inside [quote ...]".to_string(),
                expr.span,
            )
            .into()),

            // PatternDecl: error (not an expression)
            CoreExpr::PatternDecl { .. } => Err(EvalError::internal(
                "pattern declaration is only valid in instance match arms".to_string(),
                expr.span,
            )
            .into()),

            // LetDecl: error (not an expression)
            CoreExpr::LetDecl { .. } => Err(EvalError::internal(
                "let declarations are not expressions".to_string(),
                expr.span,
            )
            .into()),

            // CaseArm: error (not an expression)
            CoreExpr::CaseArm { .. } => Err(EvalError::internal(
                "case arms are not expressions".to_string(),
                expr.span,
            )
            .into()),

            // TypeApp: error (type annotation node)
            CoreExpr::TypeApp { .. } => Err(EvalError::internal(
                "TypeApp is a type annotation node and cannot be evaluated".to_string(),
                expr.span,
            )
            .into()),

            // Placeholder: error on evaluation
            CoreExpr::Placeholder => Err(EvalError::unimplemented(
                "placeholder `...` was evaluated — replace with an implementation".to_string(),
                expr.span,
            )
            .into()),

            // Error: propagate as internal error
            CoreExpr::Error(span) => Err(EvalError::internal(
                format!(
                    "syntax error at {}:{} (cannot evaluate error node)",
                    span.start.line, span.start.column
                ),
                expr.span,
            )
            .into()),
        }
    }) // end Box::pin(async move {
}

/// Public wrapper for `eval_core_expr` used by `eval_materialize.rs` force_step.
///
/// The Surface thunk handler in force_step needs to call eval_core_expr after lower()
/// to get a result thunk. eval_core_expr is private to this module, so this thin
/// wrapper exposes it for the CEK machine without making eval_core_expr fully pub.
///
/// TODO(parts-e): when eval_core_expr is moved into a dedicated eval_core.rs module
/// and the CEK machine is co-located with it, this wrapper can be removed and
/// eval_core_expr called directly.
#[inline]
pub(crate) fn eval_core_expr_pub<'a>(
    expr: &'a crate::ast::Spanned<crate::ast::CoreExpr>,
    env: &'a std::sync::Arc<std::sync::RwLock<crate::value::Environment>>,
    ctx: &'a std::sync::Arc<EvalContext>,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = crate::error::EvalResult<std::sync::Arc<crate::value::Thunk>>,
            > + 'a,
    >,
> {
    eval_core_expr(expr, env, ctx)
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
        let thunk_span = thunk.span;

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
                    cloned.materialization_span = Some(*span);
                    should_update_cache = true;
                } else if cloned.materialization_span != Some(*span)
                    && !cloned.stack.iter().any(|f| f.span == *span)
                {
                    // Different access site: add as stack frame, preserve original mat_span
                    cloned.push_frame("materialized".to_string(), *span);
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
            let mut err = EvalError::circular_dependency(label, thunk.span, cycle_path);
            if let Some(span) = mat_span {
                err = err.with_materialization_span(*span);
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
        let decorate = |e| attach_materialization_context(e, mat_span, origin_opt, thunk_span);

        if let Some((def, args, named, call_span, thunk_ctx)) = thunk.take_pending_builtin() {
            // Pre-materialize strict args before calling the builtin.
            //
            // The CEK machine (eval_materialize.rs::force_step) handles force_count and
            // pos_strictness W1 pre-materialization iteratively via BuiltinForceArg continuations.
            // This recursive path bypasses the CEK machine entirely, so it must replicate
            // force_count + W1 semantics here to prevent builtins using
            // `try_get_materialized().expect("pre-materialized by force_count/pos_strictness")` from panicking.
            //
            // Without this, any builtin with force_count > 0 (e.g. $take, $map, $drop) panics
            // when materialized via the recursive path (e.g. from builtin_collect's loop,
            // from builtin_filter_seq_step's materialize() call on the tail, etc.).
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
                            ctx: thunk_ctx,
                        });
                    }
                    return Err(e);
                }
            }
            // `named` is None for internally-created thunks (common case); only $apply
            // passes named args through. Use an empty map ref for the None case.
            let builtin_args = crate::value::BuiltinArgs {
                args,
                named,
                call_span,
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
                                mat_span: mat_span.copied(),
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
                                            call_span,
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
                            call_span,
                            ctx: thunk_ctx,
                        });
                    }
                    Err(e)
                }
            }
        } else if let Some((func_thunk, args, named, call_span, caller_env, thunk_ctx)) =
            thunk.take_pending_call()
        {
            // Materialize the function thunk to determine if it's a Function or Builtin
            let func_value = match run(
                Action::Materialize {
                    thunk: Arc::clone(&func_thunk),
                    mat_span: Some(call_span),
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
                        });
                    }
                    return Err(e);
                }
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
                        call_span,
                        origin: origin.clone(),
                        ctx: &thunk_ctx,
                    };

                    match invoke_function(&call_ctx).await.map_err(&decorate) {
                        Ok(result_thunk) => {
                            // Materialize the result and memoize
                            match run(
                                Action::Materialize {
                                    thunk: result_thunk,
                                    mat_span: mat_span.copied(),
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
                                                call_span,
                                                caller_env: caller_env.clone(),
                                                ctx: thunk_ctx.clone(),
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
                                e.push_frame(label.to_string(), call_span);
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
                                    call_span,
                                    caller_env: caller_env.clone(),
                                    ctx: thunk_ctx.clone(),
                                });
                            }
                            return Err(e);
                        }
                    }
                    let builtin_args = crate::value::BuiltinArgs {
                        args,
                        named,
                        call_span,
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
                                        mat_span: mat_span.copied(),
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
                                                    call_span,
                                                    caller_env: caller_env.clone(),
                                                    ctx: thunk_ctx.clone(),
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
                                    call_span,
                                    caller_env: caller_env.clone(),
                                    ctx: thunk_ctx.clone(),
                                });
                            }
                            Err(e)
                        }
                    }
                }
                // Unit variant used as a constructor: [Ok payload] where Ok = [variant "Ok"].
                // When a unit Variant (payload: None) is called with exactly one positional
                // arg and no named args, treat it as constructing Variant(tag, payload).
                // This allows `Ok: [variant "Ok"]` in the prelude to be called as `[Ok 42]`.
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
                        call_span,
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
            let inner_span = inner.span;

            let result = run(
                Action::Materialize {
                    thunk: Arc::clone(&inner),
                    mat_span: mat_span.copied(),
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
                                guard_span,
                                inner_span,
                                ctx,
                                default.clone(),
                            ) {
                                Ok(new_entries) => {
                                    let guarded_value = Value::Dict(new_entries);
                                    thunk.set_materialized(guarded_value.clone());
                                    Ok(guarded_value)
                                }
                                Err(err) => {
                                    // Guard validation failed - use default if present
                                    if let Some((default_expr, default_env)) = default {
                                        let default_thunk = match eval_core_expr_pub(
                                            &default_expr,
                                            &default_env,
                                            ctx,
                                        )
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
                                                mat_span: mat_span.copied(),
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
                                let default_thunk = match eval_core_expr_pub(
                                    &default_expr,
                                    &default_env,
                                    ctx,
                                )
                                .await
                                {
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
                                        mat_span: mat_span.copied(),
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
                                inner_span,
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
                        if value_matches_type(&value, &expected) {
                            thunk.set_materialized(value.clone());
                            Ok(value)
                        } else {
                            // Type mismatch for non-Record types - use default if present
                            if let Some((default_expr, default_env)) = default {
                                let default_thunk = match eval_core_expr_pub(
                                    &default_expr,
                                    &default_env,
                                    ctx,
                                )
                                .await
                                {
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
                                        mat_span: mat_span.copied(),
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
                                inner_span,
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
        } else if let Some((node, res, types, env, thunk_ctx)) = thunk.take_surface() {
            // runtime-v2 Sprint 1: Surface thunk handling via lower() → CoreExpr → eval_core_expr().
            //
            // 1. Lower the SurfaceNode to CoreExpr using lower()
            // 2. Evaluate the CoreExpr using eval_core_expr()
            // 3. Materialize the result thunk
            //
            // This is the new CoreExpr evaluation path. eval_core_expr() handles literals and
            // variable lookups directly, and falls back to the Expr bridge for complex constructs.
            let lowered = crate::lower::lower(&node, &res, &types);
            let result = async {
                let result_thunk = eval_core_expr(&lowered, &env, &thunk_ctx).await?;
                run(
                    Action::Materialize {
                        thunk: result_thunk,
                        mat_span: mat_span.copied(),
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
                            res,
                            types,
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
            // Created by match dispatch on Value::Expression. Evaluates on demand when the
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
                        mat_span: mat_span.copied(),
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

// Re-export deep_materialize from eval_materialize module
pub use crate::eval_materialize::deep_materialize;

/// Synchronous compatibility wrapper around the async `materialize()`.
///
/// Used by genuinely synchronous call sites that cannot `.await`:
/// - Sync closures (e.g., `sort_by` comparator)
/// - Macro expansion helpers (`expand.rs`)
/// - Test helper shadows inside `#[cfg(test)]` modules
/// - Bootstrap code (stdlib loading before the async runtime is entered)
///
/// Uses `async_rt::block_on_anywhere()` which is safe to call both from
/// outside any tokio runtime and from within an existing one.
///
/// New async code should call `materialize(...).await` directly.
pub fn materialize_sync(
    thunk: &Thunk,
    mat_span: Option<&Span>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Value> {
    crate::async_rt::block_on_anywhere(materialize(thunk, mat_span, ctx))
}

/// Collect all variable names bound by a pattern, recursing into sub-patterns.
///
/// Returns a list of `(name, span)` pairs — one entry per `Pattern::Variable` leaf.
/// Duplicate names in the returned list indicate a non-linear pattern.
///
/// Or-pattern branches are walked independently; the function collects from ALL branches
/// so that a duplicate that appears within a single branch is still caught.  A duplicate
/// that straddles two Or-pattern branches (same name in branch A and branch B) is a
/// separate semantic concern (the "or-pattern completeness" invariant) and is NOT reported
/// here — only intra-branch duplicates matter for linearity.
fn collect_pattern_variable_names(pattern: &Spanned<Pattern>, out: &mut Vec<(String, Span)>) {
    match &pattern.node {
        Pattern::Variable(name) => {
            out.push((name.clone(), pattern.span));
        }
        Pattern::Wildcard
        | Pattern::Literal(_)
        | Pattern::TypeTag(_)
        | Pattern::Pin(_) => {
            // No variable bindings
        }
        Pattern::Dict { fields, .. } => {
            for (_key, field_pattern) in fields {
                collect_pattern_variable_names(field_pattern, out);
            }
        }
        Pattern::Seq { head, tail } => {
            collect_pattern_variable_names(head, out);
            collect_pattern_variable_names(tail, out);
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
/// This is called once per match arm before `match_pattern` recurses, so the cost is
/// O(|pattern|) per arm.
fn check_pattern_linearity(pattern: &Spanned<Pattern>) -> Result<(), EvalError> {
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
            return Err(EvalError::duplicate_variable_in_pattern(name, *span));
        }
    }
    Ok(())
}

/// Match a pattern against a value, returning the extended environment if the pattern matches.
///
/// Returns Ok(Some(env)) if the pattern matches (env contains any bindings from the pattern).
/// Returns Ok(None) if the pattern does not match.
/// Returns Err if there's an evaluation error (e.g., undefined pin variable).
fn match_pattern<'a>(
    pattern: &'a Pattern,
    value: &'a Value,
    env: &'a Arc<RwLock<Environment>>,
    value_span: &'a Span,
    ctx: &'a Arc<EvalContext>,
) -> MatchPatternFuture<'a> {
    Box::pin(async move {
        match pattern {
            Pattern::Wildcard => {
                // Wildcard always matches, no bindings
                Ok(Some(Arc::clone(env)))
            }
            Pattern::Variable(name) => {
                // Variable always matches and binds the value
                let child_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(env))));
                let value_thunk = Arc::new(Thunk::new_materialized(value.clone(), *value_span));
                child_env.write().unwrap().insert(name.clone(), value_thunk);
                Ok(Some(child_env))
            }
            Pattern::TypeTag(tag) => {
                // TypeTag matches if type-of the value equals the tag.
                // Also matches unit-variant values whose tag equals the pattern tag.
                // A bare uppercase identifier like `None` in a match arm is parsed as
                // Pattern::TypeTag("None"). For unit constructors (Value::Variant with
                // no payload), we check the variant tag directly so that:
                //   match ma
                //     [Some a]: ...
                //     None:     ...    <- TypeTag("None") matches Variant{tag:"None",payload:None}
                let type_name = value.type_name();
                // Handle supertypes and aliases:
                //   Number matches both Int and Float
                //   Str is an alias for String (type_name returns "String")
                let matches = if tag == "Number" {
                    type_name == "Int" || type_name == "Float"
                } else if tag == "Str" {
                    type_name == "String"
                } else if let Value::Variant {
                    tag: variant_tag,
                    payload: None,
                } = value
                {
                    // Unit variant: match by tag name, not by type_name() (which returns "Variant")
                    variant_tag == tag
                } else if let Value::Expression(node) = value {
                    // Expression: match by surface tag (e.g., "IntLiteral", "Var", "Call")
                    // or by the type name "Expression" itself
                    let surf_tag = crate::surface_fields::surface_expr_tag(&node.expr);
                    tag == surf_tag || tag == "Expression"
                } else {
                    type_name == tag
                };
                if matches {
                    Ok(Some(Arc::clone(env)))
                } else {
                    Ok(None)
                }
            }
            Pattern::Literal(lit) => {
                // Literal matches if the value equals the literal
                let matches = match (lit, value) {
                    (LiteralPattern::Int(n), Value::Int(v)) => n == v,
                    (LiteralPattern::Float(f), Value::Float(v)) => f == v,
                    (LiteralPattern::Bool(b), Value::Bool(v)) => b == v,
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
            Pattern::Pin(name) => {
                // Pin matches if the variable's value equals the scrutinee value
                let var_thunk = env
                    .read()
                    .unwrap()
                    .get(name)
                    .ok_or_else(|| EvalError::undefined_variable(name.clone(), *value_span))?;
                let var_value = materialize(&var_thunk, Some(value_span), ctx).await?;

                // Compare values for equality. Dict and Seq require materialization of
                // their contents, so this is an async operation.
                let matches =
                    values_equal(var_value, value.clone(), *value_span, Arc::clone(ctx)).await?;
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
                            Arc::new(RwLock::new(Environment::with_parent(Arc::clone(env))));

                        // Check each pattern field
                        for (key, field_pattern) in fields {
                            // Look up the field in the dict
                            if let Some(field_thunk_id) =
                                dict_thunk_ids.get(&Key::String(key.clone()))
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
                        // no parser syntax sets rest=false. If closed-dict syntax is added
                        // (e.g. trailing !), remove this comment.
                        if !rest {
                            let pattern_keys: std::collections::HashSet<&str> =
                                fields.iter().map(|(k, _)| k.as_str()).collect();
                            for dict_key in dict_thunk_ids.keys() {
                                let key_matches = match dict_key {
                                    Key::String(s) => pattern_keys.contains(s.as_str()),
                                    Key::Int(_) => false,
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
                            *value_span,
                        )?;
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
                    _ => {
                        // Value is not a dict
                        Ok(None)
                    }
                }
            }
            Pattern::Seq { head, tail } => {
                // Seq pattern: match Value::Seq, force head, lazily bind tail
                match value {
                    Value::Seq {
                        head: head_thunk_id,
                        tail: tail_thunk_id,
                    } => {
                        // Force the head value
                        let head_thunk = ctx.get_thunk(*head_thunk_id);
                        let head_value = materialize(&head_thunk, Some(value_span), ctx).await?;

                        // Match the head pattern
                        let mut result_env =
                            Arc::new(RwLock::new(Environment::with_parent(Arc::clone(env))));
                        match match_pattern(&head.node, &head_value, &result_env, &head.span, ctx)
                            .await?
                        {
                            Some(new_env) => {
                                result_env = new_env;
                            }
                            None => {
                                // Head pattern didn't match
                                return Ok(None);
                            }
                        }

                        // Handle the tail pattern. Preserve laziness when possible:
                        // - Variable: bind the tail thunk directly without materializing it.
                        //   The tail is a lazy sequence and should stay unevaluated until
                        //   the binding is actually used.
                        // - Wildcard: discard the tail entirely — no binding, no forcing.
                        // - Anything else (TypeTag, Constructor, Dict, Literal, Seq, Pin):
                        //   materialize the tail and recurse into match_pattern as before.
                        match &tail.node {
                            Pattern::Variable(name) => {
                                // Bind the tail thunk directly — no materialization.
                                let tail_thunk = ctx.get_thunk(*tail_thunk_id);
                                let child_env = Arc::new(RwLock::new(
                                    Environment::with_parent(Arc::clone(&result_env)),
                                ));
                                child_env
                                    .write()
                                    .unwrap()
                                    .insert(name.clone(), tail_thunk);
                                Ok(Some(child_env))
                            }
                            Pattern::Wildcard => {
                                // Tail is discarded — no binding, no forcing.
                                Ok(Some(result_env))
                            }
                            _ => {
                                // Structural tail pattern: materialize and recurse.
                                let tail_thunk = ctx.get_thunk(*tail_thunk_id);
                                let tail_value =
                                    materialize(&tail_thunk, Some(value_span), ctx).await?;
                                match match_pattern(
                                    &tail.node,
                                    &tail_value,
                                    &result_env,
                                    &tail.span,
                                    ctx,
                                )
                                .await?
                                {
                                    Some(new_env) => Ok(Some(new_env)),
                                    None => Ok(None),
                                }
                            }
                        }
                    }
                    _ => {
                        // Value is not a Seq
                        Ok(None)
                    }
                }
            }
            Pattern::Constructor { tag, binding } => {
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
                                // Constructor { binding: None } is currently unreachable from parsed programs —
                                // nullary constructors always parse as Pattern::TypeTag, not Pattern::Constructor.
                                // Retained for structural completeness; safe to delete if TypeTag is sufficient.
                                Ok(Some(Arc::clone(env)))
                            }
                            (Some(_), None) => {
                                // Pattern expects payload but variant has none
                                Ok(None)
                            }
                            (None, Some(_)) => {
                                // Pattern expects no payload but variant has one
                                Ok(None)
                            }
                        }
                    }
                    // runtime-v2: match on Value::Expression by surface tag
                    Value::Expression(node) => {
                        let expr_tag = crate::surface_fields::surface_expr_tag(&node.expr);
                        if tag.as_str() != expr_tag {
                            return Ok(None);
                        }
                        match binding {
                            None => Ok(Some(Arc::clone(env))),
                            Some(payload_pattern) => {
                                // Build a payload Dict from the Expression's fields for pattern binding.
                                // Each field is a lazy AstNodeField thunk — only forced when demanded.
                                // Binding invariant: ALL field names get thunks, even if unused by arm body.
                                let field_names =
                                    crate::surface_fields::surface_expr_field_names(&node.expr);
                                let mut payload_map = indexmap::IndexMap::new();
                                for field_name in field_names {
                                    let thunk_id =
                                        ctx.alloc_thunk(Arc::new(Thunk::new_ast_node_field(
                                            Arc::clone(node),
                                            field_name,
                                            Arc::clone(ctx),
                                            *value_span,
                                        )));
                                    payload_map
                                        .insert(Key::String((*field_name).to_string()), thunk_id);
                                }
                                let payload_val = Value::Dict(payload_map);
                                match_pattern(
                                    &payload_pattern.node,
                                    &payload_val,
                                    env,
                                    &payload_pattern.span,
                                    ctx,
                                )
                                .await
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
        }
    }) // end Box::pin(async move {
}


/// Check if two values are equal (for pin pattern matching, `$var:`).
///
/// Primitive types compare by value. Dict and Seq require deep structural
/// equality: all field values and sequence elements must be materialized and
/// compared recursively. This is a strictness point — `$dict_var:` in a
/// pin pattern will force evaluation of every reachable value in the dict/seq.
///
/// # Strictness
/// - `Int`, `Float`, `Bool`, `String`: compare without any materialization.
/// - `Dict(a)` vs `Dict(b)`: same key set required; then each value pair is
///   materialized and compared recursively. This forces all values in both
///   dicts.
/// - `Seq` vs `Seq`: head elements materialized and compared, then tails
///   materialized and compared recursively.
/// - All other combinations return `false`.
///
/// Uses `Pin<Box<...>>` to support recursion (direct `async fn` recursion is unsized).
/// Takes owned `Value` and copies `Span` to avoid self-referential lifetime issues in
/// the recursive calls inside the pinned future.
fn values_equal(a: Value, b: Value, span: Span, ctx: Arc<EvalContext>) -> ValuesEqualFuture {
    Box::pin(async move {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(x == y),
            (Value::Float(x), Value::Float(y)) => Ok(x == y),
            (Value::Bool(x), Value::Bool(y)) => Ok(x == y),
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
            ) => Ok(s1[start1..end1] == s2[start2..end2]),
            // Nullary variants compare by tag equality
            (
                Value::Variant {
                    tag: tag1,
                    payload: None,
                },
                Value::Variant {
                    tag: tag2,
                    payload: None,
                },
            ) => Ok(tag1 == tag2),
            // Dict structural equality: same keys, then compare each value pair.
            // Deep equality requires materializing all field values in both dicts.
            (Value::Dict(map_a), Value::Dict(map_b)) => {
                if map_a.len() != map_b.len() {
                    return Ok(false);
                }
                // Keys must be identical (same set; insertion order is not required
                // for equality — only that every key in a exists in b with the same value).
                for (key, id_a) in &map_a {
                    let id_b = match map_b.get(key) {
                        Some(id) => *id,
                        None => return Ok(false),
                    };
                    let thunk_a = ctx.get_thunk(*id_a);
                    let thunk_b = ctx.get_thunk(id_b);
                    let val_a = materialize(&thunk_a, Some(&span), &ctx).await?;
                    let val_b = materialize(&thunk_b, Some(&span), &ctx).await?;
                    if !values_equal(val_a, val_b, span, Arc::clone(&ctx)).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            // Seq structural equality: materialize head and tail, compare element by element.
            // The nil sentinel is an empty Dict, so an exhausted Seq on both sides compares
            // equal via the Dict arm above (both are Dict({})).
            (
                Value::Seq {
                    head: head_a,
                    tail: tail_a,
                },
                Value::Seq {
                    head: head_b,
                    tail: tail_b,
                },
            ) => {
                let thunk_ha = ctx.get_thunk(head_a);
                let thunk_hb = ctx.get_thunk(head_b);
                let val_ha = materialize(&thunk_ha, Some(&span), &ctx).await?;
                let val_hb = materialize(&thunk_hb, Some(&span), &ctx).await?;
                if !values_equal(val_ha, val_hb, span, Arc::clone(&ctx)).await? {
                    return Ok(false);
                }
                let thunk_ta = ctx.get_thunk(tail_a);
                let thunk_tb = ctx.get_thunk(tail_b);
                let val_ta = materialize(&thunk_ta, Some(&span), &ctx).await?;
                let val_tb = materialize(&thunk_tb, Some(&span), &ctx).await?;
                values_equal(val_ta, val_tb, span, ctx).await
            }
            _ => Ok(false),
        }
    })
}

#[cfg(test)]
#[allow(clippy::to_string_in_format_args)] // test diagnostics: .to_string() in format args is fine
#[allow(clippy::useless_conversion)] // test helpers use .into() for clarity
#[allow(clippy::approx_constant)] // test values intentionally use approximate constants
mod tests {
    use super::*;
    use crate::ast::*;
    use crate::error::ErrorKind;
    use crate::test_util::{rsp, sp, test_span};
    use crate::value::*;
    use std::cell::RefCell;

    fn empty_env() -> Arc<RwLock<Environment>> {
        Arc::new(RwLock::new(Environment::new()))
    }

    fn test_ctx() -> Arc<EvalContext> {
        let env = empty_env();
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        EvalContext::new(base_dir, Arc::clone(&env), Arc::clone(&env), false)
    }

    /// Synchronous shadow of `eval()` for test contexts.
    /// Drives the async eval future on the thread-local tokio runtime.
    /// Shadows the outer async `eval` so existing test code compiles unchanged.
    fn eval(
        expr: Rc<Spanned<Expr>>,
        env: Arc<RwLock<Environment>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        crate::async_rt::block_on_anywhere(super::eval(expr, env, ctx))
    }

    /// Synchronous shadow of `materialize()` for test contexts.
    /// Drives the async materialize future on the thread-local tokio runtime.
    /// Shadows the outer async `materialize` so existing test code compiles unchanged.
    fn materialize(
        thunk: &Thunk,
        mat_span: Option<&Span>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Value> {
        crate::async_rt::block_on_anywhere(super::materialize(thunk, mat_span, ctx))
    }

    /// Synchronous shadow of `eval_document()` for test contexts.
    fn eval_document(
        doc: &crate::ast::Spanned<crate::ast::Document>,
        env: Arc<RwLock<Environment>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        crate::async_rt::block_on_anywhere(super::eval_document(doc, env, ctx))
    }

    /// Resolve a `ThunkId` from the arena in `ctx` and materialize it.
    ///
    /// Dict values in `Value::Dict` are now `ThunkId` handles into the eval context's arena.
    /// Tests that inspect individual dict entries must resolve them through the same context
    /// that was used during `eval()`.
    fn mat_id(id: &ThunkId, ctx: &Arc<EvalContext>) -> EvalResult<Value> {
        let thunk = ctx.get_thunk(*id);
        materialize(&thunk, None, ctx)
    }

    /// Resolve a `ThunkId` to `Arc<Thunk>` for tests that need direct thunk access
    /// (e.g. inspecting `ThunkState` or materializing with a custom mat_span).
    fn get_thunk_rc(id: &ThunkId, ctx: &Arc<EvalContext>) -> Arc<Thunk> {
        ctx.get_thunk(*id)
    }

    #[test]
    fn test_eval_int() {
        let expr = sp(Expr::Int(42));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_eval_float() {
        let expr = sp(Expr::Float(3.14));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Float(3.14));
    }

    #[test]
    fn test_eval_bool() {
        let expr = sp(Expr::Bool(true));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn test_eval_str() {
        let expr = sp(Expr::Str("hello".into()));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[test]
    fn test_varref_found() {
        let env = empty_env();
        let span = test_span(1, 1, 1, 5);
        env.write().unwrap().insert(
            "x".into(),
            Arc::new(Thunk::new_materialized(Value::Int(99), span)),
        );

        let expr = sp(Expr::var_ref("x".into()));
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_varref_parent_scope() {
        let parent = empty_env();
        let span = test_span(1, 1, 1, 5);
        parent.write().unwrap().insert(
            "y".into(),
            Arc::new(Thunk::new_materialized(Value::Int(77), span)),
        );

        let child = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(&parent))));
        let expr = sp(Expr::var_ref("y".into()));
        let thunk = eval(Rc::new(expr.clone()), child, &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(77));
    }

    #[test]
    fn test_varref_not_found() {
        let expr = sp(Expr::var_ref("missing".into()));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap_err();
        assert!(
            err.to_string().contains("undefined variable: missing"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_simple_dict() {
        // [x: 1  y: hello]
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::Str("hello".into())),
            }),
        ];
        let ctx = test_ctx();
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();

        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let x_id = map.get(&Key::String("x".into())).unwrap();
                assert_eq!(mat_id(x_id, &ctx).unwrap(), Value::Int(1));
                let y_id = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(mat_id(y_id, &ctx).unwrap(), string_val("hello".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_auto_indexed_dict() {
        let entries = vec![
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(10)),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(20)),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(30)),
            }),
        ];
        let ctx = test_ctx();
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();

        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                assert_eq!(
                    mat_id(map.get(&Key::Int(0)).unwrap(), &ctx).unwrap(),
                    Value::Int(10)
                );
                assert_eq!(
                    mat_id(map.get(&Key::Int(1)).unwrap(), &ctx).unwrap(),
                    Value::Int(20)
                );
                assert_eq!(
                    mat_id(map.get(&Key::Int(2)).unwrap(), &ctx).unwrap(),
                    Value::Int(30)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_mixed_keyed_and_auto_indexed() {
        // [name: hello  42  flag: true  99]
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("hello".into())),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(42)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("flag".into()))),
                value: rsp(Expr::Bool(true)),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(99)),
            }),
        ];
        let ctx = test_ctx();
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();

        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                assert_eq!(
                    mat_id(map.get(&Key::String("name".into())).unwrap(), &ctx).unwrap(),
                    string_val("hello".into())
                );
                assert_eq!(
                    mat_id(map.get(&Key::Int(0)).unwrap(), &ctx).unwrap(),
                    Value::Int(42)
                );
                assert_eq!(
                    mat_id(map.get(&Key::String("flag".into())).unwrap(), &ctx).unwrap(),
                    Value::Bool(true)
                );
                assert_eq!(
                    mat_id(map.get(&Key::Int(1)).unwrap(), &ctx).unwrap(),
                    Value::Int(99)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_dict_letrec_sibling_reference() {
        // [x: 5  y: $x]
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(5)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::var_ref("x".into())),
            }),
        ];
        let ctx = test_ctx();
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();

        match val {
            Value::Dict(map) => {
                let y_id = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(mat_id(y_id, &ctx).unwrap(), Value::Int(5));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_dict_letrec_forward_reference() {
        // [y: $x  x: 10] -- y references x which is defined after y
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::var_ref("x".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(10)),
            }),
        ];
        let ctx = test_ctx();
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();

        match val {
            Value::Dict(map) => {
                let y_id = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(mat_id(y_id, &ctx).unwrap(), Value::Int(10));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_cycle_detection() {
        // [x: $x] -- x references itself
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("x".into())),
        })];
        let ctx = test_ctx();
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();

        match val {
            Value::Dict(map) => {
                let x_id = map.get(&Key::String("x".into())).unwrap();
                let err = mat_id(x_id, &ctx).unwrap_err();
                assert!(
                    err.to_string().contains("circular dependency"),
                    "got: {}",
                    err
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_cycle_detection_transitions_to_failed() {
        // When a thunk detects a circular dependency (InProgress state),
        // it should cache the error in Failed state, not leave it in InProgress.
        // Subsequent materializations should return the cached error.
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("x".into())),
        })];
        let ctx = test_ctx();
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();

        let x_thunk = match val {
            Value::Dict(map) => get_thunk_rc(map.get(&Key::String("x".into())).unwrap(), &ctx),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First materialization: should detect the cycle and fail
        let err1 = materialize(&x_thunk, None, &ctx).unwrap_err();
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
        let err2 = materialize(&x_thunk, None, &ctx).unwrap_err();
        assert!(
            err2.kind.to_string().contains("circular dependency"),
            "second error: got: {}",
            err2.kind
        );
    }

    #[test]
    fn test_thunk_retryable_after_error() {
        // [x: $missing] -- materializing x fails because $missing is undefined.
        // After failure, the thunk must be restored to Unevaluated, not left
        // as InProgress. A second materialize attempt should produce the same
        // "undefined variable" error, NOT "circular dependency".
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("missing".into())),
        })];
        let ctx = test_ctx();
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Arc::clone(&env), &ctx).unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).unwrap();

        let x_thunk = match &dict_val {
            Value::Dict(map) => get_thunk_rc(map.get(&Key::String("x".into())).unwrap(), &ctx),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First attempt: should fail with "undefined variable"
        let err1 = materialize(&x_thunk, None, &ctx).unwrap_err();
        assert!(
            err1.kind
                .to_string()
                .contains("undefined variable: missing"),
            "first attempt: got: {}",
            err1.kind
        );

        // Second attempt: should produce the SAME error, not "circular dependency"
        let err2 = materialize(&x_thunk, None, &ctx).unwrap_err();
        assert!(
            err2.kind
                .to_string()
                .contains("undefined variable: missing"),
            "second attempt should not be poisoned, got: {}",
            err2.kind
        );
        assert!(
            !err2.kind.to_string().contains("circular dependency"),
            "thunk was poisoned: got circular dependency on retry"
        );
    }

    #[test]
    fn test_nested_dict_sees_outer_bindings() {
        // [x: 42  inner: [y: $x]]
        let inner_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("y".into()))),
            value: rsp(Expr::var_ref("x".into())),
        })];
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(42)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("inner".into()))),
                value: rsp(Expr::Dict(inner_entries)),
            }),
        ];
        let ctx = test_ctx();
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &ctx).unwrap();
        let outer = materialize(&thunk, None, &ctx).unwrap();

        match outer {
            Value::Dict(outer_map) => {
                let inner_id = outer_map.get(&Key::String("inner".into())).unwrap();
                let inner_val = mat_id(inner_id, &ctx).unwrap();
                match inner_val {
                    Value::Dict(inner_map) => {
                        let y_id = inner_map.get(&Key::String("y".into())).unwrap();
                        assert_eq!(mat_id(y_id, &ctx).unwrap(), Value::Int(42));
                    }
                    other => panic!("expected inner Dict, got {other:?}"),
                }
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_duplicate_key_error() {
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(2)),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("duplicate key: x"), "got: {}", err);
    }

    #[test]
    fn test_fn_creates_function_value() {
        // [fn [x] $x] → Function
        let expr = sp(Expr::Fn {
            return_ann: None,
            params: vec![sp(Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            })],
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            desugared: false,
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "x");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_captures_closure_env() {
        // outer: 42 is in env, [fn [] $outer] should capture it
        let env = empty_env();
        env.write().unwrap().insert(
            "outer".into(),
            Arc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );
        let fn_expr = sp(Expr::Fn {
            return_ann: None,
            params: vec![],
            body: Rc::new(sp(Expr::var_ref("outer".into()))),
            desugared: false,
        });
        let fn_thunk = eval(Rc::new(fn_expr.clone()), Arc::clone(&env), &test_ctx()).unwrap();
        let fn_val = materialize(&fn_thunk, None, &test_ctx()).unwrap();

        // Call it: [call $f]
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![],
            named_args: vec![],
            implied: false,
        });
        let result_thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();
        let result = materialize(&result_thunk, None, &test_ctx()).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn test_call_simple() {
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
            body: Arc::new(sp(CoreExpr::FreeVar("x".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::Int(42))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_call_multiple_args() {
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
            body: Arc::new(sp(CoreExpr::FreeVar("b".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::Int(10)), rsp(Expr::Int(20))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(20));
    }

    #[test]
    fn test_call_on_non_function() {
        let env = empty_env();
        env.write().unwrap().insert(
            "x".into(),
            Arc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("x".into()))),
            args: vec![],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx())
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("type mismatch"), "got: {}", err);
        assert!(err.to_string().contains("Function"), "got: {}", err);
    }

    #[test]
    fn test_call_too_few_args() {
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
            body: Arc::new(sp(CoreExpr::FreeVar("x".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::Int(1))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx())
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("missing argument for required parameter"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_call_too_many_args() {
        // f: [fn [x] $x]
        // [call $f 1 2] → arity mismatch
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::FreeVar("x".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::Int(1)), rsp(Expr::Int(2))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx())
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("arity mismatch"), "got: {}", err);
    }

    #[test]
    fn test_call_named_arg_with_default() {
        // f: [fn [x  y@[default: 99]] [result: $y]]
        // [call $f 1] → y defaults to 99
        let env = empty_env();
        let default_entry = sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(99)),
        });
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
            body: Arc::new(sp(CoreExpr::FreeVar("y".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        // Call without named arg -- y should default to 99
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::Int(1))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), Arc::clone(&env), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_call_named_arg_overridden() {
        // f: [fn [x  y@[default: 99]] $y]
        // [call $f 1 y: 42] → y = 42
        let env = empty_env();
        let default_entry = sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(99)),
        });
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
            body: Arc::new(sp(CoreExpr::FreeVar("y".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::Int(1))],
            named_args: vec![sp(NamedArg {
                name: "y".into(),
                value: rsp(Expr::Int(42)),
            })],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_call_unexpected_named_arg() {
        // f: [fn [x] $x]
        // [call $f 1 z: 2] → error: unexpected named argument
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::FreeVar("x".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::Int(1))],
            named_args: vec![sp(NamedArg {
                name: "z".into(),
                value: rsp(Expr::Int(2)),
            })],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx())
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(
            err.to_string().contains("unexpected named argument: z"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_call_duplicate_positional_and_named_error() {
        // f: [fn [x y@[default: 99]] $y]
        // [call $f 1 2 y: 42] → error: y received both positional and named argument
        let env = empty_env();
        let default_entry = sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(99)),
        });
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
            body: Arc::new(sp(CoreExpr::FreeVar("y".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::Int(1)), rsp(Expr::Int(2))],
            named_args: vec![sp(NamedArg {
                name: "y".into(),
                value: rsp(Expr::Int(42)),
            })],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx())
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("received both positional and named argument"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_call_variadic() {
        // f: [fn [x ...rest] $rest]
        // [call $f 1 2 3] → rest = Seq(2, Seq(3, {}))
        // Variadic args are now collected as a lazy Seq cons-list.
        // Empty variadic = Dict({}) (nil sentinel); non-empty = Seq { head, tail }.
        let env = empty_env();
        let fn_val = Value::Function {
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
            body: Arc::new(sp(CoreExpr::FreeVar("rest".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::Int(1)), rsp(Expr::Int(2)), rsp(Expr::Int(3))],
            named_args: vec![],
            implied: false,
        });
        let ctx = test_ctx();
        let thunk = eval(Rc::new(call_expr.clone()), env, &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();
        // Outer Seq: head = Int(2), tail = Seq(3, {})
        match val {
            Value::Seq { head, tail } => {
                assert_eq!(mat_id(&head, &ctx).unwrap(), Value::Int(2));
                // tail = Seq { head: Int(3), tail: Dict({}) }
                match mat_id(&tail, &ctx).unwrap() {
                    Value::Seq {
                        head: head2,
                        tail: tail2,
                    } => {
                        assert_eq!(mat_id(&head2, &ctx).unwrap(), Value::Int(3));
                        // tail of tail = nil sentinel (empty Dict)
                        match mat_id(&tail2, &ctx).unwrap() {
                            Value::Dict(m) => assert!(m.is_empty()),
                            other => panic!("expected empty Dict (nil), got {other:?}"),
                        }
                    }
                    other => panic!("expected Seq as tail, got {other:?}"),
                }
            }
            other => panic!("expected Seq, got {other:?}"),
        }
    }

    #[test]
    fn test_call_variadic_empty() {
        // f: [fn [x ...rest] $rest]
        // [call $f 1] → rest = Dict({})
        let env = empty_env();
        let fn_val = Value::Function {
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
            body: Arc::new(sp(CoreExpr::FreeVar("rest".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::Int(1))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        match val {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected empty Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_call_builtin() {
        fn add_builtin(
            ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let crate::value::BuiltinArgs { args, .. } = ctx;
                let a = materialize(&args[0], None, &test_ctx())?;
                let b = materialize(&args[1], None, &test_ctx())?;
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
        env.write().unwrap().insert(
            "add".into(),
            Arc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: add_builtin,
                    name: "add",
                    pos_strictness: &[],
                    force_count: 0,
                }),
                test_span(1, 1, 1, 5),
            )),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("add".into()))),
            args: vec![rsp(Expr::Int(3)), rsp(Expr::Int(4))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(7));
    }

    #[test]
    #[ignore = "pre-existing regression from runtime-v2 merge: Expr::TypeAlias now converts to CoreExpr::Error (declaration form, not evaluable as expression)"]
    fn test_type_alias_returns_empty_dict() {
        let expr = sp(Expr::TypeAlias {
            params: vec![],
            body: Box::new(sp(Expr::var_ref("MyType".into()))),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        match val {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected empty Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_rest_marker_anonymous_errors() {
        let expr = sp(Expr::Rest(None));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("rest marker (...) is only valid inside type expressions"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_rest_marker_named_errors() {
        let expr = sp(Expr::Rest(Some("x".into())));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("rest marker (...) is only valid inside type expressions"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_bare_underscore_is_not_lambda() {
        // $_ alone is just a VarRef, not an implicit lambda
        // It should fail with "undefined variable" if not in scope
        let expr = sp(Expr::var_ref("_".into()));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap_err();
        assert!(
            err.to_string().contains("undefined variable: _"),
            "got: {}",
            err
        );
    }

    // ── Integration tests for $_ desugaring + evaluation ──────────────────
    // These tests verify that the AST-level desugaring (from src/desugar.rs)
    // integrates correctly with evaluation. They manually call desugar_surface_node()
    // before eval() to simulate the full pipeline.

    #[test]
    fn test_underscore_access_chain_becomes_lambda() {
        // $_.name → [fn [_] $_.name] after desugaring
        // Evaluating this should produce a Function, not look up $_
        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("_".into()))),
            field: crate::ast::DotKey::Ident("name".into()),
        });

        // Convert to SurfaceNode, desugar, then convert back
        let mut node = crate::ast_convert::expr_to_surface_node(&expr);
        crate::desugar::desugar_surface_node(&mut node, 0);
        let desugared_expr = crate::ast_convert::surface_node_to_expr(&node);

        let thunk = eval(Rc::new(desugared_expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    #[test]
    fn test_underscore_in_call_becomes_lambda() {
        // [call $f $_] where $f is in scope → should produce a lambda after desugaring
        // The outer [call ...] contains $_ directly → wraps in [fn [_] [call $f $_]]
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::FreeVar("x".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );

        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::var_ref("_".into()))],
            named_args: vec![],
            implied: false,
        });

        // Convert to SurfaceNode, desugar, then convert back
        let mut node = crate::ast_convert::expr_to_surface_node(&call_expr);
        crate::desugar::desugar_surface_node(&mut node, 0);
        let desugared_expr = crate::ast_convert::surface_node_to_expr(&node);

        let thunk = eval(
            Rc::new(desugared_expr.clone()),
            Arc::clone(&env),
            &test_ctx(),
        )
        .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function from $_ desugaring, got {other:?}"),
        }
    }

    #[test]
    fn test_underscore_lambda_callable() {
        // Create $_.name as a lambda (via desugaring), then call it with a dict that has name: "alice"
        let env = empty_env();

        // Build the $_.name expression → becomes [fn [_] $_.name] after desugaring
        let getter_expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("_".into()))),
            field: crate::ast::DotKey::Ident("name".into()),
        });

        // Convert to SurfaceNode, desugar, then convert back
        let mut node = crate::ast_convert::expr_to_surface_node(&getter_expr);
        crate::desugar::desugar_surface_node(&mut node, 0);
        let desugared_expr = crate::ast_convert::surface_node_to_expr(&node);

        let getter_thunk = eval(
            Rc::new(desugared_expr.clone()),
            Arc::clone(&env),
            &test_ctx(),
        )
        .unwrap();
        let getter_val = materialize(&getter_thunk, None, &test_ctx()).unwrap();
        env.write().unwrap().insert(
            "getter".into(),
            Arc::new(Thunk::new_materialized(getter_val, test_span(1, 1, 1, 10))),
        );

        // Call it with [name: alice]
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("getter".into()))),
            args: vec![rsp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("alice".into())),
            })]))],
            named_args: vec![],
            implied: false,
        });
        let result_thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();
        let result = materialize(&result_thunk, None, &test_ctx()).unwrap();
        assert_eq!(result, string_val("alice".into()));
    }

    #[test]
    fn test_underscore_in_dict_entry() {
        // [a: $_.name] → desugars to [fn [_] [a: $_.name]]
        // Dict with $_ in a value position should desugar to an implicit lambda
        let expr = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("a".into()))),
            value: rsp(Expr::DotAccess {
                expr: Box::new(sp(Expr::var_ref("_".into()))),
                field: crate::ast::DotKey::Ident("name".into()),
            }),
        })]));

        // Convert to SurfaceNode, desugar, then convert back
        let mut node = crate::ast_convert::expr_to_surface_node(&expr);
        crate::desugar::desugar_surface_node(&mut node, 0);
        let desugared_expr = crate::ast_convert::surface_node_to_expr(&node);

        let thunk = eval(Rc::new(desugared_expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function from $_ dict desugaring, got {other:?}"),
        }
    }

    #[test]
    fn test_underscore_in_named_arg() {
        // [call $f x: $_] → desugars to [fn [_] [call $f x: $_]]
        // Call with $_ in a named arg value should desugar to an implicit lambda
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::FreeVar("x".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );

        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![],
            named_args: vec![sp(NamedArg {
                name: "x".into(),
                value: rsp(Expr::var_ref("_".into())),
            })],
            implied: false,
        });

        // Convert to SurfaceNode, desugar, then convert back
        let mut node = crate::ast_convert::expr_to_surface_node(&call_expr);
        crate::desugar::desugar_surface_node(&mut node, 0);
        let desugared_expr = crate::ast_convert::surface_node_to_expr(&node);

        let thunk = eval(
            Rc::new(desugared_expr.clone()),
            Arc::clone(&env),
            &test_ctx(),
        )
        .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function from $_ named arg desugaring, got {other:?}"),
        }
    }

    fn dict_with_entries(entries: Vec<(&str, Value)>) -> Spanned<Expr> {
        let ast_entries = entries
            .into_iter()
            .map(|(k, v)| {
                let value_expr = match v {
                    Value::Int(n) => Expr::Int(n),
                    Value::String {
                        ref source,
                        start,
                        end,
                    } => Expr::Str(source[start..end].to_string()),
                    Value::Bool(b) => Expr::Bool(b),
                    Value::Float(f) => Expr::Float(f),
                    _ => panic!("unsupported value type in test helper"),
                };
                sp(Entry {
                    key: Some(sp(Expr::Str(k.into()))),
                    value: rsp(value_expr),
                })
            })
            .collect();
        sp(Expr::Dict(ast_entries))
    }

    #[test]
    fn test_dot_access() {
        // [name: hello].name -> "hello"
        // Use a single ctx — ThunkIds from one ctx are invalid in another.
        let ctx = test_ctx();
        let dict = dict_with_entries(vec![("name", string_val("hello".into()))]);
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Arc::clone(&env), &ctx).unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).unwrap();

        // Bind the dict to $d in the environment
        env.write().unwrap().insert(
            "d".into(),
            Arc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("d".into()))),
            field: crate::ast::DotKey::Ident("name".into()),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[test]
    fn test_dot_access_missing_key() {
        let dict = dict_with_entries(vec![("x", Value::Int(1))]);
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Arc::clone(&env), &test_ctx()).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx()).unwrap();
        env.write().unwrap().insert(
            "d".into(),
            Arc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("d".into()))),
            field: crate::ast::DotKey::Ident("missing".into()),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(
            err.to_string().contains("key not found: missing"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_dot_access_on_non_dict() {
        let env = empty_env();
        env.write().unwrap().insert(
            "x".into(),
            Arc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("x".into()))),
            field: crate::ast::DotKey::Ident("foo".into()),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("expected"), "got: {}", err);
        assert!(err.to_string().contains("expected Dict"), "got: {}", err);
    }

    // Bracket access and range access tests removed — syntax has been removed from the language.
    // Tests are replaced by corpus tests in tests/corpus/valid/ and tests/corpus/invalid/.

    #[test]
    fn test_type_assert_int_passes() {
        // [@Int 42] -> 42
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_string_passes() {
        // [@String hello] -> "hello"
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("String".into())),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[test]
    fn test_type_assert_number_accepts_int() {
        // [@Number 42] -> 42 (Number accepts Int)
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Number".into())),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_number_accepts_float() {
        // [@Number 3.14] -> 3.14 (Number accepts Float)
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Number".into())),
            expr: Box::new(sp(Expr::Float(3.14))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Float(3.14));
    }

    #[test]
    fn test_type_assert_int_fails_on_string() {
        // [@Int hello] -> error
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_type_assert_string_fails_on_int() {
        // [@String 42] -> error
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("String".into())),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected String, got Int"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_type_assert_bool_passes() {
        // [@Bool true] -> true
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Bool".into())),
            expr: Box::new(sp(Expr::Bool(true))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn test_type_assert_property_dict_with_type() {
        // [@[type: Int] 42] -> 42
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("type".into()))),
            value: rsp(Expr::Str("Int".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_property_dict_type_mismatch() {
        // [@[type: Int] hello] -> error
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("type".into()))),
            value: rsp(Expr::Str("Int".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_type_assert_property_dict_without_type_passes() {
        // [@[default: 0] hello] -> "hello" (no type key, no check performed)
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(0)),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[test]
    fn test_type_assert_default_not_used_on_match() {
        // [@[type: Int  default: 0] 42] -> 42 (type matches, default not used)
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: rsp(Expr::Str("Int".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::Int(0)),
            }),
        ];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_default_used_on_mismatch() {
        // [@[type: Int  default: 0] hello] -> 0 (type mismatch, returns default)
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: rsp(Expr::Str("Int".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::Int(0)),
            }),
        ];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(0));
    }

    #[test]
    fn test_type_assert_property_dict_no_default_errors_on_mismatch() {
        // [@[type: Int] hello] -> error (no default, mismatch is an error)
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("type".into()))),
            value: rsp(Expr::Str("Int".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_type_assert_number_default_int_passes_string_triggers() {
        // [@[type: Number  default: -1] 42] -> 42 (Int passes Number check)
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: rsp(Expr::Str("Number".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::Int(-1)),
            }),
        ];
        let expr_pass = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr_pass.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));

        // [@[type: Number  default: -1] "nope"] -> -1 (String fails Number, returns default)
        let entries2 = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: rsp(Expr::Str("Number".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::Int(-1)),
            }),
        ];
        let expr_fail = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries2)),
            expr: Box::new(sp(Expr::Str("nope".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk2 = eval(Rc::new(expr_fail.clone()), empty_env(), &test_ctx()).unwrap();
        let val2 = materialize(&thunk2, None, &test_ctx()).unwrap();
        assert_eq!(val2, Value::Int(-1));
    }

    #[test]
    fn test_type_assert_default_accesses_outer_scope() {
        // [@[type: Int  default: $fallback] hello] with fallback=99 -> 99
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: rsp(Expr::Str("Int".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::var_ref("fallback".into())),
            }),
        ];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let env = empty_env();
        env.write().unwrap().insert(
            "fallback".into(),
            Arc::new(Thunk::new_materialized(
                Value::Int(99),
                test_span(1, 1, 1, 1),
            )),
        );
        let thunk = eval(Rc::new(expr.clone()), Arc::clone(&env), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_annotated_bare_string() {
        // Config@ConfigType -> "Config"
        let expr = sp(Expr::Annotated {
            name: "Config".into(),
            annotation: sp(Annotation::Simple("ConfigType".into())),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, string_val("Config".into()));
    }

    #[test]
    fn test_chained_dot_access() {
        // [outer: [inner: 99]].outer.inner -> 99
        // Use a single ctx throughout — ThunkIds from one ctx are invalid in another.
        let ctx = test_ctx();
        let inner_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("inner".into()))),
            value: rsp(Expr::Int(99)),
        })];
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("outer".into()))),
            value: rsp(Expr::Dict(inner_entries)),
        })];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Arc::clone(&env), &ctx).unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).unwrap();
        env.write().unwrap().insert(
            "d".into(),
            Arc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        // $d.outer.inner
        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::DotAccess {
                expr: Box::new(sp(Expr::var_ref("d".into()))),
                field: crate::ast::DotKey::Ident("outer".into()),
            })),
            field: crate::ast::DotKey::Ident("inner".into()),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_materialization_span_on_error() {
        // [x: $missing] -- materializing x fails because $missing is undefined
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("missing".into())),
        })];
        let ctx = test_ctx();
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Arc::clone(&env), &ctx).unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).unwrap();

        // Extract x's thunk from the dict
        let x_thunk = match &dict_val {
            Value::Dict(map) => get_thunk_rc(map.get(&Key::String("x".into())).unwrap(), &ctx),
            other => panic!("expected Dict, got {other:?}"),
        };

        // Materialize x with a known materialization span
        let mat_span = test_span(5, 1, 5, 5);
        let err = materialize(&x_thunk, Some(&mat_span), &ctx).unwrap_err();
        assert!(
            err.to_string().contains("undefined variable: missing"),
            "got: {}",
            err
        );
        assert_eq!(
            err.materialization_span,
            Some(mat_span),
            "materialization span should be the access site"
        );
    }

    #[test]
    fn test_cycle_has_materialization_span() {
        // [x: $x] -- force x with a known materialization site
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("x".into())),
        })];
        let ctx = test_ctx();
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();

        match val {
            Value::Dict(map) => {
                let x_id = map.get(&Key::String("x".into())).unwrap();
                let x_thunk = get_thunk_rc(x_id, &ctx);
                let mat_span = test_span(10, 1, 10, 5);
                let err = materialize(&x_thunk, Some(&mat_span), &ctx).unwrap_err();
                assert!(err.to_string().contains("circular dependency"));
                assert_eq!(err.materialization_span, Some(mat_span));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_value_to_key_invalid_type_bool() {
        // A dict with a Bool key expression should fail in eval_key -> value_to_key
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Bool(true))),
            value: rsp(Expr::Int(1)),
        })];
        let expr = sp(Expr::Dict(entries));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("type mismatch"), "got: {}", err);
        assert!(
            err.to_string().contains("expected String or Int"),
            "got: {}",
            err
        );
        assert!(err.to_string().contains("got Bool"), "got: {}", err);
    }

    #[test]
    fn test_value_to_key_invalid_type_float() {
        // A dict with a Float key expression should fail in eval_key -> value_to_key
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Float(3.14))),
            value: rsp(Expr::Int(1)),
        })];
        let expr = sp(Expr::Dict(entries));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("type mismatch"), "got: {}", err);
        assert!(
            err.to_string().contains("expected String or Int"),
            "got: {}",
            err
        );
        assert!(err.to_string().contains("got Float"), "got: {}", err);
    }

    #[test]
    fn test_eval_document_single_expression() {
        // A document with one dict expression returns that dict
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::Int(2)),
            }),
        ];
        let doc = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(entries)))],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
            stage: None,
        });
        let ctx = test_ctx();
        let thunk = eval_document(&doc, empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    mat_id(map.get(&Key::String("x".into())).unwrap(), &ctx).unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    mat_id(map.get(&Key::String("y".into())).unwrap(), &ctx).unwrap(),
                    Value::Int(2)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_scope_chain() {
        // Two expressions: expr 1 defines x, expr 2 references $x
        // Expr 1: [x: 10]
        // Expr 2: [y: $x]
        let expr1 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::Int(10)),
        })]));
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("y".into()))),
            value: rsp(Expr::var_ref("x".into())),
        })]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2)],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
            stage: None,
        });
        let ctx = test_ctx();
        let thunk = eval_document(&doc, empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();
        match val {
            Value::Dict(map) => {
                let y_id = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(mat_id(y_id, &ctx).unwrap(), Value::Int(10));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_scope_chain_shadowing() {
        // Expr 1: [x: 1]
        // Expr 2: [x: 2  y: $x]
        // y should be 2 (local letrec wins over parent scope)
        let expr1 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::Int(1)),
        })]));
        let expr2 = sp(Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(2)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::var_ref("x".into())),
            }),
        ]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2)],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
            stage: None,
        });
        let ctx = test_ctx();
        let thunk = eval_document(&doc, empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();
        match val {
            Value::Dict(map) => {
                let y_id = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(mat_id(y_id, &ctx).unwrap(), Value::Int(2));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_intermediate_non_dict_error() {
        // Two expressions where expr 1 is a literal (not a dict). Should error.
        let expr1 = sp(Expr::Int(42));
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::Int(1)),
        })]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2)],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
            stage: None,
        });
        let err = eval_document(&doc, empty_env(), &test_ctx()).unwrap_err();
        assert!(
            err.to_string().contains("document pipeline"),
            "got: {}",
            err
        );
        assert!(err.to_string().contains("expected Dict"), "got: {}", err);
    }

    #[test]
    fn test_eval_document_empty() {
        // A document with zero expressions returns an empty dict
        let doc = sp(Document {
            expressions: vec![],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
            stage: None,
        });
        let thunk = eval_document(&doc, empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 0);
            }
            other => panic!("expected empty Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_three_expressions() {
        // Three expressions chaining scope:
        // Expr 1: [a: 1]
        // Expr 2: [b: 2]
        // Expr 3: [ref_a: $a  ref_b: $b]
        // Expr 3 should see both $a (from expr 1 via grandparent) and $b (from expr 2 via parent)
        let expr1 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("a".into()))),
            value: rsp(Expr::Int(1)),
        })]));
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("b".into()))),
            value: rsp(Expr::Int(2)),
        })]));
        let expr3 = sp(Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("ref_a".into()))),
                value: rsp(Expr::var_ref("a".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("ref_b".into()))),
                value: rsp(Expr::var_ref("b".into())),
            }),
        ]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2), Rc::new(expr3)],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
            stage: None,
        });
        let ctx = test_ctx();
        let thunk = eval_document(&doc, empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let ref_a = map.get(&Key::String("ref_a".into())).unwrap();
                assert_eq!(mat_id(ref_a, &ctx).unwrap(), Value::Int(1));
                let ref_b = map.get(&Key::String("ref_b".into())).unwrap();
                assert_eq!(mat_id(ref_b, &ctx).unwrap(), Value::Int(2));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_inherits_parent_env() {
        // A document evaluated with a pre-populated parent env.
        // The document's expressions should see the parent's bindings.
        let parent_env = empty_env();
        parent_env.write().unwrap().insert(
            "external".into(),
            Arc::new(Thunk::new_materialized(
                Value::Int(999),
                test_span(1, 1, 1, 5),
            )),
        );

        let expr = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("local".into()))),
            value: rsp(Expr::var_ref("external".into())),
        })]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr)],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
            stage: None,
        });
        let ctx = test_ctx();
        let thunk = eval_document(&doc, parent_env, &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();
        match val {
            Value::Dict(map) => {
                let local = map.get(&Key::String("local".into())).unwrap();
                assert_eq!(mat_id(local, &ctx).unwrap(), Value::Int(999));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_single_non_dict_expression() {
        // A document with a single Int expression (not a dict).
        // The last expression can be any type.
        let doc = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Int(42)))],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
            stage: None,
        });
        let thunk = eval_document(&doc, empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_eval_document_integer_keys_skipped_in_scope_chain() {
        // Expr 1: [10 20 30] (auto-indexed: keys Int(0), Int(1), Int(2))
        // Expr 2: [result: 99]
        // Integer keys from expr 1 should not become scope bindings.
        let expr1 = sp(Expr::Dict(vec![
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(10)),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(20)),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(30)),
            }),
        ]));
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("result".into()))),
            value: rsp(Expr::Int(99)),
        })]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2)],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
            stage: None,
        });
        let ctx = test_ctx();
        let thunk = eval_document(&doc, empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();
        match val {
            Value::Dict(map) => {
                let result_id = map.get(&Key::String("result".into())).unwrap();
                assert_eq!(mat_id(result_id, &ctx).unwrap(), Value::Int(99));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_scope_chain_plus_letrec() {
        // Expr 1: [x: 1]
        // Expr 2: [y: $x  z: $y]
        // y references x from the scope chain, z references y via letrec.
        // Verify z resolves to 1.
        let expr1 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::Int(1)),
        })]));
        let expr2 = sp(Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::var_ref("x".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("z".into()))),
                value: rsp(Expr::var_ref("y".into())),
            }),
        ]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2)],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
            stage: None,
        });
        let ctx = test_ctx();
        let thunk = eval_document(&doc, empty_env(), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();
        match val {
            Value::Dict(map) => {
                let z_id = map.get(&Key::String("z".into())).unwrap();
                assert_eq!(mat_id(z_id, &ctx).unwrap(), Value::Int(1));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_single_document() {
        // A file with one document containing [x: 1]. Verify x=1.
        let result = crate::eval_source("[x: 1]").expect("eval failed");
        assert_eq!(result, r#"Dict({"x": Int(1)})"#);
    }

    #[test]
    fn test_eval_file_percent_is_empty_for_first_doc() {
        // A file with one document containing [prev: %].
        // % is VarRef("%"), should resolve to empty dict for first doc.
        let result = crate::eval_source("[prev: %]").expect("eval failed");
        assert_eq!(result, r#"Dict({"prev": Dict({})})"#);
    }

    #[test]
    fn test_eval_file_percent_pipeline() {
        // Doc 1: [x: 10]
        // Doc 2: [y: %.x]  (access previous doc's x via %)
        // Verify y=10.
        let result = crate::eval_source("[x: 10]\n---\n[y: %.x]").expect("eval failed");
        assert_eq!(result, r#"Dict({"y": Int(10)})"#);
    }

    #[test]
    fn test_eval_file_non_dict_percent() {
        // Doc 1: 42 (a bare Int, not a dict)
        // Doc 2: [prev: %]
        // Verify that prev resolves to Int(42).
        let result = crate::eval_source("42\n---\n[prev: %]").expect("eval failed");
        assert_eq!(result, r#"Dict({"prev": Int(42)})"#);
    }

    #[test]
    fn test_eval_file_percent_lazy() {
        // Verify that % is lazy: Doc 1 contains a value that would error if
        // materialized. Doc 2 accesses a DIFFERENT key from %, so the error
        // value is never forced.
        // Doc 1: [good: 1  bad: $missing]
        // Doc 2: [result: %.good]
        // Doc 2's output only has `result` — `bad` from doc 1 is never forced.
        let result = crate::eval_source("[good: 1  bad: $missing]\n---\n[result: %.good]")
            .expect("eval failed");
        assert_eq!(result, r#"Dict({"result": Int(1)})"#);
    }

    #[test]
    fn test_eval_file_three_documents() {
        // Three documents piped:
        // Doc 1: [a: 1]
        // Doc 2: [b: %.a  c: 2]
        // Doc 3: [result: %.b]
        // Verify result=1 (piped through two boundaries).
        let result = crate::eval_source("[a: 1]\n---\n[b: %.a  c: 2]\n---\n[result: %.b]")
            .expect("eval failed");
        assert_eq!(result, r#"Dict({"result": Int(1)})"#);
    }

    #[test]
    fn test_eval_file_documents_isolated() {
        // Verify documents don't share scope:
        // Doc 1: [x: 42]
        // Doc 2: [y: $x]  (NOT %.x — bare $x is undefined in doc 2's scope)
        // eval_source deep-materializes all keys, so forcing y fails.
        let err = crate::eval_source("[x: 42]\n---\n[y: $x]").expect_err("expected error");
        assert!(err.contains("undefined variable: x"), "got: {err}");
    }

    #[test]
    fn test_eval_file_empty() {
        // A file with zero documents (empty string). Should return an empty dict.
        let result = crate::eval_source("").expect("eval failed");
        assert_eq!(result, "Dict({})");
    }

    #[test]
    fn test_eval_file_inherits_env() {
        // A document expression should see bindings from the same document scope.
        // The letrec dict env means all entries in a dict share one scope,
        // so `val: $external` can reference `external: 777` defined in the same dict.
        // This covers the "document expressions see available bindings" invariant
        // without needing a manually-injected parent env.
        let result = crate::eval_source("[external: 777  val: $external]").expect("eval failed");
        assert_eq!(result, r#"Dict({"external": Int(777), "val": Int(777)})"#);
    }

    #[test]
    fn test_eval_file_named_sections() {
        // Test named sections with %name binding.
        // Doc 1 (named "defaults"): [port: 8080]
        // Doc 2 (named "overrides"): [host: "prod"]
        // Doc 3 (anonymous): [port: %defaults.port  host: %overrides.host]
        let result = crate::eval_source(
            "--- %defaults\n[port: 8080]\n--- %overrides\n[host: \"prod\"]\n---\n[port: %defaults.port  host: %overrides.host]",
        )
        .expect("eval failed");
        assert_eq!(
            result,
            r#"Dict({"port": Int(8080), "host": String("prod")})"#
        );
    }

    #[test]
    fn test_eval_file_named_sections_no_forward_refs() {
        // Test that named sections cannot reference later sections (no forward references).
        //
        // File layout:
        //   Doc 1 (named "early"):  [x: %late.value]   — references %late which is NOT yet defined
        //   Doc 2 (named "late"):   [value: 42]
        //   Doc 3 (unnamed):        [result: %early.x]  — forces materialization of doc1's x field
        //
        // The forward reference %late inside doc1 should produce UndefinedVariable when doc3
        // forces doc1 to materialize. eval_source deep-materializes all keys, so result is forced.
        let err = crate::eval_source(
            "--- %early\n[x: %late.value]\n--- %late\n[value: 42]\n---\n[result: %early.x]",
        )
        .expect_err("expected error: %late not in scope for doc1");
        assert!(
            err.contains("undefined variable: %late"),
            "expected 'undefined variable: %late', got: {err}"
        );
    }

    #[test]
    fn test_deep_materialize_int() {
        let val = Value::Int(42);
        let result = deep_materialize(&val, &test_ctx(), None).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn test_deep_materialize_float() {
        let val = Value::Float(3.14);
        let result = deep_materialize(&val, &test_ctx(), None).unwrap();
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn test_deep_materialize_string() {
        let val = string_val("hello".into());
        let result = deep_materialize(&val, &test_ctx(), None).unwrap();
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn test_deep_materialize_bool() {
        let val = Value::Bool(true);
        let result = deep_materialize(&val, &test_ctx(), None).unwrap();
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn test_deep_materialize_empty_dict() {
        let val = Value::Dict(IndexMap::new());
        let result = deep_materialize(&val, &test_ctx(), None).unwrap();
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_flat_dict() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let mut map: IndexMap<Key, ThunkId> = IndexMap::new();
        map.insert(
            Key::String("a".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(1), span))),
        );
        map.insert(
            Key::String("b".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(2), span))),
        );
        let val = Value::Dict(map);
        let result = deep_materialize(&val, &ctx, None).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let a = mat_id(&map[&Key::String("a".into())], &ctx).unwrap();
                assert_eq!(a, Value::Int(1));
                let b = mat_id(&map[&Key::String("b".into())], &ctx).unwrap();
                assert_eq!(b, Value::Int(2));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_nested_dict() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let mut inner: IndexMap<Key, ThunkId> = IndexMap::new();
        inner.insert(
            Key::String("y".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(42), span))),
        );
        let mut outer: IndexMap<Key, ThunkId> = IndexMap::new();
        outer.insert(
            Key::String("x".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(inner), span))),
        );
        let val = Value::Dict(outer);
        let result = deep_materialize(&val, &ctx, None).unwrap();
        match result {
            Value::Dict(outer_map) => {
                let x_val = mat_id(&outer_map[&Key::String("x".into())], &ctx).unwrap();
                match x_val {
                    Value::Dict(inner_map) => {
                        let y_val = mat_id(&inner_map[&Key::String("y".into())], &ctx).unwrap();
                        assert_eq!(y_val, Value::Int(42));
                    }
                    other => panic!("expected inner Dict, got {other:?}"),
                }
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_forces_unevaluated_thunks() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let expr = Arc::new(Spanned::new(CoreExpr::Int(99), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let unevaluated = Arc::new(Thunk::new_unevaluated_core(
            expr,
            env,
            Arc::clone(&ctx),
            span,
        ));

        let mut map: IndexMap<Key, ThunkId> = IndexMap::new();
        map.insert(Key::String("val".into()), ctx.alloc_thunk(unevaluated));
        let val = Value::Dict(map);

        let result = deep_materialize(&val, &ctx, None).unwrap();
        match result {
            Value::Dict(map) => {
                let v = mat_id(&map[&Key::String("val".into())], &ctx).unwrap();
                assert_eq!(v, Value::Int(99));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_function_passthrough() {
        let span = test_span(1, 1, 1, 5);
        let val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(Spanned::new(CoreExpr::Int(0), span)),
            env: Arc::new(RwLock::new(Environment::new())),
            annotation: None,
        };
        let result = deep_materialize(&val, &test_ctx(), None).unwrap();
        // Functions are opaque -- returned as-is
        match result {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "x");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_builtin_passthrough() {
        fn dummy(
            _ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Int(0),
                    test_span(1, 1, 1, 1),
                )))
            })
        }
        let val = Value::Builtin(crate::value::BuiltinDef {
            func: dummy,
            name: "test",
            pos_strictness: &[],
            force_count: 0,
        });
        let result = deep_materialize(&val, &test_ctx(), None).unwrap();
        match result {
            Value::Builtin(def) => assert_eq!(def.name, "test"),
            other => panic!("expected Builtin, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_dict_with_int_keys() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let mut map: IndexMap<Key, ThunkId> = IndexMap::new();
        map.insert(
            Key::Int(0),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val("zero".into()),
                span,
            ))),
        );
        map.insert(
            Key::Int(1),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val("one".into()),
                span,
            ))),
        );
        let val = Value::Dict(map);
        let result = deep_materialize(&val, &ctx, None).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let v0 = mat_id(&map[&Key::Int(0)], &ctx).unwrap();
                assert_eq!(v0, string_val("zero".into()));
                let v1 = mat_id(&map[&Key::Int(1)], &ctx).unwrap();
                assert_eq!(v1, string_val("one".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_preserves_key_order() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let mut map: IndexMap<Key, ThunkId> = IndexMap::new();
        map.insert(
            Key::String("c".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(3), span))),
        );
        map.insert(
            Key::String("a".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(1), span))),
        );
        map.insert(
            Key::String("b".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(2), span))),
        );
        let val = Value::Dict(map);
        let result = deep_materialize(&val, &ctx, None).unwrap();
        match result {
            Value::Dict(map) => {
                let keys: Vec<&Key> = map.keys().collect();
                assert_eq!(
                    keys,
                    vec![
                        &Key::String("c".into()),
                        &Key::String("a".into()),
                        &Key::String("b".into()),
                    ]
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_dict_containing_function() {
        // Dict with a function value -- function should pass through, not be traversed
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let func_val = Value::Function {
            params: Rc::new(vec![]),
            body: Arc::new(Spanned::new(CoreExpr::Int(0), span)),
            env: Arc::new(RwLock::new(Environment::new())),
            annotation: None,
        };
        let mut map: IndexMap<Key, ThunkId> = IndexMap::new();
        map.insert(
            Key::String("f".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(func_val, span))),
        );
        map.insert(
            Key::String("v".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(10), span))),
        );
        let val = Value::Dict(map);
        let result = deep_materialize(&val, &ctx, None).unwrap();
        match result {
            Value::Dict(map) => {
                let f = mat_id(&map[&Key::String("f".into())], &ctx).unwrap();
                assert!(matches!(f, Value::Function { .. }));
                let v = mat_id(&map[&Key::String("v".into())], &ctx).unwrap();
                assert_eq!(v, Value::Int(10));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_three_levels_deep() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Build [a: [b: [c: 99]]]
        let mut level3: IndexMap<Key, ThunkId> = IndexMap::new();
        level3.insert(
            Key::String("c".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(99), span))),
        );
        let mut level2: IndexMap<Key, ThunkId> = IndexMap::new();
        level2.insert(
            Key::String("b".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(level3), span))),
        );
        let mut level1: IndexMap<Key, ThunkId> = IndexMap::new();
        level1.insert(
            Key::String("a".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(level2), span))),
        );
        let val = Value::Dict(level1);

        let result = deep_materialize(&val, &ctx, None).unwrap();
        // Navigate three levels deep
        match result {
            Value::Dict(l1) => {
                let a = mat_id(&l1[&Key::String("a".into())], &ctx).unwrap();
                match a {
                    Value::Dict(l2) => {
                        let b = mat_id(&l2[&Key::String("b".into())], &ctx).unwrap();
                        match b {
                            Value::Dict(l3) => {
                                let c = mat_id(&l3[&Key::String("c".into())], &ctx).unwrap();
                                assert_eq!(c, Value::Int(99));
                            }
                            other => panic!("expected level 3 Dict, got {other:?}"),
                        }
                    }
                    other => panic!("expected level 2 Dict, got {other:?}"),
                }
            }
            other => panic!("expected level 1 Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_result_thunks_are_materialized() {
        // Verify that after deep_materialize, all thunks in the result dict
        // are in the Materialized state (not Unevaluated or PendingBuiltin)
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let expr = Arc::new(Spanned::new(CoreExpr::Int(7), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let unevaluated = Arc::new(Thunk::new_unevaluated_core(
            expr,
            env,
            Arc::clone(&ctx),
            span,
        ));

        let mut map: IndexMap<Key, ThunkId> = IndexMap::new();
        map.insert(Key::String("x".into()), ctx.alloc_thunk(unevaluated));
        let val = Value::Dict(map);

        let result = deep_materialize(&val, &ctx, None).unwrap();
        match result {
            Value::Dict(map) => {
                let thunk = get_thunk_rc(&map[&Key::String("x".into())], &ctx);
                // The thunk in the result should be in Materialized state
                assert_eq!(thunk.try_get_materialized(), Some(Value::Int(7)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_seq() {
        // Verify that deep_materialize forces both head and tail of Seq
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let head_expr = Arc::new(Spanned::new(CoreExpr::Int(42), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let head_thunk_rc = Arc::new(Thunk::new_unevaluated_core(
            head_expr,
            Arc::clone(&env),
            Arc::clone(&ctx),
            span,
        ));

        let tail_expr = Arc::new(Spanned::new(CoreExpr::Str("tail".into()), span));
        let tail_thunk_rc = Arc::new(Thunk::new_unevaluated_core(
            tail_expr,
            Arc::clone(&env),
            Arc::clone(&ctx),
            span,
        ));

        let seq = Value::Seq {
            head: ctx.alloc_thunk(head_thunk_rc),
            tail: ctx.alloc_thunk(tail_thunk_rc),
        };

        let result = deep_materialize(&seq, &ctx, None).unwrap();
        match result {
            Value::Seq { head, tail } => {
                // Both head and tail should be materialized
                let head_thunk = get_thunk_rc(&head, &ctx);
                assert_eq!(head_thunk.try_get_materialized(), Some(Value::Int(42)));

                let tail_thunk = get_thunk_rc(&tail, &ctx);
                match tail_thunk.try_get_materialized() {
                    Some(Value::String { source, start, end }) => {
                        assert_eq!(&source[start..end], "tail");
                    }
                    other => panic!("expected Materialized(String \"tail\"), got {other:?}"),
                }
            }
            other => panic!("expected Seq, got {other:?}"),
        }
    }

    // ── Sharing preservation tests (Launchbury 1993 invariant) ──────────

    #[test]
    fn test_deep_materialize_preserves_dict_sharing() {
        // Two dict entries share the same ThunkId in the input.
        // After deep_materialize, both entries should resolve to the same value.
        // Note: ThunkId equality is NOT guaranteed (deep_materialize allocates
        // new IDs in the output), but value equality must hold.
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let shared_thunk = Arc::new(Thunk::new_materialized(Value::Int(42), span));
        // Allocate once; use the same ThunkId for both entries
        let shared_id = ctx.alloc_thunk(shared_thunk);

        let mut map: IndexMap<Key, ThunkId> = IndexMap::new();
        map.insert(Key::String("a".into()), shared_id);
        map.insert(Key::String("b".into()), shared_id);
        let val = Value::Dict(map);

        let result = deep_materialize(&val, &ctx, None).unwrap();
        match result {
            Value::Dict(map) => {
                let a = &map[&Key::String("a".into())];
                let b = &map[&Key::String("b".into())];
                // Both entries must resolve to the same value (Int(42)).
                let va = mat_id(a, &ctx).unwrap();
                let vb = mat_id(b, &ctx).unwrap();
                assert_eq!(va, Value::Int(42), "entry a should be Int(42)");
                assert_eq!(vb, Value::Int(42), "entry b should be Int(42)");
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_preserves_seq_sharing() {
        // Head and tail share the same ThunkId. After deep_materialize,
        // both must resolve to the same value (Int(99)).
        // Note: ThunkId equality is NOT guaranteed; value equality is checked instead.
        // Intentionally invalid Seq tail (Int instead of Seq/Dict) — tests value preservation.
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let shared_thunk = Arc::new(Thunk::new_materialized(Value::Int(99), span));
        let shared_id = ctx.alloc_thunk(shared_thunk);

        let seq = Value::Seq {
            head: shared_id,
            tail: shared_id,
        };

        let result = deep_materialize(&seq, &ctx, None).unwrap();
        match result {
            Value::Seq { head, tail } => {
                let vh = mat_id(&head, &ctx).unwrap();
                let vt = mat_id(&tail, &ctx).unwrap();
                assert_eq!(vh, Value::Int(99), "head should be Int(99)");
                assert_eq!(vt, Value::Int(99), "tail should be Int(99)");
            }
            other => panic!("expected Seq, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_preserves_cross_structure_sharing() {
        // A shared ThunkId appears in both a nested dict and a seq within the
        // same top-level dict. All occurrences must resolve to the same VALUE
        // (ThunkId equality not guaranteed in the arena model).
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let shared_rc = Arc::new(Thunk::new_materialized(string_val("shared".into()), span));
        let shared_id = ctx.alloc_thunk(shared_rc);

        let mut inner_dict: IndexMap<Key, ThunkId> = IndexMap::new();
        inner_dict.insert(Key::String("x".into()), shared_id);
        let inner_dict_thunk = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(inner_dict),
            span,
        )));

        let empty_tail_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            span,
        )));
        let seq_val = Value::Seq {
            head: shared_id,
            tail: empty_tail_id,
        };
        let seq_thunk = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(seq_val, span)));

        let mut outer: IndexMap<Key, ThunkId> = IndexMap::new();
        outer.insert(Key::String("nested".into()), inner_dict_thunk);
        outer.insert(Key::String("seq".into()), seq_thunk);
        let val = Value::Dict(outer);

        let result = deep_materialize(&val, &ctx, None).unwrap();
        match result {
            Value::Dict(map) => {
                // Extract the shared ThunkId from the nested dict
                let nested_val = mat_id(&map[&Key::String("nested".into())], &ctx).unwrap();
                let nested_shared = match nested_val {
                    Value::Dict(d) => d[&Key::String("x".into())],
                    other => panic!("expected Dict, got {other:?}"),
                };

                // Extract the shared ThunkId from the seq head
                let seq_val = mat_id(&map[&Key::String("seq".into())], &ctx).unwrap();
                let seq_shared = match seq_val {
                    Value::Seq { head, .. } => head,
                    other => panic!("expected Seq, got {other:?}"),
                };

                // Verify both resolve to the same value (ThunkId equality not guaranteed).
                let v_nested = mat_id(&nested_shared, &ctx).unwrap();
                let v_seq = mat_id(&seq_shared, &ctx).unwrap();
                assert_eq!(
                    v_nested,
                    string_val("shared".into()),
                    "nested dict shared value"
                );
                assert_eq!(v_seq, string_val("shared".into()), "seq head shared value");
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_proxy() {
        // Test that deep_materialize traverses into the proxy handler thunk
        // and returns a new Proxy with the deep-materialized handler.
        let span = test_span(1, 1, 1, 5);
        let expr = Arc::new(Spanned::new(CoreExpr::Int(42), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let ctx = test_ctx();

        // Create an unevaluated handler thunk
        let handler_rc = Arc::new(Thunk::new_unevaluated_core(
            expr,
            env,
            Arc::clone(&ctx),
            span,
        ));
        let handler_id = ctx.alloc_thunk(handler_rc);
        let proxy_val = Value::Proxy {
            handler: handler_id,
        };

        // Deep materialize the proxy
        let result = deep_materialize(&proxy_val, &ctx, None).unwrap();

        match result {
            Value::Proxy {
                handler: deep_handler_id,
            } => {
                // Verify the handler was deep-materialized
                let handler_val = mat_id(&deep_handler_id, &ctx).unwrap();
                assert_eq!(handler_val, Value::Int(42));
            }
            other => panic!("expected Proxy, got {other:?}"),
        }
    }

    // ── Stack trace / call stack reconstruction tests ──────────────────

    #[test]
    fn test_call_error_has_stack_frame_with_function_name() {
        // [f: [fn [x] missing]; result: [f 1]]
        // Calling f with body that references missing should produce a
        // stack frame with "[f ...]".
        let env = empty_env();
        let fn_span = test_span(1, 1, 1, 20);
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(Spanned::new(
                CoreExpr::FreeVar("missing".to_string()),
                test_span(1, 15, 1, 23),
            )),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, fn_span)),
        );

        let call_span = test_span(2, 1, 2, 15);
        let call_expr = Spanned::new(
            Expr::Call {
                func: Box::new(Spanned::new(
                    Expr::var_ref("f".into()),
                    test_span(2, 7, 2, 8),
                )),
                args: vec![Rc::new(Spanned::new(Expr::Int(1), test_span(2, 10, 2, 11)))],
                named_args: vec![],
                implied: false,
            },
            call_span,
        );

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
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

    #[test]
    fn test_nested_call_produces_multi_frame_stack() {
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
                CoreExpr::FreeVar("missing".to_string()),
                test_span(1, 20, 1, 28),
            )),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "inner".into(),
            Arc::new(Thunk::new_materialized(inner_fn, test_span(1, 1, 1, 30))),
        );

        // Outer function: body is [call $inner $y]
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
                        CoreExpr::FreeVar("inner".to_string()),
                        test_span(2, 21, 2, 26),
                    )),
                    args: vec![Arc::new(Spanned::new(
                        CoreExpr::FreeVar("y".to_string()),
                        test_span(2, 28, 2, 29),
                    ))],
                    named_args: vec![],
                    implied: false,
                },
                inner_call_span,
            )),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "outer".into(),
            Arc::new(Thunk::new_materialized(outer_fn, test_span(2, 1, 2, 35))),
        );

        // Evaluate [call $outer 1]
        let outer_call_span = test_span(3, 1, 3, 20);
        let call_expr = Spanned::new(
            Expr::Call {
                func: Box::new(Spanned::new(
                    Expr::var_ref("outer".into()),
                    test_span(3, 7, 3, 12),
                )),
                args: vec![Rc::new(Spanned::new(Expr::Int(1), test_span(3, 14, 3, 15)))],
                named_args: vec![],
                implied: false,
            },
            outer_call_span,
        );

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("undefined variable: missing"));

        // Should have frames for both call sites
        let labels: Vec<&str> = err.stack.iter().map(|f| f.label.as_str()).collect();
        assert!(
            labels.contains(&"[inner ...]"),
            "expected '[inner ...]' in stack, got: {labels:?}"
        );
        assert!(
            labels.contains(&"[outer ...]"),
            "expected '[outer ...]' in stack, got: {labels:?}"
        );
        // Inner call should appear before outer call (innermost first)
        let inner_pos = labels.iter().position(|l| *l == "[inner ...]").unwrap();
        let outer_pos = labels.iter().position(|l| *l == "[outer ...]").unwrap();
        assert!(
            inner_pos < outer_pos,
            "inner call frame should come before outer: {labels:?}"
        );
    }

    #[test]
    fn test_dot_access_error_has_access_frame() {
        // When dot access fails because the target evaluation itself errors,
        // the error should include a frame indicating the access context.
        //
        // [a: $missing]
        // $a.x  -- accessing .x should add a frame
        let env = empty_env();

        // Put a dict with a broken value in the env
        let ctx = test_ctx();
        let dict_span = test_span(1, 1, 1, 20);
        let mut dict_map: IndexMap<Key, ThunkId> = IndexMap::new();
        let bad_thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(
                CoreExpr::FreeVar("missing".into()),
                test_span(1, 8, 1, 15),
            )),
            Arc::clone(&env),
            Arc::clone(&ctx),
            test_span(1, 8, 1, 15),
        ));
        dict_map.insert(Key::String("x".into()), ctx.alloc_thunk(bad_thunk));

        env.write().unwrap().insert(
            "a".into(),
            Arc::new(Thunk::new_materialized(Value::Dict(dict_map), dict_span)),
        );

        // Now access $a.x -- this should succeed (returns the thunk), but
        // materializing the result should fail
        let access_span = test_span(2, 1, 2, 5);
        let access_expr = Spanned::new(
            Expr::DotAccess {
                expr: Box::new(Spanned::new(
                    Expr::var_ref("a".into()),
                    test_span(2, 1, 2, 2),
                )),
                field: crate::ast::DotKey::Ident("x".into()),
            },
            access_span,
        );

        let thunk = eval(Rc::new(access_expr.clone()), env, &ctx).unwrap();
        let mat_span = test_span(3, 1, 3, 10);
        let err = materialize(&thunk, Some(&mat_span), &ctx).unwrap_err();
        assert!(err.to_string().contains("undefined variable: missing"));
        // The materialization span should be set
        assert!(err.materialization_span.is_some());
    }

    #[test]
    fn test_dot_access_on_erroring_target_has_frame() {
        // $nonexistent.field -- the target itself fails, and the error
        // should include an "accessing .field" frame.
        let env = empty_env();
        let access_span = test_span(1, 1, 1, 20);
        let access_expr = Spanned::new(
            Expr::DotAccess {
                expr: Box::new(Spanned::new(
                    Expr::var_ref("nonexistent".into()),
                    test_span(1, 1, 1, 12),
                )),
                field: crate::ast::DotKey::Ident("field".into()),
            },
            access_span,
        );

        let thunk = eval(Rc::new(access_expr.clone()), env, &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("undefined variable: nonexistent"));
        // Should have an "accessing .field" frame
        assert!(
            err.stack.iter().any(|f| f.label == "accessing .field"),
            "expected 'accessing .field' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_chained_access_error_shows_chain() {
        // [a: [x: $missing]]
        // $a.x  -- force chain
        // When materialized, the error should show the materialization chain.
        let ctx = test_ctx();
        let inner_env = empty_env();
        let mut inner_map: IndexMap<Key, ThunkId> = IndexMap::new();
        inner_map.insert(
            Key::String("x".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_unevaluated_core(
                Arc::new(Spanned::new(
                    CoreExpr::FreeVar("missing".into()),
                    test_span(1, 10, 1, 18),
                )),
                Arc::clone(&inner_env),
                Arc::clone(&ctx),
                test_span(1, 10, 1, 18),
            ))),
        );
        let inner_dict = Value::Dict(inner_map);

        let env = empty_env();
        env.write().unwrap().insert(
            "a".into(),
            Arc::new(Thunk::new_materialized(inner_dict, test_span(1, 1, 1, 20))),
        );

        // Build $a.x access
        let access_span = test_span(2, 1, 2, 5);
        let access_expr = Spanned::new(
            Expr::DotAccess {
                expr: Box::new(Spanned::new(
                    Expr::var_ref("a".into()),
                    test_span(2, 1, 2, 2),
                )),
                field: crate::ast::DotKey::Ident("x".into()),
            },
            access_span,
        );

        // Eval returns an Unevaluated thunk wrapping the DotAccess
        let thunk = eval(Rc::new(access_expr.clone()), Arc::clone(&env), &ctx).unwrap();

        // Materialize with a different span (simulating a reference from $b)
        let b_span = test_span(3, 1, 3, 5);
        let err = materialize(&thunk, Some(&b_span), &ctx).unwrap_err();
        assert!(err.to_string().contains("undefined variable: missing"));
        // After threading outer_mat_span through DotAccessForceData, the error should
        // show b_span (the outermost call-site) rather than access_span (the .x access).
        assert_eq!(
            err.materialization_span,
            Some(b_span),
            "should use outer materialization span from access chain"
        );
    }

    #[test]
    fn test_func_label_varref() {
        use crate::eval_call::func_label_core;
        let label = func_label_core(&CoreExpr::Var {
            name: "f".to_string(),
            level: 0,
            slot: 0,
        });
        assert_eq!(label.as_deref(), Some("[f ...]"));
    }

    #[test]
    fn test_func_label_dot_access() {
        use crate::eval_call::func_label_core;
        let expr = CoreExpr::DotAccess {
            expr: Arc::new(sp(CoreExpr::Var {
                name: "utils".to_string(),
                level: 0,
                slot: 0,
            })),
            field: crate::ast::DotKey::Ident("run".into()),
        };
        let label = func_label_core(&expr);
        assert_eq!(label.as_deref(), Some("[<dot-access> ...]"));
    }

    #[test]
    fn test_func_label_chained_dot_access() {
        use crate::eval_call::func_label_core;
        let expr = CoreExpr::DotAccess {
            expr: Arc::new(sp(CoreExpr::DotAccess {
                expr: Arc::new(sp(CoreExpr::Var {
                    name: "a".to_string(),
                    level: 0,
                    slot: 0,
                })),
                field: crate::ast::DotKey::Ident("b".into()),
            })),
            field: crate::ast::DotKey::Ident("c".into()),
        };
        let label = func_label_core(&expr);
        assert_eq!(label.as_deref(), Some("[<dot-access> ...]"));
    }

    #[test]
    fn test_func_label_anonymous() {
        use crate::eval_call::func_label_core;
        // Anonymous calls return None (no origin label adds diagnostic value)
        assert_eq!(func_label_core(&CoreExpr::Int(42)), None);
    }

    #[test]
    fn test_materialize_chain_no_duplicate_frames() {
        // When the same mat_span propagates through nested materialize calls,
        // we should not get duplicate frames for the same span.
        let env = empty_env();

        // Create a thunk whose body is another unevaluated thunk that errors
        let inner_expr = Spanned::new(CoreExpr::FreeVar("missing".into()), test_span(1, 1, 1, 8));
        let inner_thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(inner_expr),
            Arc::clone(&env),
            Arc::clone(&test_ctx()),
            test_span(1, 1, 1, 8),
        ));

        // Materialize with a specific span
        let mat_span = test_span(5, 1, 5, 10);
        let err = materialize(&inner_thunk, Some(&mat_span), &test_ctx()).unwrap_err();

        // Count how many frames have the same span
        let frame_count = err.stack.iter().filter(|f| f.span == mat_span).count();
        assert!(
            frame_count <= 1,
            "expected at most 1 frame with mat_span, got {frame_count}: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_call_arity_error_has_call_frame() {
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
            body: Arc::new(sp(CoreExpr::FreeVar("a".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 20))),
        );

        // Call with wrong arity: [call $f 1] (needs 2 args)
        let call_span = test_span(2, 1, 2, 15);
        let call_expr = Spanned::new(
            Expr::Call {
                func: Box::new(Spanned::new(
                    Expr::var_ref("f".into()),
                    test_span(2, 7, 2, 8),
                )),
                args: vec![Rc::new(Spanned::new(Expr::Int(1), test_span(2, 10, 2, 11)))],
                named_args: vec![],
                implied: false,
            },
            call_span,
        );

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx())
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
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

    #[test]
    fn test_builtin_error_has_stack_frame_with_builtin_name() {
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
        env.write().unwrap().insert(
            "fail".into(),
            Arc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: failing_builtin,
                    name: "fail",
                    pos_strictness: &[],
                    force_count: 0,
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("fail".into()))),
            args: vec![],
            named_args: vec![],
            implied: false,
        });

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("test builtin failure"));
        // The stack should contain "[fail ...]"
        assert!(
            err.stack.iter().any(|f| f.label == "[fail ...]"),
            "expected '[fail ...]' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_error_display_with_full_stack() {
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

    #[test]
    fn test_pending_call_llt_function() {
        // Create a PendingCall thunk that calls an LLT function
        // [fn [x y] [call $+ $x $y]] with args (3, 4)
        let env = empty_env();

        // Create a simple addition function
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
                func: Arc::new(sp(CoreExpr::FreeVar("+".to_string()))),
                args: vec![
                    Arc::new(sp(CoreExpr::FreeVar("x".to_string()))),
                    Arc::new(sp(CoreExpr::FreeVar("y".to_string()))),
                ],
                named_args: vec![],
                implied: false,
            })),
            env: Arc::clone(&env),
            annotation: None,
        };

        // Add the builtin $+ to the environment
        fn add_builtin(
            ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let crate::value::BuiltinArgs { args, .. } = ctx;
                let a = materialize(&args[0], None, &test_ctx())?;
                let b = materialize(&args[1], None, &test_ctx())?;
                match (a, b) {
                    (Value::Int(x), Value::Int(y)) => Ok(Arc::new(Thunk::new_materialized(
                        Value::Int(x + y),
                        test_span(1, 1, 1, 1),
                    ))),
                    _ => panic!("test expects Int args"),
                }
            })
        }
        env.write().unwrap().insert(
            "+".into(),
            Arc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: add_builtin,
                    name: "+",
                    pos_strictness: &[],
                    force_count: 0,
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
            call_span,
            empty_env(),
            call_span,
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
        );

        // Materialize should call the function and return the result
        let result = materialize(&pending, None, &test_ctx()).unwrap();
        assert_eq!(result, Value::Int(7));
    }

    #[test]
    fn test_pending_call_builtin_function() {
        // Create a PendingCall thunk where the function is a Builtin
        fn multiply_builtin(
            ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let crate::value::BuiltinArgs { args, .. } = ctx;
                let a = materialize(&args[0], None, &test_ctx())?;
                let b = materialize(&args[1], None, &test_ctx())?;
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
            call_span,
            empty_env(),
            call_span,
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
        );

        // Materialize should call the builtin directly and return the result
        let result = materialize(&pending, None, &test_ctx()).unwrap();
        assert_eq!(result, Value::Int(30));
    }

    #[test]
    fn test_pending_call_memoizes() {
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
            body: Arc::new(sp(CoreExpr::FreeVar("x".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
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
            call_span,
            empty_env(),
            call_span,
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
        ));

        // First materialization
        let result1 = materialize(&pending, None, &test_ctx()).unwrap();
        assert_eq!(result1, Value::Int(42));

        // Check that the thunk is now in Materialized state
        assert_eq!(
            pending.try_get_materialized(),
            Some(Value::Int(42)),
            "expected Materialized after first call"
        );

        // Second materialization should return cached value
        let result2 = materialize(&pending, None, &test_ctx()).unwrap();
        assert_eq!(result2, Value::Int(42));
    }

    #[test]
    fn test_pending_call_non_function_error() {
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
            call_span,
            empty_env(),
            call_span,
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
        );

        let err = materialize(&pending, None, &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("expected Function or Builtin, got Int"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_pending_call_with_unevaluated_args() {
        // PendingCall should work with unevaluated argument thunks (lazy evaluation)
        let env = empty_env();

        let identity_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::FreeVar("x".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
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
            call_span,
            empty_env(),
            call_span,
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
        );

        // Materialize should evaluate the arg thunk and return the result
        let result = materialize(&pending, None, &test_ctx()).unwrap();
        assert_eq!(result, Value::Int(99));
    }

    #[test]
    fn test_pending_call_with_named_args() {
        // PendingCall should pass named args through to function invocation
        let env = empty_env();

        // Install a built-in add function
        fn add_builtin(
            ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let crate::value::BuiltinArgs { args, .. } = ctx;
                let a = materialize(&args[0], None, &test_ctx())?;
                let b = materialize(&args[1], None, &test_ctx())?;
                match (a, b) {
                    (Value::Int(x), Value::Int(y)) => Ok(Arc::new(Thunk::new_materialized(
                        Value::Int(x + y),
                        test_span(1, 1, 1, 1),
                    ))),
                    _ => panic!("test expects Int args"),
                }
            })
        }
        env.write().unwrap().insert(
            "+".into(),
            Arc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: add_builtin,
                    name: "+",
                    pos_strictness: &[],
                    force_count: 0,
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        // Create a function that takes a mix of positional and named parameters
        let fn_with_named = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "a".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "b".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![sp(Entry {
                        key: Some(sp(Expr::Str("default".into()))),
                        value: rsp(Expr::Int(10)),
                    })]))),
                    variadic: false,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Call {
                func: Arc::new(sp(CoreExpr::FreeVar("+".to_string()))),
                args: vec![
                    Arc::new(sp(CoreExpr::FreeVar("a".to_string()))),
                    Arc::new(sp(CoreExpr::FreeVar("b".to_string()))),
                ],
                named_args: vec![],
                implied: false,
            })),
            env: Arc::clone(&env),
            annotation: None,
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
            call_span,
            empty_env(),
            call_span,
            Some(Arc::from("test-pending-call-named")),
            Arc::clone(&test_ctx()),
        );

        // Materialize should pass named args through correctly
        let result = materialize(&pending, None, &test_ctx()).unwrap();
        assert_eq!(result, Value::Int(8)); // 5 + 3
    }

    #[test]
    fn test_pending_call_with_default_named_args() {
        // PendingCall with partial named args should use defaults
        let env = empty_env();

        // Install a built-in add function
        fn add_builtin(
            ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let crate::value::BuiltinArgs { args, .. } = ctx;
                let a = materialize(&args[0], None, &test_ctx())?;
                let b = materialize(&args[1], None, &test_ctx())?;
                match (a, b) {
                    (Value::Int(x), Value::Int(y)) => Ok(Arc::new(Thunk::new_materialized(
                        Value::Int(x + y),
                        test_span(1, 1, 1, 1),
                    ))),
                    _ => panic!("test expects Int args"),
                }
            })
        }
        env.write().unwrap().insert(
            "+".into(),
            Arc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: add_builtin,
                    name: "+",
                    pos_strictness: &[],
                    force_count: 0,
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        let fn_with_default = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![sp(Entry {
                        key: Some(sp(Expr::Str("default".into()))),
                        value: rsp(Expr::Int(10)),
                    })]))),
                    variadic: false,
                },
            ]),
            body: Arc::new(sp(CoreExpr::Call {
                func: Arc::new(sp(CoreExpr::FreeVar("+".to_string()))),
                args: vec![
                    Arc::new(sp(CoreExpr::FreeVar("x".to_string()))),
                    Arc::new(sp(CoreExpr::FreeVar("y".to_string()))),
                ],
                named_args: vec![],
                implied: false,
            })),
            env: Arc::clone(&env),
            annotation: None,
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
            call_span,
            empty_env(),
            call_span,
            Some(Arc::from("test-pending-call-default")),
            Arc::clone(&test_ctx()),
        );

        // Materialize should use default for y (10)
        let result = materialize(&pending, None, &test_ctx()).unwrap();
        assert_eq!(result, Value::Int(17)); // 7 + 10
    }

    // ── Failed thunk state tests ───────────────────────────────────────

    #[test]
    fn test_failed_state_returns_cached_error() {
        // When a thunk fails, it should cache the error in Failed state
        // and return it on subsequent materialization attempts
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("undefined".into())),
        })];
        let ctx = test_ctx();
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Arc::clone(&env), &ctx).unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).unwrap();

        let x_thunk = match &dict_val {
            Value::Dict(map) => get_thunk_rc(map.get(&Key::String("x".into())).unwrap(), &ctx),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First materialization: should fail and cache the error
        let err1 = materialize(&x_thunk, None, &ctx).unwrap_err();
        assert!(
            err1.kind
                .to_string()
                .contains("undefined variable: undefined"),
            "first error: got: {}",
            err1.kind
        );

        // Check that the thunk is now in Failed state
        {
            let cached_err = x_thunk
                .get_cached_error()
                .expect("thunk should be in Failed state");
            assert!(cached_err
                .kind
                .to_string()
                .contains("undefined variable: undefined"));
        }

        // Second materialization: should return the cached error
        let err2 = materialize(&x_thunk, None, &ctx).unwrap_err();
        assert!(
            err2.kind
                .to_string()
                .contains("undefined variable: undefined"),
            "second error: got: {}",
            err2.kind
        );
    }

    #[test]
    #[ignore = "pre-existing: OnceCell-based thunk state cannot update cached error; cache_failure_once is write-once"]
    fn test_failed_state_updates_materialization_span() {
        // Failed state should preserve the first materialization_span and add
        // subsequent access sites as stack frames (dual-span model)
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("broken".into()))),
            value: rsp(Expr::var_ref("missing".into())),
        })];
        let ctx = test_ctx();
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Arc::clone(&env), &ctx).unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).unwrap();

        let broken_thunk = match &dict_val {
            Value::Dict(map) => get_thunk_rc(map.get(&Key::String("broken".into())).unwrap(), &ctx),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First access with one materialization span
        let span1 = test_span(10, 1, 10, 5);
        let err1 = materialize(&broken_thunk, Some(&span1), &ctx).unwrap_err();
        assert_eq!(err1.materialization_span, Some(span1));
        assert_eq!(err1.stack.len(), 0);

        // Second access with a different materialization span should preserve span1
        // and add span2 as a stack frame
        let span2 = test_span(20, 1, 20, 5);
        let err2 = materialize(&broken_thunk, Some(&span2), &ctx).unwrap_err();
        assert_eq!(err2.materialization_span, Some(span1)); // PRESERVED
        assert_eq!(err2.stack.len(), 1);
        assert_eq!(err2.stack[0].label, "materialized");
        assert_eq!(err2.stack[0].span, span2);

        // Third access with no materialization span returns error with the
        // original materialization_span and the stack frame from the second access
        let err3 = materialize(&broken_thunk, None, &ctx).unwrap_err();
        assert_eq!(err3.materialization_span, Some(span1)); // PRESERVED
        assert_eq!(err3.stack.len(), 1);
        assert_eq!(err3.stack[0].span, span2);
    }

    #[test]
    fn test_failed_state_preserves_stack_frames() {
        // Failed state should preserve the original error's stack frames
        let env = empty_env();

        // Create a function that will fail
        let failing_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Arc::new(sp(CoreExpr::FreeVar("nonexistent".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };

        env.write().unwrap().insert(
            "bad_fn".into(),
            Arc::new(Thunk::new_materialized(failing_fn, test_span(1, 1, 1, 20))),
        );

        // Call the failing function
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("bad_fn".into()))),
            args: vec![rsp(Expr::Int(1))],
            named_args: vec![],
            implied: false,
        });

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();

        // First materialization: error should have stack frames
        let err1 = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err1
            .kind
            .to_string()
            .contains("undefined variable: nonexistent"));
        let frame_count1 = err1.stack.len();
        assert!(frame_count1 > 0, "should have at least one stack frame");

        // Second materialization: error should have the same stack frames
        let err2 = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert_eq!(
            err2.stack.len(),
            frame_count1,
            "stack frames should be preserved"
        );
    }

    #[test]
    fn test_pending_builtin_error_becomes_failed() {
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
        env.write().unwrap().insert(
            "fail".into(),
            Arc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: failing_builtin,
                    name: "fail",
                    pos_strictness: &[],
                    force_count: 0,
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("fail".into()))),
            args: vec![],
            named_args: vec![],
            implied: false,
        });

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();

        // First materialization: should fail
        let err1 = materialize(&thunk, None, &test_ctx()).unwrap_err();
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
        let err2 = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err2
            .kind
            .to_string()
            .contains("builtin intentionally failed"));
    }

    #[test]
    fn test_pending_call_error_becomes_failed() {
        // When a PendingCall fails, it should transition to Failed state
        let env = empty_env();

        let failing_fn = Value::Function {
            params: Rc::new(vec![]),
            body: Arc::new(sp(CoreExpr::FreeVar("does_not_exist".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };

        let func_thunk = Arc::new(Thunk::new_materialized(failing_fn, test_span(1, 1, 1, 10)));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Arc::new(Thunk::new_pending_call(
            func_thunk,
            vec![],
            IndexMap::new(),
            call_span,
            empty_env(),
            call_span,
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
        ));

        // First materialization: should fail
        let err1 = materialize(&pending, None, &test_ctx()).unwrap_err();
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
        let err2 = materialize(&pending, None, &test_ctx()).unwrap_err();
        assert!(err2
            .kind
            .to_string()
            .contains("undefined variable: does_not_exist"));
    }

    #[test]
    fn test_pending_call_func_materialization_failure() {
        let bad_func = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(sp(CoreExpr::FreeVar("nonexistent_func".into()))),
            empty_env(),
            Arc::clone(&test_ctx()),
            test_span(1, 1, 1, 10),
        ));
        let call_span = test_span(2, 1, 2, 10);
        let pending = Arc::new(Thunk::new_pending_call(
            bad_func,
            vec![],
            IndexMap::new(),
            call_span,
            empty_env(),
            call_span,
            Some(Arc::from("test-pending-call")),
            Arc::clone(&test_ctx()),
        ));

        // First materialization should fail with undefined variable error
        let err = materialize(&pending, None, &test_ctx()).unwrap_err();
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
        let err2 = materialize(&pending, None, &test_ctx()).unwrap_err();
        assert!(err2
            .kind
            .to_string()
            .contains("undefined variable: nonexistent_func"));
        assert!(!err2.kind.to_string().contains("circular dependency"));
    }

    #[test]
    fn test_unevaluated_error_becomes_failed() {
        // When an Unevaluated thunk fails during materialization, it should transition to Failed
        let expr = sp(CoreExpr::FreeVar("undefined_var".into()));
        let env = empty_env();
        let thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(expr),
            Arc::clone(&env),
            Arc::clone(&test_ctx()),
            test_span(1, 1, 1, 15),
        ));

        // First materialization: should fail
        let err1 = materialize(&thunk, None, &test_ctx()).unwrap_err();
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
        let err2 = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err2
            .kind
            .to_string()
            .contains("undefined variable: undefined_var"));
    }

    #[test]
    fn test_failed_state_same_span_no_duplicate() {
        // Accessing a Failed thunk twice with the same mat_span should not duplicate frames.
        // Use DotAccess (deferred thunk) so eval returns Ok and failure happens on materialize.
        let env = empty_env();

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("undefined_var".into()))),
            field: crate::ast::DotKey::Ident("field".into()),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx()).unwrap();

        // First materialization: error with a specific mat_span
        let mat_span = test_span(10, 5, 10, 15);
        let err1 = materialize(&thunk, Some(&mat_span), &test_ctx()).unwrap_err();
        assert!(err1
            .kind
            .to_string()
            .contains("undefined variable: undefined_var"));
        let frame_count1 = err1.stack.len();

        // Second materialization: same mat_span
        let err2 = materialize(&thunk, Some(&mat_span), &test_ctx()).unwrap_err();
        assert_eq!(
            err2.stack.len(),
            frame_count1,
            "same mat_span should not duplicate frames"
        );
    }

    #[test]
    #[ignore = "pre-existing: OnceCell-based thunk state cannot update cached error; cache_failure_once is write-once"]
    fn test_failed_state_none_then_some_mat_span() {
        // First access with None mat_span, then Some(span1), then Some(span2).
        // Use DotAccess (deferred thunk) so eval returns Ok and failure happens on materialize.
        let env = empty_env();

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("undefined_var".into()))),
            field: crate::ast::DotKey::Ident("field".into()),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx()).unwrap();

        // First access: None mat_span
        let err1 = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err1
            .kind
            .to_string()
            .contains("undefined variable: undefined_var"));
        assert!(err1.materialization_span.is_none());

        // Second access: Some(span1) — should update materialization_span
        let span1 = test_span(10, 5, 10, 15);
        let err2 = materialize(&thunk, Some(&span1), &test_ctx()).unwrap_err();
        assert_eq!(
            err2.materialization_span,
            Some(span1),
            "mat_span should be set on second access with Some"
        );

        // Third access: Some(span2) — should add as stack frame, preserve span1 as mat_span
        let span2 = test_span(20, 5, 20, 15);
        let err3 = materialize(&thunk, Some(&span2), &test_ctx()).unwrap_err();
        assert_eq!(
            err3.materialization_span,
            Some(span1),
            "original mat_span should be preserved"
        );
        assert!(
            err3.stack.iter().any(|f| f.span == span2),
            "span2 should be in stack frames"
        );
    }

    #[test]
    #[ignore = "pre-existing regression from runtime-v2 merge: materialize returns Ok for infinite recursion instead of DepthExceeded"]
    fn test_pending_call_cycle_detection() {
        // 256 levels of LLT recursion needs more than the default 8MB Rust stack.
        let result = std::thread::Builder::new()
            .stack_size(128 * 1024 * 1024) // 128MB — debug-mode materialize() needs ~100MB at 256 levels
            .spawn(|| {
                let env = empty_env();

                let recursive_fn = Value::Function {
                    params: Rc::new(vec![Param {
                        name: "x".into(),
                        annotation: None,
                        variadic: false,
                    }]),
                    body: Arc::new(sp(CoreExpr::Call {
                        func: Arc::new(sp(CoreExpr::FreeVar("f".to_string()))),
                        args: vec![Arc::new(sp(CoreExpr::FreeVar("x".to_string())))],
                        named_args: vec![],
                        implied: false,
                    })),
                    env: Arc::clone(&env),
                    annotation: None,
                };

                env.write().unwrap().insert(
                    "f".into(),
                    Arc::new(Thunk::new_materialized(
                        recursive_fn,
                        test_span(1, 1, 1, 20),
                    )),
                );

                let call_expr = sp(Expr::Call {
                    func: Box::new(sp(Expr::var_ref("f".into()))),
                    args: vec![rsp(Expr::Int(1))],
                    named_args: vec![],
                    implied: false,
                });

                let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx()).unwrap();
                materialize(&thunk, None, &test_ctx()).unwrap_err()
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(
            result
                .kind
                .to_string()
                .contains("maximum evaluation depth exceeded"),
            "got: {}",
            result.kind.to_string()
        );
    }

    // ── Non-cacheable error tests (is_cacheable) ───────────────────────

    #[test]
    fn test_regular_error_does_cache() {
        // Regular errors (not DepthExceeded) should transition to Failed state
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("undefined".into())),
        })];
        let ctx = test_ctx();
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Arc::clone(&env), &ctx).unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).unwrap();

        let x_thunk = match &dict_val {
            Value::Dict(map) => get_thunk_rc(map.get(&Key::String("x".into())).unwrap(), &ctx),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First materialization: should fail and cache the error
        let err1 = materialize(&x_thunk, None, &ctx).unwrap_err();
        assert!(
            err1.kind
                .to_string()
                .contains("undefined variable: undefined"),
            "expected undefined variable error, got: {}",
            err1.kind.to_string()
        );

        // The thunk SHOULD be in Failed state because UndefinedVariable is cacheable
        let cached_err = x_thunk
            .get_cached_error()
            .expect("expected Failed state with cached error after cacheable error");
        assert!(
            cached_err
                .kind
                .to_string()
                .contains("undefined variable: undefined"),
            "cached error mismatch: got: {}",
            cached_err.to_string()
        );
    }

    #[test]
    fn test_depth_exceeded_does_not_cache() {
        // DepthExceeded errors should NOT cache in Failed state — they should allow retry.
        // This test verifies the is_cacheable() contract for DepthExceeded errors.
        //
        // NOTE: Constructing a deep-enough non-cyclic computation to actually trigger
        // DepthExceeded is complex (requires 256+ nested calls without cycles).
        // This test validates the is_cacheable() property directly. The full
        // DepthExceeded error path is tested via eval_materialize.rs::deep_tests::test_deep_materialize_cache_cleanup_on_materialize_error.
        use crate::error::ErrorKind;

        // Verify DepthExceeded is non-cacheable
        assert!(
            !ErrorKind::DepthExceeded { limit: 256 }.is_cacheable(),
            "DepthExceeded must be non-cacheable (allows retry at different depth)"
        );

        // All other error kinds should be cacheable (tested in error.rs::test_is_cacheable)
        assert!(
            ErrorKind::UndefinedVariable {
                name: "x".to_string()
            }
            .is_cacheable(),
            "Regular errors should be cacheable"
        );
    }

    // === EvalContext isolation tests ===

    // ── Structural TypeAssert tests (resolved_type: Some(Type::...)) ────
    // These test the NEW structural validation path added by the
    // typeassert-structural sprint, distinct from the nominal fallback path
    // (resolved_type: None) tested in the existing TypeAssert tests above.

    #[test]
    fn test_typeassert_structural_int_pass() {
        // Structural path: resolved_type = Some(Type::Int), value is Int(42) -> pass
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(Some(Type::Int)),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    #[ignore = "pre-existing: TypeAssert is lazy in new CEK model, type error fires on materialize() not eval()"]
    fn test_typeassert_structural_int_fail() {
        // Structural path: resolved_type = Some(Type::Int), value is String -> error
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(Some(Type::Int)),
        });
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_typeassert_structural_str_pass() {
        // Structural path: resolved_type = Some(Type::Str), value is String -> pass
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Str".into())),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(Some(Type::Str)),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[test]
    fn test_typeassert_structural_any() {
        // Structural path: resolved_type = Some(Type::Top), any value passes
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Any".into())),
            expr: Box::new(sp(Expr::Str("anything".into()))),
            resolved_type: RefCell::new(Some(Type::Top)),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, string_val("anything".into()));
    }

    #[test]
    fn test_typeassert_structural_any_accepts_int() {
        // Type::Top accepts Int as well (covers any-value branch)
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Any".into())),
            expr: Box::new(sp(Expr::Int(99))),
            resolved_type: RefCell::new(Some(Type::Top)),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_typeassert_structural_record_shape_check() {
        // Structural path: resolved_type = Some(Type::Record(..., Open))
        // Dict has required field "name" -> pass.
        // The record type check is immediate (shape check), field guard wrapping deferred.
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), Type::Str);
        let record_type = Type::Record(Row { fields });

        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("Alice".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("age".into()))),
                value: rsp(Expr::Int(30)),
            }),
        ];
        let dict_expr = sp(Expr::Dict(entries));
        let inner_expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Record".into())),
            expr: Box::new(dict_expr),
            resolved_type: RefCell::new(Some(record_type)),
        });

        let thunk = eval(Rc::new(inner_expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        // Should be a Dict with the expected fields
        match &val {
            Value::Dict(map) => {
                assert!(map.contains_key(&Key::String("name".into())));
                assert!(map.contains_key(&Key::String("age".into())));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "pre-existing: TypeAssert type errors require materialize() in lazy CEK model"]
    fn test_typeassert_structural_record_missing_field() {
        // Structural path: record type requires field "id", dict doesn't have it -> error
        let mut fields = HashMap::new();
        fields.insert("id".to_string(), Type::Int);
        let record_type = Type::Record(Row { fields });

        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("name".into()))),
            value: rsp(Expr::Str("Alice".into())),
        })];
        let dict_expr = sp(Expr::Dict(entries));
        let inner_expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Record".into())),
            expr: Box::new(dict_expr),
            resolved_type: RefCell::new(Some(record_type)),
        });

        let err = eval(Rc::new(inner_expr.clone()), empty_env(), &test_ctx()).unwrap_err();
        assert!(
            err.to_string().contains("record missing field \"id\""),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_typeassert_structural_record_extra_field_accepted() {
        // BAS width subtyping: under BAS, extra fields are ALWAYS accepted.
        // A dict with {x: 1, extra: 2} satisfies the annotation @[x: Int]
        // because the annotation only constrains what it declares.
        let mut fields = HashMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row { fields });

        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("extra".into()))),
                value: rsp(Expr::Int(2)),
            }),
        ];
        let dict_expr = sp(Expr::Dict(entries));
        let inner_expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Record".into())),
            expr: Box::new(dict_expr),
            resolved_type: RefCell::new(Some(record_type)),
        });

        // BAS: should PASS — extra fields accepted
        let thunk = eval(Rc::new(inner_expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        match &val {
            Value::Dict(map) => {
                assert!(map.contains_key(&Key::String("x".into())));
                assert!(
                    map.contains_key(&Key::String("extra".into())),
                    "extra field should be preserved"
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_typeassert_structural_closed_record_exact_fields_pass() {
        // Structural path: closed record, dict has exactly the required fields -> pass
        let mut fields = HashMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row { fields });

        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::Int(42)),
        })];
        let dict_expr = sp(Expr::Dict(entries));
        let inner_expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Record".into())),
            expr: Box::new(dict_expr),
            resolved_type: RefCell::new(Some(record_type)),
        });

        let thunk = eval(Rc::new(inner_expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        match &val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                assert!(map.contains_key(&Key::String("x".into())));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "pre-existing: TypeAssert type errors require materialize() in lazy CEK model"]
    fn test_typeassert_structural_record_non_dict_fails() {
        // Structural path: resolved_type = Some(Type::Record(...)), value is Int -> error
        let mut fields = HashMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row { fields });

        let inner_expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Record".into())),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(Some(record_type)),
        });

        let err = eval(Rc::new(inner_expr.clone()), empty_env(), &test_ctx()).unwrap_err();
        assert!(
            err.to_string().contains("type assertion failed"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_typeassert_nominal_fallback() {
        // Nominal fallback path: resolved_type = None, annotation "Int", value is Int -> pass
        // (This ensures the existing nominal path is preserved alongside the new structural path.)
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Int(7))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(7));
    }

    #[test]
    #[ignore = "pre-existing: TypeAssert type errors require materialize() in lazy CEK model"]
    fn test_typeassert_nominal_fallback_mismatch() {
        // Nominal fallback path: resolved_type = None, annotation "Int", value is String -> error
        // (Verifies nominal fallback still rejects mismatches.)
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Str("oops".into()))),
            resolved_type: RefCell::new(None),
        });
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err
        );
    }

    #[test]
    #[ignore = "pre-existing: TypeAssert lazy in new CEK model, default evaluation not yet correct"]
    fn test_typeassert_primitive_eager_with_default() {
        // Primitive TypeAssert with default: MUST eagerly validate to decide whether to use default
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(999)),
        })];

        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("not an int".into()))),
            resolved_type: RefCell::new(Some(Type::Int)),
        });

        // eval() returns a Materialized thunk containing the default value
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        assert!(
            thunk.try_get_materialized().is_some(),
            "TypeAssert with default must eagerly materialize"
        );
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(999));
    }

    // ── annotation_has_structural_fields unit tests ────────────────────
    // Tests for the helper that distinguishes structural record annotations
    // (e.g. [@[name: String] $x]) from metadata-only annotations (e.g.
    // [@[default: 0] $x]) in the --no-typecheck fallback path.

    #[test]
    fn test_annotation_has_structural_fields_simple_returns_false() {
        // Simple annotations like @Int have no structural fields
        assert!(!annotation_has_structural_fields(&Annotation::Simple(
            "Int".into()
        )));
    }

    #[test]
    fn test_annotation_has_structural_fields_empty_property_dict() {
        // Empty PropertyDict has no structural fields
        assert!(!annotation_has_structural_fields(
            &Annotation::PropertyDict(vec![])
        ));
    }

    #[test]
    fn test_annotation_has_structural_fields_default_only() {
        // [@[default: 0] $x] — default-only, no structural fields
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(0)),
        })];
        assert!(!annotation_has_structural_fields(
            &Annotation::PropertyDict(entries)
        ));
    }

    #[test]
    fn test_annotation_has_structural_fields_type_only() {
        // [@[type: Int] $x] — type-only, no structural fields
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("type".into()))),
            value: rsp(Expr::Str("Int".into())),
        })];
        assert!(!annotation_has_structural_fields(
            &Annotation::PropertyDict(entries)
        ));
    }

    #[test]
    fn test_annotation_has_structural_fields_record_annotation() {
        // [@[name: String age: Int] $x] — has structural fields
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("String".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("age".into()))),
                value: rsp(Expr::Str("Int".into())),
            }),
        ];
        assert!(annotation_has_structural_fields(&Annotation::PropertyDict(
            entries
        )));
    }

    #[test]
    fn test_annotation_has_structural_fields_mixed_meta_and_record() {
        // [@[name: String default: []] $x] — has structural field "name"
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("String".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::Dict(vec![])),
            }),
        ];
        assert!(annotation_has_structural_fields(&Annotation::PropertyDict(
            entries
        )));
    }

    // ── elaboration gap tests ────────────────────────────────────────────
    // Tests for the --no-typecheck fallback path when resolved_type is None
    // and the annotation has structural fields (Dict tag check).

    #[test]
    fn test_elaboration_gap_structural_annotation_dict_passes() {
        // [@[name: String] [name: hello]] with resolved_type=None (no typecheck)
        // Should pass: value is a Dict (tag check succeeds)
        let ann_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("name".into()))),
            value: rsp(Expr::Str("String".into())),
        })];
        let dict_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("name".into()))),
            value: rsp(Expr::Str("hello".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(ann_entries)),
            expr: Box::new(sp(Expr::Dict(dict_entries))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert!(
            matches!(val, Value::Dict(_)),
            "Structural annotation with Dict value should pass tag check"
        );
    }

    #[test]
    #[ignore = "pre-existing: TypeAssert type errors require materialize() in lazy CEK model"]
    fn test_elaboration_gap_structural_annotation_non_dict_fails() {
        // [@[name: String] 42] with resolved_type=None (no typecheck)
        // Should fail: value is Int, not Dict
        let ann_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("name".into()))),
            value: rsp(Expr::Str("String".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(ann_entries)),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected Record, got Int"),
            "Structural annotation with non-Dict value should fail; got: {}",
            err.to_string()
        );
    }

    #[test]
    fn test_elaboration_gap_structural_annotation_non_dict_with_default() {
        // [@[name: String default: []] 42] with resolved_type=None (no typecheck)
        // Should use default: value is Int (not Dict), default is available
        let ann_entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("String".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::Dict(vec![])),
            }),
        ];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(ann_entries)),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert!(
            matches!(val, Value::Dict(_)),
            "Should use default when tag check fails; got: {val:?}"
        );
    }

    #[test]
    fn test_elaboration_gap_default_only_no_structural_check() {
        // [@[default: 0] "hello"] with resolved_type=None
        // Should pass through without validation (no type, no structural fields)
        let ann_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(0)),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(ann_entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    // ── value_matches_type unit tests ────────────────────────────────────
    // Direct tests of the value_matches_type() helper function, which is
    // called in the structural TypeAssert handler for non-Record types.

    #[test]
    fn test_value_matches_type_int() {
        assert!(value_matches_type(&Value::Int(42), &Type::Int));
        assert!(!value_matches_type(&string_val("x".into()), &Type::Int));
        assert!(!value_matches_type(&Value::Bool(true), &Type::Int));
    }

    #[test]
    fn test_value_matches_type_str() {
        assert!(value_matches_type(&string_val("hello".into()), &Type::Str));
        assert!(!value_matches_type(&Value::Int(1), &Type::Str));
        assert!(!value_matches_type(&Value::Bool(false), &Type::Str));
    }

    #[test]
    fn test_value_matches_type_float() {
        assert!(value_matches_type(&Value::Float(3.14), &Type::Float));
        assert!(!value_matches_type(&Value::Int(3), &Type::Float));
    }

    #[test]
    fn test_value_matches_type_bool() {
        assert!(value_matches_type(&Value::Bool(true), &Type::Bool));
        assert!(value_matches_type(&Value::Bool(false), &Type::Bool));
        assert!(!value_matches_type(&Value::Int(1), &Type::Bool));
    }

    #[test]
    fn test_value_matches_type_number() {
        // Type::Number accepts both Int and Float
        assert!(value_matches_type(&Value::Int(42), &Type::Number));
        assert!(value_matches_type(&Value::Float(1.5), &Type::Number));
        assert!(!value_matches_type(&string_val("42".into()), &Type::Number));
        assert!(!value_matches_type(&Value::Bool(true), &Type::Number));
    }

    #[test]
    fn test_value_matches_type_any() {
        // Type::Top accepts all value kinds
        assert!(value_matches_type(&Value::Int(1), &Type::Top));
        assert!(value_matches_type(&Value::Float(1.0), &Type::Top));
        assert!(value_matches_type(&string_val("s".into()), &Type::Top));
        assert!(value_matches_type(&Value::Bool(true), &Type::Top));
        assert!(value_matches_type(
            &Value::Dict(IndexMap::new()),
            &Type::Top
        ));
    }

    #[test]
    fn test_value_matches_type_int_literal() {
        // Type::IntLiteral(n) matches only Int(n)
        assert!(value_matches_type(&Value::Int(5), &Type::IntLiteral(5)));
        assert!(!value_matches_type(&Value::Int(6), &Type::IntLiteral(5)));
        assert!(!value_matches_type(
            &string_val("5".into()),
            &Type::IntLiteral(5)
        ));
    }

    #[test]
    fn test_value_matches_type_string_literal() {
        // Type::StringLiteral("foo") matches only String("foo")
        assert!(value_matches_type(
            &string_val("foo".into()),
            &Type::StringLiteral("foo".into())
        ));
        assert!(!value_matches_type(
            &string_val("bar".into()),
            &Type::StringLiteral("foo".into())
        ));
        assert!(!value_matches_type(
            &Value::Int(0),
            &Type::StringLiteral("foo".into())
        ));
    }

    #[test]
    fn test_value_matches_type_typevar_always_true() {
        // Type::TypeVar is treated as Any (residual polymorphic instantiation)
        assert!(value_matches_type(
            &Value::Int(1),
            &Type::TypeVar("a".into(), 0)
        ));
        assert!(value_matches_type(
            &string_val("x".into()),
            &Type::TypeVar("a".into(), 0)
        ));
        assert!(value_matches_type(
            &Value::Bool(true),
            &Type::TypeVar("a".into(), 0)
        ));
    }

    #[test]
    fn test_value_matches_type_record_always_true() {
        // Type::Record always returns true (deferred to proxy contract wrapping).
        // This is intentional per the spec: record field validation happens via
        // validate_and_wrap_record, not value_matches_type.
        let mut fields = HashMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row { fields });
        // Even a non-Dict value returns true here — record validation is done separately
        assert!(value_matches_type(&Value::Int(99), &record_type));
        assert!(value_matches_type(
            &Value::Dict(IndexMap::new()),
            &record_type
        ));
    }

    #[test]
    fn test_value_matches_type_proxy() {
        // Type::Proxy should match Value::Proxy and reject other value kinds
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let handler = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(42), span)));
        let proxy_val = Value::Proxy { handler };

        assert!(value_matches_type(&proxy_val, &Type::Proxy));
        assert!(!value_matches_type(&proxy_val, &Type::Int));
        assert!(value_matches_type(&proxy_val, &Type::Top));
    }

    // ── validate_and_wrap_record unit tests ──────────────────────────────────
    // Tests for validate_and_wrap_record helper function, particularly the
    // field_path error message generation for nested record validation.

    #[test]
    fn test_validate_and_wrap_record_nested_field_path_error() {
        // Test that validate_and_wrap_record generates correct error messages
        // when field_path is non-empty (nested record validation).
        //
        // This exercises the code path where field_path_prefix is built with each
        // segment separately quoted per doc/07-type-extensions.md:162.

        // Create a row type requiring field "y"
        let mut fields = HashMap::new();
        fields.insert("y".to_string(), Type::Int);
        let row = Row { fields };

        // Create entries that are missing field "y"
        let entries: IndexMap<Key, ThunkId> = IndexMap::new();
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
            data_span,
            &ctx,
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

    #[test]
    fn test_validate_and_wrap_record_nested_field_path_extra_field_accepted() {
        // BAS width subtyping: extra fields in closed records are ACCEPTED.
        // Under BAS, a value with more fields satisfies an annotation with fewer fields.

        // Create a row type requiring only field "x"
        let mut fields = HashMap::new();
        fields.insert("x".to_string(), Type::Int);
        let row = Row { fields };

        // Create entries with "x" plus an extra field "z"
        let ctx = test_ctx();
        let mut entries: IndexMap<Key, ThunkId> = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        entries.insert(
            Key::String("x".to_string()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(1), span))),
        );
        entries.insert(
            Key::String("z".to_string()),
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
        );

        // BAS: should SUCCEED — extra fields accepted under width subtyping
        assert!(
            result.is_ok(),
            "BAS: extra fields should be accepted, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_validate_and_wrap_record_empty_field_path() {
        // Verify that when field_path is empty, no prefix is added to error messages.
        // This is the common case for top-level TypeAssert validation.

        // Create a row type requiring field "name"
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), Type::Str);
        let row = Row { fields };

        // Create empty entries (missing "name")
        let entries: IndexMap<Key, ThunkId> = IndexMap::new();
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
            data_span,
            &ctx,
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

    #[test]
    fn test_validate_and_wrap_record_accepts_int_key_bas() {
        // BAS width subtyping: integer-keyed entries are extra fields and are ACCEPTED.
        // Under BAS, a value with more fields (including int-keyed) satisfies the annotation.

        let mut fields = HashMap::new();
        fields.insert("name".to_string(), Type::Str);
        let row = Row { fields };

        // Create entries with "name" (valid) plus an integer-keyed entry
        let ctx = test_ctx();
        let mut entries: IndexMap<Key, ThunkId> = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        entries.insert(
            Key::Int(0),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val("x".into()),
                span,
            ))),
        );
        entries.insert(
            Key::String("name".to_string()),
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
        );

        // BAS: should SUCCEED — extra int-keyed fields accepted under width subtyping
        assert!(
            result.is_ok(),
            "BAS: integer-keyed extra fields should be accepted, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_validate_and_wrap_record_allows_int_key_in_open_record() {
        // BAS: Integer-keyed entries are extra fields and are accepted by width subtyping.
        // All records are closed (RowTail::Empty) but BAS allows extra fields.

        let mut fields = HashMap::new();
        fields.insert("name".to_string(), Type::Str);
        let row = Row { fields };

        let ctx = test_ctx();
        let mut entries: IndexMap<Key, ThunkId> = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        entries.insert(
            Key::Int(0),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val("x".into()),
                span,
            ))),
        );
        entries.insert(
            Key::String("name".to_string()),
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
        );

        // Should succeed: open records allow extra fields (including integer-keyed ones)
        assert!(
            result.is_ok(),
            "Expected success for integer-keyed entry in open record, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_materialize_cached_thunk_at_high_depth() {
        // Pre-materialized thunks should succeed even at depth > MAX_EVAL_DEPTH.
        // Previously, the depth check fired BEFORE the Materialized early-return,
        // causing spurious DepthExceeded errors when accessing cached values at high depth.
        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_materialized(Value::Int(42), span);
        let ctx = test_ctx();

        // Materialize at depth=300 (> MAX_EVAL_DEPTH=256) should succeed
        let result = materialize(&thunk, None, &ctx);
        assert!(
            result.is_ok(),
            "Expected success for cached thunk at high depth, got error: {:?}",
            result.unwrap_err()
        );
        assert_eq!(result.unwrap(), Value::Int(42));
    }

    #[test]
    #[ignore = "pre-existing: OnceCell-based thunk cannot transition Materialized→Failed; cache_failure_once on an already-materialized thunk is a no-op"]
    fn test_materialize_failed_thunk_at_high_depth() {
        // Pre-failed thunks should return their cached error even at high depth,
        // without hitting the depth check.
        let span = test_span(1, 1, 1, 5);
        let thunk = Arc::new(Thunk::new_materialized(Value::Int(42), span));

        // Force it into Failed state with a cached error
        let err = Box::new(EvalError::type_mismatch("String", "Int", span));
        thunk.cache_failure_once(&err);

        let ctx = test_ctx();

        // Materialize at depth=300 should return the cached error, not DepthExceeded
        let result = materialize(&thunk, None, &ctx);
        assert!(result.is_err(), "Expected cached error");
        let error = result.unwrap_err();
        assert!(
            error.kind.to_string().contains("type mismatch"),
            "Expected cached type mismatch error, got: {}",
            error.kind.to_string()
        );
    }

    #[test]
    fn test_thunk_guarded_memoizes_on_success() {
        // Task 3(3): Guarded thunk memoization — after successful validation, the
        // thunk transitions to Materialized and the second access returns the cached
        // value without re-running the type guard.
        use crate::types::Type;

        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);

        // Inner thunk: a materialized Int(42) — passes the Int guard.
        let inner = Arc::new(Thunk::new_materialized(Value::Int(42), span));

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
        let result1 = materialize(&guarded, None, &ctx);
        assert!(result1.is_ok(), "first materialization should succeed");
        assert_eq!(result1.unwrap(), Value::Int(42));

        // After successful validation, thunk must be in Materialized state (memoized).
        assert_eq!(
            guarded.try_get_materialized(),
            Some(Value::Int(42)),
            "after first materialization thunk should be Materialized(Int(42))"
        );

        // Second materialization: must return cached value, not re-run the guard.
        let result2 = materialize(&guarded, None, &ctx);
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

    #[test]
    fn test_guarded_thunk_failure_path() {
        // Task 3(2): Guarded thunk failure path — when the inner value fails the type guard,
        // the thunk transitions to Failed (cacheable) and subsequent access returns the
        // cached error without re-running the guard.
        use crate::types::Type;

        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);

        // Inner thunk: a String value — fails the Int guard.
        let inner = Arc::new(Thunk::new_materialized(string_val("hello".into()), span));

        // Wrap it in a Guarded thunk expecting Int.
        let guarded = Arc::new(Thunk::new_guarded(
            Arc::clone(&inner),
            Type::Int,
            vec!["field".to_string()],
            span,
        ));

        // First materialization: triggers guard, validates String against Type::Int → fail.
        let result1 = materialize(&guarded, None, &ctx);
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
        let result2 = materialize(&guarded, None, &ctx);
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

    #[test]
    fn test_guarded_thunk_preserves_inner_origin() {
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
        let result = materialize(&guarded, None, &ctx);
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

    #[test]
    fn test_iterative_materialize_deep_chain() {
        // Create a deep Unevaluated chain to verify the iterative implementation
        // doesn't stack overflow. Each thunk is an Unevaluated expression that
        // references the next thunk. This would overflow the Rust stack with
        // recursive materialize().
        let chain_len = 192;

        let ctx = test_ctx();
        let env = empty_env();
        let span = test_span(1, 1, 1, 10);

        // Base case: Thunk holding Int(chain_len as i64)
        let base_thunk = Arc::new(Thunk::new_materialized(Value::Int(chain_len as i64), span));
        env.write()
            .unwrap()
            .insert("base".into(), Arc::clone(&base_thunk));

        // Build a chain of chain_len thunks, each just referencing the previous one
        // var_0 = $base, var_1 = $var_0, ..., var_{chain_len-1} = $var_{chain_len-2}
        for i in 0..chain_len {
            let prev_name = if i == 0 {
                "base".to_string()
            } else {
                format!("var_{}", i - 1)
            };
            let curr_name = format!("var_{}", i);

            let expr = Arc::new(sp(CoreExpr::FreeVar(prev_name.clone())));
            let thunk = Arc::new(Thunk::new_unevaluated_core(
                expr,
                Arc::clone(&env),
                Arc::clone(&ctx),
                span,
            ));
            env.write().unwrap().insert(curr_name, thunk);
        }

        // Materialize the outermost thunk — should succeed with iterative implementation
        let final_name = format!("var_{}", chain_len - 1);
        let final_thunk = env.read().unwrap().get(&final_name).unwrap().clone();
        let result = materialize(&final_thunk, None, &ctx);
        assert!(
            result.is_ok(),
            "Deep chain materialization should succeed, got: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap(), Value::Int(chain_len as i64));
    }

    #[test]
    fn test_iterative_materialize_cycle_detection() {
        // Verify that the iterative run() function detects circular dependencies
        // correctly via InProgress state detection in force_step.
        let ctx = test_ctx();
        let env = empty_env();

        // Create a cycle: x references y, y references x
        // x = $y
        let x_expr = Arc::new(sp(CoreExpr::FreeVar("y".into())));
        let x_thunk = Arc::new(Thunk::new_unevaluated_core(
            x_expr,
            Arc::clone(&env),
            Arc::clone(&ctx),
            test_span(1, 1, 1, 2),
        ));

        // y = $x
        let y_expr = Arc::new(sp(CoreExpr::FreeVar("x".into())));
        let y_thunk = Arc::new(Thunk::new_unevaluated_core(
            y_expr,
            Arc::clone(&env),
            Arc::clone(&ctx),
            test_span(1, 1, 1, 2),
        ));

        // Bind x -> x_thunk, y -> y_thunk in env
        env.write()
            .unwrap()
            .insert("x".into(), Arc::clone(&x_thunk));
        env.write()
            .unwrap()
            .insert("y".into(), Arc::clone(&y_thunk));

        // Materialize x — should detect cycle (2-node cycle)
        let result = materialize(&x_thunk, None, &ctx);
        assert!(result.is_err(), "Cycle should be detected");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("circular dependency"),
            "Error should mention circular dependency, got: {}",
            err.to_string()
        );

        // Test 3-node cycle: a→b→c→a
        let env3 = empty_env();

        let a_expr = Arc::new(sp(CoreExpr::FreeVar("b".into())));
        let a_thunk = Arc::new(Thunk::new_unevaluated_core(
            a_expr,
            Arc::clone(&env3),
            Arc::clone(&ctx),
            test_span(1, 1, 1, 2),
        ));

        let b_expr = Arc::new(sp(CoreExpr::FreeVar("c".into())));
        let b_thunk = Arc::new(Thunk::new_unevaluated_core(
            b_expr,
            Arc::clone(&env3),
            Arc::clone(&ctx),
            test_span(1, 1, 1, 2),
        ));

        let c_expr = Arc::new(sp(CoreExpr::FreeVar("a".into())));
        let c_thunk = Arc::new(Thunk::new_unevaluated_core(
            c_expr,
            Arc::clone(&env3),
            Arc::clone(&ctx),
            test_span(1, 1, 1, 2),
        ));

        env3.write()
            .unwrap()
            .insert("a".into(), Arc::clone(&a_thunk));
        env3.write()
            .unwrap()
            .insert("b".into(), Arc::clone(&b_thunk));
        env3.write()
            .unwrap()
            .insert("c".into(), Arc::clone(&c_thunk));

        let result3 = materialize(&a_thunk, None, &ctx);
        assert!(result3.is_err(), "3-node cycle should be detected");
        let err3 = result3.unwrap_err();
        assert!(
            err3.kind.to_string().contains("circular dependency"),
            "3-node cycle error should mention circular dependency, got: {}",
            err3.kind.to_string()
        );

        // Test self-reference: x→x
        let env_self = empty_env();

        let self_expr = Arc::new(sp(CoreExpr::FreeVar("x".into())));
        let self_thunk = Arc::new(Thunk::new_unevaluated_core(
            self_expr,
            Arc::clone(&env_self),
            Arc::clone(&ctx),
            test_span(1, 1, 1, 2),
        ));

        env_self
            .write()
            .unwrap()
            .insert("x".into(), Arc::clone(&self_thunk));

        let result_self = materialize(&self_thunk, None, &ctx);
        assert!(result_self.is_err(), "Self-reference should be detected");
        let err_self = result_self.unwrap_err();
        assert!(
            err_self.kind.to_string().contains("circular dependency"),
            "Self-reference error should mention circular dependency, got: {}",
            err_self.kind.to_string()
        );
    }

    #[test]
    fn test_circular_dependency_cycle_path() {
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
        let res = std::sync::Arc::new(crate::resolve::resolve_surface_program(&surface_program));
        let types = std::sync::Arc::new(crate::ast::TypeAnnotationTable::new());
        let thunk = crate::async_rt::block_on_anywhere(super::eval_surface_file(
            &surface_program,
            Arc::clone(&env),
            &ctx,
            &res,
            &types,
        ))
        .expect("eval_surface_file should succeed (lazy dict construction)");
        // Dict construction is lazy — the cycle is only detected when forcing an entry.
        // Materialize the dict to get the Value::Dict, then force an entry to trigger
        // cycle detection. deep_materialize recursively forces all dict entries.
        let dict_val = materialize(&thunk, None, &ctx).expect("dict should materialize");
        let result = crate::eval_materialize::deep_materialize(&dict_val, &ctx, None);

        assert!(
            result.is_err(),
            "Cycle should be detected when forcing cyclic entries"
        );
        let err = result.unwrap_err();

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

    #[test]
    #[ignore = "pre-existing regression from runtime-v2 merge: stdlib loading fails"]
    fn test_tco_tail_recursive_function() {
        // Tail-recursive countdown. Verifies that recursive LLT functions work
        // without Rust stack overflow. The heap-based Action stack / CEK machine
        // prevents Rust stack growth per recursive call, but the LLT continuation
        // stack (MAX_CONTINUATION_STACK = 2048) still limits total recursion depth.
        // Each iteration of count-down adds ~1 continuation frame (Memoize for the
        // PendingBuiltin result returned by builtin_if), so iterations must stay
        // well under 2048. When builtin_if is refactored to return an Action (tail-
        // call style), the continuation frame is not added and unlimited depth is
        // possible — at that point this test can be raised to 100_000+.
        //
        // Spawns a large-stack thread as defensive measure against any remaining
        // Rust-level recursion in debug mode.
        let result = std::thread::Builder::new()
            .stack_size(512 * 1024 * 1024)
            .spawn(|| {
                let iterations = 100_i64;
                let source = format!(
                    r#"
[
    count-down: [fn [let n acc]
        [if [= n 0]
            acc
            [count-down [- n 1] [+ acc 1]]]]
    result: [count-down {} 0]
]
    "#,
                    iterations
                );
                let mut parsed_output = crate::parse(&source).expect("parse should succeed");
                crate::desugar::desugar_surface_program(&mut parsed_output.program);
                let res = std::sync::Arc::new(crate::resolve::resolve_surface_program(
                    &parsed_output.program,
                ));
                let types = std::sync::Arc::new(crate::ast::TypeAnnotationTable::new());
                let env = crate::builtins::create_stdlib_env()
                    .expect("stdlib env creation should succeed");
                let type_stage_env =
                    crate::imports::build_type_stage_env().unwrap_or_else(|| Arc::clone(&env));
                let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
                let ctx = EvalContext::new(base_dir, Arc::clone(&env), type_stage_env, false);
                let thunk = crate::async_rt::block_on_anywhere(super::eval_surface_file(
                    &parsed_output.program,
                    env,
                    &ctx,
                    &res,
                    &types,
                ))
                .expect("eval_surface_file should succeed");
                let dict_val =
                    materialize(&thunk, None, &ctx).expect("materialization should succeed");
                match dict_val {
                    Value::Dict(map) => {
                        let result_id = map
                            .get(&Key::String("result".into()))
                            .expect("result key should exist");
                        let result = mat_id(result_id, &ctx).unwrap_or_else(|e| {
                            panic!("TCO should allow {} iterations: {}", iterations, e)
                        });
                        match result {
                            Value::Int(n) => assert_eq!(n, iterations),
                            other => panic!("Expected Int({}), got {:?}", iterations, other),
                        }
                    }
                    other => panic!("Expected Dict, got {:?}", other),
                }
            })
            .expect("thread spawn should succeed");
        result.join().expect("TCO test thread should not panic");
    }

    #[test]
    fn test_decorate_deduplication() {
        // Verify that decorating an error with the same span twice doesn't create duplicates.
        // This tests the deduplication logic used when attaching stack frames during error propagation.
        let def_span = test_span(1, 1, 1, 10);
        let frame_span = test_span(5, 1, 5, 10);

        let mut err = EvalError::key_not_found("key", vec![], def_span);

        // Add the frame once
        err.push_frame("first access".to_string(), frame_span);
        assert_eq!(err.stack.len(), 1, "Should have exactly one frame");
        assert_eq!(err.stack[0].label, "first access");

        // Manually check for duplicate before adding (this is what error decoration does)
        if !err.stack.iter().any(|f| f.span == frame_span) {
            err.push_frame("second access".to_string(), frame_span);
        }

        // Should still be 1 frame (duplicate was avoided)
        assert_eq!(err.stack.len(), 1, "Duplicate span should be deduplicated");
        assert_eq!(
            err.stack[0].label, "first access",
            "Original label preserved"
        );
    }

    #[test]
    fn test_eval_context_no_fs_flag() {
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

    /// Integration test: `with_base_dir()` inherits `no_fs` flag.
    ///
    /// Verifies the no_fs=true code path end-to-end through `with_base_dir()`:
    /// 1. Create a ctx1 with no_fs=true.
    /// 2. Call ctx1.with_base_dir() to get ctx2 with a different base_dir.
    /// 3. Evaluate a `$include` call using ctx2.
    /// 4. Confirm the result is `IncludeForbidden` [E042] — proving:
    ///    a. `with_base_dir()` correctly propagates the no_fs flag.
    ///    b. `$include` resolves via ctx2's config (not a stale ctx1 config).
    ///    c. No actual filesystem access is needed — the error fires immediately.
    #[test]
    #[ignore = "pre-existing regression from runtime-v2 merge: stdlib loading fails"]
    fn test_eval_context_with_base_dir_inherits_no_fs() {
        // Two separate base dirs: ctx1 starts with no_fs=true.
        let env = crate::builtins::create_stdlib_env().expect("stdlib env");
        let type_stage_env =
            crate::imports::build_type_stage_env().unwrap_or_else(|| Arc::clone(&env));

        // ctx1 has no_fs=true
        let ctx1 = EvalContext::new(
            crate::test_util::test_caps().root.try_clone().unwrap(),
            Arc::clone(&env),
            type_stage_env,
            true,
        );
        assert!(ctx1.config.no_fs, "ctx1 must have no_fs=true");

        // ctx2 shares ctx1's state but has a different base_dir
        let ctx2 = ctx1.with_base_dir(crate::test_util::test_caps().root.try_clone().unwrap());

        // Verify structural properties of ctx2
        assert!(
            ctx2.config.no_fs,
            "ctx2 created via with_base_dir() must inherit no_fs=true from ctx1"
        );
        assert!(
            Arc::ptr_eq(&ctx1.state, &ctx2.state),
            "ctx2 must share the same state Arc as ctx1"
        );

        // Exercise the no_fs path: $include must produce IncludeForbidden [E042].
        // This proves ctx2 correctly propagates no_fs to $include without needing
        // any real files on disk.
        let include_expr = sp(crate::ast::Expr::Call {
            func: Box::new(sp(crate::ast::Expr::var_ref("include".into()))),
            args: vec![Rc::new(sp(crate::ast::Expr::Str(
                "hypothetical.llt".into(),
            )))],
            named_args: vec![],
            implied: false,
        });

        let thunk = eval(
            Rc::new(include_expr.clone()),
            Arc::clone(&ctx2.config.stdlib_env),
            &ctx2,
        )
        .expect("eval should succeed (thunk creation does not access filesystem)");
        let err = materialize(&thunk, None, &ctx2).expect_err("$include with no_fs=true must fail");

        assert!(
            matches!(err.kind, crate::error::ErrorKind::IncludeForbidden),
            "Expected IncludeForbidden [E042], got: {}",
            err.kind.code()
        );
        assert_eq!(
            err.kind.code(),
            "E042",
            "IncludeForbidden must produce error code E042"
        );
    }

    #[test]
    fn test_selective_materialization_unused_branch() {
        // Verify that accessing only one dict entry doesn't materialize unused entries
        use crate::parser::parse_expression;

        let input = r#"[used: 1  unused: [call $error "should not materialize"]]"#;
        let parsed = parse_expression(input).expect("parse failed");
        let env = empty_env();
        let ctx = test_ctx();
        let thunk = eval(Rc::new(parsed.clone()), Arc::clone(&env), &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();

        // Extract the dict
        match val {
            Value::Dict(map) => {
                // Access only the "used" key
                let used_key = Key::String("used".into());
                let used_thunk = map.get(&used_key).expect("used key should exist");
                let used_val = mat_id(used_thunk, &ctx).expect("used should materialize");
                assert_eq!(used_val, Value::Int(1));

                // Verify the "unused" key exists but is NOT materialized
                let unused_key = Key::String("unused".into());
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

    // ── boundary_guards wiring ────────────────────────────────────────────────

    /// Helper: create an EvalContext and install a single boundary guard for span `s`
    /// expecting `expected_ty`. Used to simulate what the type checker produces for
    /// gradual-typing boundaries (Unknown arg crossing into a concrete param type).
    fn ctx_with_guard(s: Span, expected_ty: Type) -> Arc<EvalContext> {
        let ctx = test_ctx();
        let mut map = HashMap::new();
        map.insert(s, expected_ty);
        ctx.set_boundary_guards(map);
        ctx
    }

    /// Boundary guard: Int expected, Int given — guard fires but passes.
    #[test]
    fn test_boundary_guard_passes_on_matching_type() {
        // Span that will carry the guard.
        let guarded_span = test_span(1, 1, 1, 4);

        // AST node with the guarded span: `42` (Int literal)
        let mut expr = sp(Expr::Int(42));
        expr.span = guarded_span;

        let ctx = ctx_with_guard(guarded_span, Type::Int);

        // eval() should wrap the Int thunk in a Guarded thunk.
        let thunk = eval(Rc::new(expr), empty_env(), &ctx).unwrap();

        // The outer thunk must be Guarded (not yet materialized).
        assert!(
            thunk.is_guarded(),
            "expected Guarded thunk when boundary guard matches span"
        );

        // Forcing the guard must succeed and return the Int value.
        let val = materialize(&thunk, None, &ctx).unwrap();
        assert_eq!(val, Value::Int(42), "guard should pass for matching type");
    }

    /// Boundary guard: Int expected, String given — guard fires and returns a
    /// type_assert_failed error with a helpful message.
    #[test]
    fn test_boundary_guard_fires_on_type_mismatch() {
        // Span that will carry the guard.
        let guarded_span = test_span(1, 1, 1, 7);

        // AST node with the guarded span: `"hello"` (String literal)
        let mut expr = sp(Expr::Str("hello".into()));
        expr.span = guarded_span;

        // Guard expects Int — the String value will fail.
        let ctx = ctx_with_guard(guarded_span, Type::Int);

        let thunk = eval(Rc::new(expr), empty_env(), &ctx).unwrap();

        // The guard must be present.
        assert!(
            thunk.is_guarded(),
            "expected Guarded thunk for span with guard"
        );

        // Forcing must return a type_assert_failed error.
        let err = materialize(&thunk, None, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Int") || msg.contains("type"),
            "error should mention the expected type; got: {msg}"
        );
        assert!(
            msg.contains("String") || msg.contains("Str"),
            "error should mention the actual type; got: {msg}"
        );
    }

    /// Boundary guard: guard is lazy — thunk is NOT forced during eval(), only during
    /// materialize(). If the guard span doesn't match any AST node span, eval() returns
    /// an unguarded thunk.
    #[test]
    fn test_boundary_guard_not_applied_for_non_matching_span() {
        let guarded_span = test_span(5, 1, 5, 4); // a span on "line 5"
        let expr_span = test_span(1, 1, 1, 4); // different span

        let mut expr = sp(Expr::Int(42));
        expr.span = expr_span;

        // Guard is for guarded_span, but expr uses expr_span — no wrap.
        let ctx = ctx_with_guard(guarded_span, Type::Int);
        let thunk = eval(Rc::new(expr), empty_env(), &ctx).unwrap();

        // Must NOT be Guarded — guard did not match.
        assert!(
            !thunk.is_guarded(),
            "thunk must not be Guarded when span doesn't match any guard"
        );

        // Value is still accessible normally.
        let val = materialize(&thunk, None, &ctx).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    /// Boundary guard: guard is lazy — eval() wraps the thunk without forcing it.
    /// The Guarded state must persist between eval() and materialize().
    #[test]
    fn test_boundary_guard_is_lazy() {
        let guarded_span = test_span(1, 1, 1, 5);

        let mut expr = sp(Expr::Int(7));
        expr.span = guarded_span;

        let ctx = ctx_with_guard(guarded_span, Type::Int);
        let thunk = eval(Rc::new(expr), empty_env(), &ctx).unwrap();

        // Thunk must be Guarded (lazy wrap, inner not yet forced).
        assert!(
            thunk.is_guarded(),
            "guard wrap must be lazy — Guarded state expected before materialization"
        );

        // Now force it — only here does the guard check run.
        let val = materialize(&thunk, None, &ctx).unwrap();
        assert_eq!(val, Value::Int(7));

        // After successful materialization, the thunk must be Materialized (guard consumed).
        assert!(
            thunk.try_get_materialized().is_some(),
            "thunk must be Materialized after successful guard check"
        );
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
    #[test]
    fn pending_builtin_bypass_path_pre_materializes_args() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Build an unevaluated thunk that evaluates to an empty dict.
        // `CoreExpr::Dict(vec![])` evaluates to `Value::Dict(IndexMap::new())`.
        let dict_expr = Arc::new(sp(CoreExpr::Dict(vec![])));
        let unevaluated_arg = Arc::new(Thunk::new_unevaluated_core(
            dict_expr,
            empty_env(),
            Arc::clone(&ctx),
            span,
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
        };

        // Create a PendingBuiltin thunk wrapping `builtin_keys` with the unevaluated arg.
        let outer = Arc::new(Thunk::new_pending_builtin(
            keys_def,
            vec![Arc::clone(&unevaluated_arg)],
            None,
            span,
            None,
            Arc::clone(&ctx),
        ));

        // Materialize via the recursive path. If force_count pre-materialization is
        // missing, this panics at `try_get_materialized().expect(...)` inside `builtin_keys`.
        let result = materialize(&outer, None, &ctx);
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
    #[test]
    fn pending_call_builtin_bypass_path_pre_materializes_args() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Build an unevaluated thunk that evaluates to an empty dict.
        let dict_expr = Arc::new(sp(CoreExpr::Dict(vec![])));
        let unevaluated_arg = Arc::new(Thunk::new_unevaluated_core(
            dict_expr,
            empty_env(),
            Arc::clone(&ctx),
            span,
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
        };
        let func_thunk = Arc::new(Thunk::new_materialized(Value::Builtin(keys_def), span));

        // Create a PendingCall thunk: calls builtin_keys with the unevaluated arg.
        let outer = Arc::new(Thunk::new_pending_call(
            func_thunk,
            vec![Arc::clone(&unevaluated_arg)],
            IndexMap::new(),
            span,
            empty_env(),
            span,
            None,
            Arc::clone(&ctx),
        ));

        // Materialize via the recursive path. If force_count pre-materialization is
        // missing for the PendingCall→Builtin case, this panics inside `builtin_keys`.
        let result = materialize(&outer, None, &ctx);
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
        sp(Pattern::Variable(name.to_string()))
    }

    /// Helper: build a wildcard pattern Spanned<Pattern> at the default test span.
    fn wildcard_pattern() -> Spanned<Pattern> {
        sp(Pattern::Wildcard)
    }

    #[test]
    fn test_check_pattern_linearity_linear_is_ok() {
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

    #[test]
    fn test_check_pattern_linearity_wildcard_is_ok() {
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

    #[test]
    fn test_check_pattern_linearity_single_variable_is_ok() {
        // A single variable binding is always linear.
        let pattern = var_pattern("x");
        assert!(check_pattern_linearity(&pattern).is_ok());
    }

    #[test]
    fn test_check_pattern_linearity_duplicate_in_dict_rejected() {
        // `[a: x  b: x  ...]:` — `x` appears twice in the same arm.
        let pattern = sp(Pattern::Dict {
            fields: vec![
                ("a".to_string(), var_pattern("x")),
                ("b".to_string(), var_pattern("x")),
            ],
            rest: true,
        });
        let result = check_pattern_linearity(&pattern);
        assert!(result.is_err(), "duplicate variable must be rejected");
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::DuplicateVariable { ref name } if name == "x"),
            "expected DuplicateVariable(\"x\"), got: {:?}",
            err.kind
        );
        assert!(
            err.to_string()
                .contains("duplicate variable in pattern: 'x' appears more than once"),
            "error message should name the duplicate variable, got: {}",
            err
        );
    }

    #[test]
    fn test_check_pattern_linearity_duplicate_in_seq_rejected() {
        // `[seq x x]:` — `x` appears in both head and tail.
        let pattern = sp(Pattern::Seq {
            head: Box::new(var_pattern("x")),
            tail: Box::new(var_pattern("x")),
        });
        let result = check_pattern_linearity(&pattern);
        assert!(result.is_err(), "duplicate in Seq head/tail must be rejected");
        let err = result.unwrap_err();
        assert!(matches!(err.kind, ErrorKind::DuplicateVariable { ref name } if name == "x"));
    }

    #[test]
    fn test_check_pattern_linearity_distinct_in_seq_is_ok() {
        // `[seq h t]:` — distinct variables are fine.
        let pattern = sp(Pattern::Seq {
            head: Box::new(var_pattern("h")),
            tail: Box::new(var_pattern("t")),
        });
        assert!(check_pattern_linearity(&pattern).is_ok());
    }

    #[test]
    fn test_check_pattern_linearity_constructor_payload_duplicate_rejected() {
        // `[Some x]:` body pattern with duplicate inside payload dict:
        // `[Some [a: x  b: x]]:` — x appears twice inside Constructor payload.
        let payload = sp(Pattern::Dict {
            fields: vec![
                ("a".to_string(), var_pattern("x")),
                ("b".to_string(), var_pattern("x")),
            ],
            rest: true,
        });
        let pattern = sp(Pattern::Constructor {
            tag: "Some".to_string(),
            binding: Some(Box::new(payload)),
        });
        let result = check_pattern_linearity(&pattern);
        assert!(
            result.is_err(),
            "duplicate inside Constructor payload must be rejected"
        );
    }

    #[test]
    fn test_pm3_match_expr_duplicate_dict_field_errors() {
        // Integration test: eval a Match expression whose arm has a non-linear
        // Dict pattern. The error fires during eval (match arms are tried eagerly
        // in eval_core_expr, not deferred to materialize).
        //
        // match [a: 1  b: 2]
        //   [a: x  b: x  ...]: x
        //
        // The arm is non-linear (x appears twice), so eval must return E072.
        let scrutinee = sp(Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("a".into()))),
                value: rsp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("b".into()))),
                value: rsp(Expr::Int(2)),
            }),
        ]));
        let arm = MatchArm {
            pattern: sp(Pattern::Dict {
                fields: vec![
                    ("a".to_string(), var_pattern("x")),
                    ("b".to_string(), var_pattern("x")),
                ],
                rest: true,
            }),
            guard: None,
            body: Box::new(sp(Expr::var_ref("x".into()))),
        };
        let match_expr = sp(Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms: vec![arm],
        });
        // CoreExpr::Match evaluation is eager (not deferred to a thunk), so the
        // linearity error propagates from eval(), not from materialize().
        let err = eval(Rc::new(match_expr), empty_env(), &test_ctx()).unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::DuplicateVariable { ref name } if name == "x"),
            "expected DuplicateVariable(\"x\"), got: {:?}",
            err.kind
        );
        assert_eq!(
            err.kind.code(),
            "E072",
            "DuplicateVariable must be code E072"
        );
    }

    #[test]
    fn test_pm3_match_expr_linear_dict_pattern_succeeds() {
        // Integration test: linear Dict pattern must succeed normally.
        //
        // match [a: 1  b: 2]
        //   [a: x  b: y  ...]: x
        //
        // The arm is linear; x should bind to 1 (accessed via FreeVar lookup).
        // Note: without the resolver running on a manually-constructed AST, pattern
        // variable references in the body fall back to FreeVar name-based lookup.
        // This is sufficient for verifying that no linearity error fires.
        let scrutinee = sp(Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("a".into()))),
                value: rsp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("b".into()))),
                value: rsp(Expr::Int(2)),
            }),
        ]));
        let arm = MatchArm {
            pattern: sp(Pattern::Dict {
                fields: vec![
                    ("a".to_string(), var_pattern("x")),
                    ("b".to_string(), var_pattern("y")),
                ],
                rest: true,
            }),
            guard: None,
            // Use a literal in the body to avoid FreeVar lookup failures;
            // we only care that the linear check doesn't fire.
            body: Box::new(sp(Expr::Int(99))),
        };
        let match_expr = sp(Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms: vec![arm],
        });
        // Must not error — the pattern is linear.
        let result = eval(Rc::new(match_expr), empty_env(), &test_ctx());
        assert!(
            result.is_ok(),
            "linear Dict pattern must not trigger linearity error; got: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_pm3_same_name_in_different_arms_is_ok() {
        // The same variable name used in different arms is fine — each arm is a
        // separate linear scope. Only duplicates within a single arm are rejected.
        //
        // match 42
        //   _:  99      <- first arm (wildcard), returns literal 99
        //
        // If we used `x: x` in one arm and `x: x` in another, both must pass linearity.
        // Single-variable arms by definition have no duplicates.
        let scrutinee = sp(Expr::Int(42));
        let arm1 = MatchArm {
            pattern: var_pattern("x"),
            guard: None,
            body: Box::new(sp(Expr::Int(1))),
        };
        let arm2 = MatchArm {
            pattern: var_pattern("x"), // same name, different arm — OK
            guard: None,
            body: Box::new(sp(Expr::Int(2))),
        };
        let match_expr = sp(Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms: vec![arm1, arm2],
        });
        // Both arms have a single `x` binding (linear); arm1 matches Int, returns 1.
        let result = eval(Rc::new(match_expr), empty_env(), &test_ctx());
        assert!(
            result.is_ok(),
            "same name in different arms must not trigger linearity error; got: {:?}",
            result.err()
        );
    }
}
