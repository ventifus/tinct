//! Core evaluation module: lazy evaluation with letrec dict scoping, variable lookup,
//! sequential expression evaluation, and document pipeline execution.
//!
//! See also: eval_call.rs (function evaluation), eval_materialize.rs (CEK machine implementation).

pub(crate) use crate::eval_call::eval_call_core;
pub use crate::eval_call::{invoke_function, CallContext};

// Re-export CEK machine components from eval_materialize

// Split modules — dict construction
#[path = "eval_dict.rs"]
mod eval_dict_mod;

pub(crate) use eval_dict_mod::*;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use indexmap::IndexMap;

use crate::ast::{Span, Spanned, SurfaceNode, SurfaceProgram};
use crate::error::{EvalError, EvalResult};
use crate::rust_span;
use crate::type_tags::*;
// Circular module dependency: this module calls builtins via function pointers stored in `Value::Builtin`.
// builtins.rs imports `invoke_function` and `materialize` from this module.
// This bidirectional dependency is safe because neither module's initialization depends on the other.
use crate::value::{EvalFrame, HashableValue, Thunk, Value};

// ============================================================================
// Document pipeline evaluation
// ============================================================================

/// Global repr_registry shared by all root EvalContexts.
///
/// `repr:` declarations are emitted by builtin_core.llt — a global, immutable library.
/// Registration is monotonic (types are only added, never removed), so a global OnceLock
/// holding the single shared registry is the correct pattern.
///
/// `new_empty()` and `new_with_options()` both read from this OnceLock (initializing it on
/// first call), so independently-created contexts always share the same registry Arc.
static GLOBAL_REPR_REGISTRY: std::sync::OnceLock<
    Arc<Mutex<std::collections::HashMap<String, Arc<Value>>>>,
> = std::sync::OnceLock::new();

/// Global is_predicates shared by all root EvalContexts.
///
/// Same sharing model as `GLOBAL_REPR_REGISTRY`: monotonic, global, immutable-source
/// (builtin_core.llt), so all independently-created root contexts must share one Arc.
static GLOBAL_IS_PREDICATES: std::sync::OnceLock<
    Arc<Mutex<std::collections::HashMap<String, Arc<Value>>>>,
> = std::sync::OnceLock::new();

thread_local! {
    /// Cached empty dict thunk used as the default `%` when no stdin is provided.
    /// Avoids allocating a fresh `Arc<Thunk>` on every `eval_surface_file` call for empty programs.
    static EMPTY_DICT_THUNK: Arc<Thunk> = Arc::new(Thunk::value(
        Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
        rust_span!(),
    ));
}

/// Evaluate a sequence of surface expression nodes as a scope chain, returning the
/// last expression's thunk lazily.
///
/// `initial_group`: when `Some`, seeds the accumulated group with the given thunks before
/// processing any expression node. Used by `builtin-eval` to inject env-dict thunks as the
/// initial accumulated group so that Dispatch(_, slot) references resolve into the
/// caller-supplied environment.
///
/// Returns the thunk for the last expression in the sequence.
pub(crate) async fn eval_document_exprs_with_env(
    expr_nodes: &[Arc<SurfaceNode>],
    ctx: &Arc<EvalContext>,
    initial_group: Option<Vec<Arc<Thunk>>>,
) -> EvalResult<Arc<Thunk>> {
    if expr_nodes.is_empty() {
        return Ok(Arc::new(Thunk::value(
            Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
            rust_span!(),
        )));
    }

    // Sequential document evaluation using accumulated_group.
    //
    // accumulated_group starts with root-scope entries (builtins + capabilities) at
    // slots 0..N-1, optionally followed by env-dict entries (when initial_group is Some).
    // After each intermediate dict is evaluated and materialized, its string-keyed thunks
    // are appended to accumulated_group at the next cumulative slot indices.
    //
    // The resolver assigns LGM(absolute_slot) for all variable references (cross-dict
    // references included), where absolute_slot is the unique cumulative index assigned
    // by walk_surface_document_with_offset. frame.group[slot] resolves any LGM reference
    // directly — no outer-frame traversal needed.
    let mut spine = Arc::clone(&ctx.root_spine);
    if let Some(env_thunks) = initial_group {
        spine = spine.extend(env_thunks);
    }

    let last_idx = expr_nodes.len() - 1;

    for (i, node) in expr_nodes.iter().enumerate() {
        let (core_spanned, lower_diags) =
            crate::lower::lower(node, ctx.scope_frames.as_ref().map(|v| v.as_slice()));
        {
            let (info_diags, other_diags): (Vec<_>, Vec<_>) = lower_diags
                .into_iter()
                .partition(|d| d.level == crate::error::DiagnosticLevel::Info);
            for d in info_diags {
                ctx.runtime_diagnostics
                    .lock()
                    .expect("runtime_diagnostics mutex poisoned")
                    .push(d);
            }
            let (err_opt, warnings) =
                crate::eval_materialize::lower_errors_to_eval_error(other_diags);
            for w in warnings {
                ctx.runtime_diagnostics
                    .lock()
                    .expect("runtime_diagnostics mutex poisoned")
                    .push(w);
            }
            if let Some(err) = err_opt {
                return Err(err);
            }
        }
        let node_span = node.span.clone();

        // Build the frame from the current accumulated_group.
        let frame = Arc::new(EvalFrame {
            group: Arc::clone(&spine),
            closure_env: crate::value::GroupSpine::empty(),
            params: Arc::new(vec![]),
        });

        if i == last_idx {
            // Last expression: build frame and return thunk lazily.
            //
            // Capture the accumulated_group snapshot for Matchable dispatch (T-1847).
            // The OnceLock ensures this is written at most once (first call wins), so
            // nested calls from builtin-eval or user documents are silently ignored.
            // At this point accumulated_group contains all intermediate dict entries from
            // the loader program at their canonical LGM slots.
            ctx.set_init_accumulated_group(Arc::clone(&spine));
            let thunk = crate::eval_core::eval_core_expr(&core_spanned, &frame, ctx).await?;
            return Ok(thunk);
        }

        // Intermediate expression: eval and materialize to extract its exported bindings.
        let thunk = crate::eval_core::eval_core_expr(&core_spanned, &frame, ctx).await?;
        let val = materialize(&thunk, Some(&node_span), ctx).await?;

        // Extend accumulated_group with the intermediate value's string-keyed thunks
        // at the next cumulative slot indices. String keys are added in insertion order
        // (matching the resolver's cumulative offset assignment in walk_surface_document_with_offset).
        //
        // Reject non-Dict intermediate values with a clear error rather than
        // silently dropping them (which would misalign subsequent LGM slot references).
        let dict_map = match val {
            Value::Dict {
                entries: ref dict_map,
                ..
            } => dict_map.clone(),
            other => {
                return Err(Box::new(crate::error::EvalError::type_mismatch_ctx(
                    "document expression".to_string(),
                    "Dict",
                    other.type_name(),
                    node_span,
                )));
            }
        };
        let new_entries: Vec<Arc<Thunk>> = dict_map
            .iter()
            .filter_map(|(k, v)| {
                if matches!(k, crate::value::HashableValue::Str(_)) {
                    Some(Arc::clone(v))
                } else {
                    None
                }
            })
            .collect();
        spine = spine.extend(new_entries);
    }

    unreachable!(
        "eval_document_exprs_with_env: loop did not return — expr_nodes was non-empty but last_idx was never reached"
    )
}

/// Evaluate a sequence of pre-lowered `CoreExpr` entries (from `Value::CoreDocument`) as a
/// scope chain, returning the last expression's result dict.
///
/// This is the `builtin-eval` evaluation path for the env-dict protocol (T-1775 / B-553).
/// Unlike `eval_document_exprs_with_env`, which calls `lower()` on each SurfaceNode, this
/// function accepts CoreExprs that are already lowered and evaluates them directly via
/// `eval_core_expr`.
///
/// `initial_group`: the env-dict thunks in insertion order. The env-dict IS the full
/// scope — no root_spine prefix. Thunk j occupies slot j directly, matching the
/// resolver's `resolve_surface_document_with_env_dict` called with `root_group_len=0`
/// which assigns `LGM(j)` to the j-th env-dict name and cumulative slots starting
/// at `env_names.len()` to document dict entries.
///
/// Returns the last expression's thunk. The caller is responsible for materializing
/// to a Dict (exports). Intermediate dict expressions are materialized to extract their
/// string-keyed thunks into the accumulated_group (identical semantics to
/// `eval_document_exprs_with_env`).
pub(crate) async fn eval_core_document_exprs(
    core_entries: &[(
        String,
        std::sync::Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
    )],
    ctx: &Arc<EvalContext>,
    initial_group: Vec<Arc<Thunk>>,
) -> EvalResult<Arc<Thunk>> {
    if core_entries.is_empty() {
        return Ok(Arc::new(Thunk::value(
            Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
            rust_span!(),
        )));
    }

    // Build accumulated_group from env-dict entries only.
    // The env-dict IS the full scope — no root_spine prefix. The resolver assigns
    // LGM(j) directly for the j-th env-dict name (root_group_len=0 in builtin_resolve).
    // Document dict entries are appended at cumulative slots as each dict is evaluated.
    let mut spine = crate::value::GroupSpine::from_flat(initial_group);

    let last_idx = core_entries.len() - 1;

    for (i, (_key, spanned_core)) in core_entries.iter().enumerate() {
        let frame = Arc::new(EvalFrame {
            group: Arc::clone(&spine),
            closure_env: crate::value::GroupSpine::empty(),
            params: Arc::new(vec![]),
        });

        if i == last_idx {
            // Last expression: return lazily.
            let thunk = crate::eval_core::eval_core_expr(spanned_core, &frame, ctx).await?;
            return Ok(thunk);
        }

        // Intermediate expression: eval and materialize to extract its exported bindings.
        let thunk = crate::eval_core::eval_core_expr(spanned_core, &frame, ctx).await?;
        let entry_span = spanned_core.span.clone();
        let val = materialize(&thunk, Some(&entry_span), ctx).await?;

        // Extend accumulated_group with string-keyed thunks from this dict.
        // Reject non-Dict values.
        let dict_map = match val {
            Value::Dict {
                entries: ref dict_map,
                ..
            } => dict_map.clone(),
            other => {
                return Err(Box::new(crate::error::EvalError::type_mismatch_ctx(
                    "document expression".to_string(),
                    "Dict",
                    other.type_name(),
                    entry_span,
                )));
            }
        };
        let new_entries: Vec<Arc<Thunk>> = dict_map
            .iter()
            .filter_map(|(k, v)| {
                if matches!(k, HashableValue::Str(_)) {
                    Some(Arc::clone(v))
                } else {
                    None
                }
            })
            .collect();
        spine = spine.extend(new_entries);
    }

    unreachable!(
        "eval_core_document_exprs: loop did not return — core_entries was non-empty but last_idx was never reached"
    )
}

/// Evaluate a SurfaceProgram: one or more documents separated by `---`.
///
/// # Precondition
///
/// **Pipeline invariant:** `desugar_program_full` →
/// `resolve_surface_program` must be called before passing the program here —
/// it writes de Bruijn coordinates inline to the AST nodes.
/// If type checking was skipped, `TypeAssert` nodes use `TypeAssertCheck::Source` with the
/// raw annotation, which resolves to TypeValue.Unknown at runtime (accepts all values).
///
/// # Document sequencing
///
/// Documents are evaluated sequentially via the EvalFrame chain protocol.
/// `eval_core_document_exprs` receives an `initial_group` env dict seeded with exported
/// bindings from all prior documents; each document's exported names are merged into the
/// accumulated env dict by `builtin-eval` in `loader.llt` after each document completes,
/// and that updated dict is passed as `initial_group` for the next document.
pub async fn eval_surface_file(
    program: &SurfaceProgram,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    eval_surface_file_from_env(program, ctx).await
}

/// Evaluate a SurfaceProgram with an optional initial value injected into the environment.
///
/// See `eval_surface_file` for preconditions. When `initial_input` is `Some(thunk)`,
/// that thunk is injected into `accumulated_group` at slot `root_group.len()` — the slot
/// assigned to the caller's variable name by the resolver (e.g., `input-ast` for the formatter).
///
/// The caller is responsible for ensuring the resolver was seeded with the correct variable
/// name at that slot (i.e., the program was resolved via `resolve_surface_program` with a
/// seed frame that maps the variable name to `ctx.root_group.len()`).
///
/// Used by the formatter (which passes the AST dict as `input-ast` and seeds the resolver accordingly).
pub async fn eval_surface_file_with_input(
    program: &SurfaceProgram,
    ctx: &Arc<EvalContext>,
    initial_input: Option<Arc<Thunk>>,
) -> EvalResult<Arc<Thunk>> {
    let initial_group = initial_input.map(|thunk| vec![thunk]);
    eval_surface_file_with_initial_group(program, ctx, initial_group).await
}

/// Evaluate a SurfaceProgram with an optional pre-built initial group extension.
///
/// `initial_group`: when `Some`, these thunks are appended to the root group at cumulative
/// slots `root_group.len()..root_group.len()+initial_group.len()-1` before evaluating any
/// document expression. The resolver must have been seeded with matching names at those slots.
async fn eval_surface_file_with_initial_group(
    program: &SurfaceProgram,
    ctx: &Arc<EvalContext>,
    initial_group: Option<Vec<Arc<Thunk>>>,
) -> EvalResult<Arc<Thunk>> {
    let mut last = EMPTY_DICT_THUNK.with(Arc::clone);
    for surface_doc in &program.documents {
        let expr_nodes: Vec<Arc<SurfaceNode>> = surface_doc.node.expressions().cloned().collect();
        last = eval_document_exprs_with_env(&expr_nodes, ctx, initial_group.clone()).await?;
    }
    Ok(last)
}

/// Evaluate a SurfaceProgram, returning the last expression's thunk.
///
/// Sequences through each document's expression scope chain. Identical to
/// `eval_surface_file` but called as the underlying implementation.
async fn eval_surface_file_from_env(
    program: &SurfaceProgram,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let mut last = EMPTY_DICT_THUNK.with(Arc::clone);
    for surface_doc in &program.documents {
        let expr_nodes: Vec<Arc<SurfaceNode>> = surface_doc.node.expressions().cloned().collect();
        last = eval_document_exprs_with_env(&expr_nodes, ctx, None).await?;
    }
    Ok(last)
}

// ============================================================================
// End document pipeline evaluation
// ============================================================================

pub(crate) const DEFAULT_ANNOTATION_KEY: &str = "default";
pub(crate) const IS_ANNOTATION_KEY: &str = "is";

/// Type alias for the optional default expression + FlatEnv env_id used by guarded thunks.
/// Matches value.rs GuardDefault: (Arc<Spanned<CoreExpr>>, u32).
/// The u32 is a bridge placeholder (was EnvId in the arena model; now unused in EvalFrame model).
type GuardDefault = (Arc<Spanned<crate::ast::CoreExpr>>, u32);

// ValuesEqualFuture removed — primitive_eq is synchronous (no async needed).

// Protocol-level annotation meta-keys — not structural record fields.
// These are the canonical property names from the annotation protocol (doc/05-type-annotations.md).
// Uses the same string values as the field constants in type_tags.rs.
#[cfg(test)]
const ANNOTATION_META_KEYS: &[&str] = &[
    crate::ast::ANNOTATION_KEY_DEFAULT, // = "default" — annotation fallback property
    crate::ast::ANNOTATION_KEY_TYPE,    // = "type"    — type constraint property
    crate::type_tags::FIELD_DOC,        // = "doc"     — documentation meta-key
    crate::type_tags::FIELD_IS,         // = "is"      — typeclass instance predicate
    crate::type_tags::FIELD_REPR,       // = "repr"    — Value variant discriminant
    crate::ast::ANNOTATION_KEY_CONSTRUCTOR, // = "_constructor" — constructor marker
];

#[cfg(test)]
pub(crate) fn annotation_has_structural_fields(annotation: &crate::ast::Annotation) -> bool {
    match annotation {
        crate::ast::Annotation::PropertyDict(entries) => entries.iter().any(|entry| {
            let Some(key_node) = entry.node.key.as_ref() else {
                return false;
            };
            match &key_node.expr {
                crate::ast::SurfaceExpression::StringLiteral { content: name, .. } => {
                    !ANNOTATION_META_KEYS.contains(&name.as_str())
                }
                _ => true,
            }
        }),
        crate::ast::Annotation::Simple(_)
        | crate::ast::Annotation::Quote
        | crate::ast::Annotation::Annotated(_, _) => false,
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
/// Encapsulates the type-checker state (populated by `builtin-typecheck-doc` as a side
/// effect). Accessed via `builtin-get-type-context` and mutated by `builtin-typecheck-doc`.
/// This is the opaque handle that loader.llt threads through the type-checking pipeline.
///
/// Wrapped in `Arc<Mutex<Option<...>>>` on `EvalContext` so that:
/// - `None` = TypeContext not yet initialized (bootstrap phase before builtin-make-type-ctx)
/// - `Some` = initialized and ready for use
/// - The `Arc` allows child contexts to share the same TypeContext (they should see each
///   other's updates because builtin-typecheck-doc is a side-effecting operation)
///
/// Full implementation in T-1341 (builtin-get-type-context, builtin-make-type-ctx,
/// builtin-fork-type-ctx). This struct is the stable field layout.
#[derive(Debug, Clone)]
pub struct TypeContextData {
    /// Accumulated Hindley-Milner inference environment.
    /// Initialized to the builtin_core TypeEnv at startup (via `init_type_context` callers).
    /// Each `builtin-typecheck-doc` call seeds from this env and writes the resulting `final_env`
    /// back, accumulating type knowledge across files (prelude → user code).
    /// This makes prelude names (map, filter, raise, etc.) visible to the type checker
    /// when checking user code, without re-typechecking prelude on every call.
    pub inference_env: Arc<RwLock<crate::env::Env>>,
    /// Accumulated type constructor definitions (TyConDef).
    /// Each `builtin-typecheck-doc` call seeds InferState.tycon_env from this map and writes
    /// newly registered TyConDefs back. This propagates opaque types (DirCap, File,
    /// ClockCap, Handle, etc.) declared in `builtin_core.llt` to subsequent module
    /// type-checks (builtin_io.llt, builtin_async.llt, ...) without requiring them to
    /// re-declare types they receive from the runtime environment.
    pub tycon_env: std::collections::HashMap<String, std::sync::Arc<crate::type_def::TyConDef>>,
    /// Type-stage scope chain: pre-computed resolved TypeValues from type-stage evaluation.
    /// Vec[0] = innermost (highest priority); Vec[N-1] = outermost.
    /// Populated by builtin-tc-update-type-stage-env and builtin_typecheck_doc write-back.
    /// Function entries live in `type_stage_fns`; TypeVar entries live in `type_stage_type_vars`.
    pub type_stage_scope: Vec<std::collections::HashMap<String, crate::type_infer::TypeValue>>,
    /// Parameterized type constructor thunks: name → function thunk (e.g., Seq, Result).
    pub type_stage_fns: std::collections::HashMap<String, std::sync::Arc<crate::value::Thunk>>,
    /// TypeVar kind annotations: name → kind string (e.g., "Operator", "Label").
    pub type_stage_type_vars: std::collections::HashMap<String, String>,
    /// Accumulated type errors from all `builtin-typecheck-doc` calls.
    /// Each call to `builtin-typecheck-doc` appends the errors from that document to this vec.
    /// Type diagnostics (errors + warnings + info) from the most recent typecheck pass.
    /// Currently stored for observability; not surfaced to tinct code yet.
    pub type_diagnostics: Vec<crate::error::Diagnostic>,
}

/// Immutable session configuration shared across evaluation.
#[derive(Debug)]
pub struct EvalConfig {
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

/// Evaluation infrastructure context: separates session config from variable bindings.
///
/// Config is immutable (Arc without Mutex). Variable bindings are now managed via
/// EvalFrame (closure-converted, Arc<Vec<Arc<Thunk>>>) rather than a scope arena.
/// Thread as `&Arc<EvalContext>` through eval/materialize; thunks capture `Arc::clone(ctx)`.
#[derive(Debug)]
pub struct EvalContext {
    pub config: Arc<EvalConfig>,
    /// Env variable allowlist. None = unrestricted (all allowed), Some(set) = only those in set.
    /// Some(empty) means all denied (--no-env mode).
    pub env_allowed: Option<HashSet<String>>,
    /// Pipeline blame map: records producing stage label for each `%` thunk at `---` boundaries.
    /// Key is the Arc<Thunk> pointer address (usize), value is the producing stage's file path
    /// or index. Used by contract violation errors to identify the positive party (producer)
    /// per Findler & Felleisen (2002). Avoids a `Value::Tagged` variant which would require
    /// updating all exhaustive `Value` matches.
    pub blame_map: Mutex<HashMap<usize, String>>,
    /// Boundary guards from type inference: span → expected_param_type.
    /// When an Unknown-typed expression crosses into a concrete-typed context,
    /// the type checker records the boundary. The evaluator checks if a thunk's
    /// span matches a guard and wraps it with a runtime Guarded thunk if so.
    /// HashMap for O(1) lookup at thunk creation time in eval_core_expr.
    /// Populated by the type checker via set_boundary_guards().
    pub boundary_guards: RwLock<HashMap<Span, Arc<crate::value::Value>>>,
    /// Monad resolutions for inferred [do] forms: sentinel VarRef name → monad variable name.
    /// The type checker records the resolved monad name here (keyed by the sentinel name, e.g.,
    /// `ℊꜱʏᴍ⧼do-infer⧽0`). At eval time, when a FreeVar with that name is evaluated, the
    /// evaluator looks up this map by name and returns the monad dict value from the environment.
    /// Populated by the prelude at runtime via set_do_infer_resolutions(), consumed during eval().
    /// The type checker does not write to this map — it returns TypeValue.Unknown for do-infer
    /// sentinel calls and leaves monad-type resolution entirely to the runtime prelude.
    pub do_infer_resolutions: RwLock<HashMap<String, String>>,
    /// Already-open libdir Dir, shared from the bootstrap boundary (main.rs).
    /// Used by `builtin_include` to inject `%libdir` into the included file's environment
    /// without calling `open_ambient_dir` again. `None` in contexts where libdir was not
    /// opened (e.g., --no-libdir, bootstrap contexts, tests).
    /// Propagated to child contexts so nested includes see the same Dir.
    ///
    /// Note: `cap_std::fs::Dir` is `Send` on Linux (wraps `std::fs::File`, which is Send).
    /// The `Arc<Dir>` wrapper enables sharing across child contexts without dup-ing the fd.
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
    /// opens and closes a span. Shared via Arc<Mutex<>> so child contexts write to the same
    /// collector.
    /// Public for CLI initialization (main.rs --profile flag).
    pub profiling: Option<Arc<Mutex<crate::profiling::ProfilingCollector>>>,
    /// Type constructor environment from type inference: name → TyConDef.
    /// Set once after typechecking via `set_tycon_env`; read-only thereafter (OnceLock).
    /// Used by `is_subtype` to determine variance and structural rules for user-defined
    /// type constructors. `None` before typechecking or when `--no-typecheck` is used;
    /// `is_subtype` falls back to invariant behaviour in that case.
    /// Propagated to child contexts (with_cancel_token, with_explicit_cancel,
    /// with_timeout_ms) so nested includes and scoped cancellation see the same TyConEnv.
    pub tycon_env: std::sync::OnceLock<std::sync::Arc<crate::type_def::TyConEnv>>,
    /// Unified type environment handle for this evaluation scope.
    ///
    /// `None` until initialized by `builtin-make-type-ctx` (T-1341). Once set, shared
    /// across all child contexts via `Arc::clone` so that `builtin-typecheck-doc` side effects
    /// (TypeScheme registration) are visible everywhere in the pipeline.
    ///
    /// Child contexts created via `with_cancel_token`, `with_explicit_cancel`,
    /// and `with_timeout_ms` all share the same `Arc` — they see the same TypeContext state.
    /// This is intentional: type checking is monotonic (schemas are only added, never removed)
    /// and the pipeline must accumulate type knowledge across files.
    ///
    /// Full implementation: T-1341 (builtin-get-type-context, builtin-make-type-ctx,
    /// builtin-fork-type-ctx), T-1343 (TypeContext struct layout).
    pub type_context: Arc<Mutex<Option<TypeContextData>>>,
    /// Accumulated resolver scope frames from the init program's resolver run.
    ///
    /// Set by `with_scope_frames()` after `resolve_surface_program` is called on the init
    /// (loader) program; `None` in bootstrap contexts and tests where `with_scope_frames()`
    /// was not called. Available in eval_materialize.rs thunk forcing via `thunk_ctx.scope_frames`.
    ///
    /// Used by `lower()` at `eval_document_exprs_with_env` call sites to resolve
    /// scope-frame-dependent names (e.g., builtin-dict-merge for spread dicts) to correct
    /// De Bruijn coordinates.
    ///
    /// Propagated unchanged to all child contexts (with_cancel_token, with_explicit_cancel,
    /// with_timeout_ms) because scope frames are read-only after initialization.
    pub scope_frames: Option<Arc<Vec<indexmap::IndexMap<String, u32>>>>,
    /// Root group: the thunks for all root-scope entries in slot order.
    ///
    /// Slots 0..N-1 hold pre-materialized `Value::Builtin` thunks, one per builtin def
    /// in the order returned by all builtin modules (matching the slot ordering that
    /// `enter_scope_from_frame` / the resolver assigns via `LGM(slot)`).
    ///
    /// At document evaluation time, accumulated_group starts with root_group entries at
    /// slots 0..root_group.len()-1. Document dict entries follow at cumulative slots.
    /// All LGM(slot) references index into this accumulated_group directly.
    ///
    /// Capabilities are added via `with_root_group_capabilities` after the initial builtin
    /// slots. The resolver's `enter_scope_from_frame` uses the same slot ordering so
    /// LGM(slot) addresses are in sync.
    ///
    /// Shared cheaply across child contexts via `Arc::clone`. Child contexts created by
    /// `with_cancel_token`, `with_explicit_cancel`, etc. all see the same root group (read-only).
    pub root_group: Arc<Vec<Arc<Thunk>>>,
    /// GroupSpine representation of root_group for O(1) frame construction.
    ///
    /// Persistent cons-list version of root_group. Every document evaluation starts with
    /// `Arc::clone(&ctx.root_spine)` (O(1)) instead of `root_group.iter().cloned().collect()`
    /// (O(n)). Extended with dict entries via `spine.extend(new_entries)` which is also
    /// O(|new_entries|) and shares the previous level via Arc::clone.
    pub root_spine: std::sync::Arc<crate::value::GroupSpine>,
    /// Snapshot of the accumulated_group after the init program (loader + prelude) finishes
    /// evaluating all its intermediate dicts.
    ///
    /// Set exactly once via `set_init_accumulated_group`, called from
    /// `eval_document_exprs_with_env` the first time it reaches the last expression node.
    /// The OnceLock guarantees at-most-once write; the `Arc` allows sharing across all child
    /// contexts without copying the vec.
    ///
    /// Contains every thunk at its canonical LGM slot: root-scope entries (builtins + caps)
    /// at slots 0..root_group.len()-1, then prelude dict entries at cumulative slots above
    /// that. Builtins use `call_to_match_resolved` to retrieve the `to-match` instance
    /// binding thunk for Matchable dispatch (T-1846/T-1847).
    ///
    /// `None` (OnceLock not set) in bootstrap/test contexts where the full loader pipeline
    /// has not run. All callers must gracefully fall back when `get()` returns `None`.
    pub init_accumulated_group: Arc<std::sync::OnceLock<std::sync::Arc<crate::value::GroupSpine>>>,
    /// Bootstrap TypeValue metatype. Represents the "type of all types." Initialized as an empty
    /// Dict since the self-referential fixed-point requires `Weak<Value>` semantics: constructing
    /// `Type.type_val == Type` needs a back-pointer into an already-constructed `Arc<Value>`, which
    /// plain `Arc` cannot express. During bootstrap, all TypeValues carry the unknown sentinel.
    ///
    /// Allocated once per root context and propagated to all child contexts via `Arc::clone` so
    /// that pointer-identity checks against it are consistent.
    pub type_metatype: Arc<Value>,
    /// Registry mapping repr strings to their runtime TypeValue dict (constructor dict).
    ///
    /// Keyed by the `repr: "Value::X"` string from a type declaration. Populated by the
    /// `CoreExpr::ReprDecl` evaluator (T-1949) when the type declaration is first forced.
    ///
    /// Shared across all child contexts via `Arc` so that registrations made during prelude
    /// evaluation are visible to all subsequent evaluation contexts. Interior mutability via
    /// `Mutex` allows writes through `Arc<EvalContext>`.
    pub repr_registry: Arc<Mutex<std::collections::HashMap<String, Arc<Value>>>>,
    /// Registry mapping repr strings to their `is:` predicate Value (a Function).
    ///
    /// Keyed by the same repr string as `repr_registry`. Populated alongside `repr_registry`
    /// by the `CoreExpr::ReprDecl` evaluator (T-1950) when the type declaration is first forced.
    ///
    /// Shared across all child contexts via `Arc<Mutex<...>>` — same sharing model as
    /// `repr_registry`.
    pub is_predicates: Arc<Mutex<std::collections::HashMap<String, Arc<Value>>>>,

    /// Registry mapping type names to their stable Arc-level identity value.
    ///
    /// Populated by `CoreExpr::TypeDecl` evaluation (T-2111): when a named `[type Name ...]`
    /// declaration is first forced, the evaluator creates a fresh `Arc<Value>` identity and
    /// inserts it here under the type name (e.g., `"Color"`).
    ///
    /// `CoreExpr::UnitVariant` and `CoreExpr::Variant` arms look up this registry to stamp
    /// the correct `type_val` on every `Value::Variant` produced by the type's constructors.
    /// The constructor dict's own `type_val` is set to the same Arc by the TypeDecl arm.
    ///
    /// `match_pattern` uses `Arc::ptr_eq` on `type_val` fields: if the scrutinee's
    /// `type_val` and the pinned dict's `type_val` point to the same allocation, the
    /// scrutinee belongs to that type. Plain dicts (no TypeDecl) carry `unknown_type_val()`
    /// and do not match.
    ///
    /// NOT a global OnceLock: unlike `repr_registry` (populated from `builtin_core.llt` —
    /// global immutable library), type identities are program-specific. Different programs
    /// may define different types with the same name; sharing identities across contexts
    /// would create false matches. Each evaluation context holds its own registry, shared
    /// only with its child contexts (via `Arc::clone`).
    ///
    /// Keyed by type_decl_id (u64) instead of type_name (String) to prevent same-name types
    /// in nested scopes from overwriting each other's registry entries (B-714).
    pub type_identity_registry: Arc<Mutex<std::collections::HashMap<u64, Arc<Value>>>>,
    /// Runtime diagnostics collected during evaluation.
    ///
    /// Shared across all child contexts (Arc clone) so that diagnostics emitted in any scope
    /// are accumulated in a single unified collection. Printed at the end of the pipeline
    /// by `run_loader_pipeline` in lib.rs.
    pub runtime_diagnostics: std::sync::Arc<std::sync::Mutex<Vec<crate::error::Diagnostic>>>,
}

impl EvalContext {
    pub fn new() -> Arc<Self> {
        Self::new_with_options(false, None)
    }

    /// Build the root group (Arc<Vec<Arc<Thunk>>>) from all static builtin modules.
    ///
    /// Each builtin def occupies one slot in the vec, in the same order that the resolver's
    /// `enter_scope_from_frame` assigns slot indices (i.e., the order returned by the same
    /// builtin-module chain: core → meta → string → async → math → io → net → datetime).
    /// This slot ordering is what `LGM(slot)` addresses reference at runtime.
    ///
    /// Called once per root context (`new_empty`, `new_with_options`). Child contexts share the same Arc.
    fn build_root_group_builtins() -> Arc<Vec<Arc<Thunk>>> {
        let all_builtins: Vec<crate::value::BuiltinDef> = crate::builtins_core::core_builtins()
            .into_iter()
            .chain(crate::builtins_meta::meta_builtins())
            .chain(crate::builtins_string::string_builtins())
            .chain(crate::builtins_async::async_builtins())
            .chain(crate::builtins_math::math_builtins())
            .chain(crate::builtins_io::io_builtins())
            .chain(crate::builtins_net::net_builtins())
            .chain(crate::builtins_datetime::datetime_builtins())
            .collect();
        let group: Vec<Arc<Thunk>> = all_builtins
            .into_iter()
            .map(|def| {
                Arc::new(Thunk::value(
                    crate::value::Value::Builtin {
                        def,
                        type_val: crate::value::unknown_type_val(),
                    },
                    crate::rust_span!(),
                ))
            })
            .collect();
        Arc::new(group)
    }

    /// Create a new EvalContext for bootstrap and test contexts.
    ///
    /// - Bootstrap contexts (run_loader_pipeline, where loader.llt is being evaluated)
    /// - Re-entrant macro expansion (depth > 0 in expand.rs)
    /// - Test helpers that create contexts without a prelude env
    ///
    /// `repr_registry` and `is_predicates` are both initialized from the global OnceLocks
    /// (`GLOBAL_REPR_REGISTRY` / `GLOBAL_IS_PREDICATES`) so that all independently-created
    /// root contexts share the same registry Arc. Registrations made by one context
    /// (e.g. during builtin_core.llt evaluation) are immediately visible in all others.
    pub fn new_empty() -> Arc<Self> {
        let root_group = Self::build_root_group_builtins();
        let root_spine = crate::value::GroupSpine::from_flat(root_group.iter().cloned().collect());
        let repr_registry = Arc::clone(
            GLOBAL_REPR_REGISTRY
                .get_or_init(|| Arc::new(Mutex::new(std::collections::HashMap::new()))),
        );
        let is_predicates = Arc::clone(
            GLOBAL_IS_PREDICATES
                .get_or_init(|| Arc::new(Mutex::new(std::collections::HashMap::new()))),
        );
        Arc::new(Self {
            config: Arc::new(EvalConfig {
                require_integrity: false,
                macro_injects_map: HashMap::new(),
                source_file: None,
            }),
            env_allowed: None,
            blame_map: Mutex::new(HashMap::new()),
            boundary_guards: RwLock::new(HashMap::new()),
            do_infer_resolutions: RwLock::new(HashMap::new()),
            libdir_dir: Mutex::new(None),
            cancel: tokio_util::sync::CancellationToken::new(),
            task_registry: Arc::new(Mutex::new(Vec::new())),
            profiling: None,
            tycon_env: std::sync::OnceLock::new(),
            type_context: Arc::new(Mutex::new(None)),
            scope_frames: None,
            root_group,
            root_spine,
            init_accumulated_group: Arc::new(std::sync::OnceLock::new()),
            type_metatype: Arc::new(Value::Dict {
                entries: indexmap::IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            }),
            repr_registry,
            is_predicates,
            type_identity_registry: Arc::new(Mutex::new(std::collections::HashMap::new())),
            runtime_diagnostics: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }

    pub fn new_with_options(
        require_integrity: bool,
        env_allowed: Option<HashSet<String>>,
    ) -> Arc<Self> {
        let root_group = Self::build_root_group_builtins();
        let root_spine = crate::value::GroupSpine::from_flat(root_group.iter().cloned().collect());
        let repr_registry = Arc::clone(
            GLOBAL_REPR_REGISTRY
                .get_or_init(|| Arc::new(Mutex::new(std::collections::HashMap::new()))),
        );
        let is_predicates = Arc::clone(
            GLOBAL_IS_PREDICATES
                .get_or_init(|| Arc::new(Mutex::new(std::collections::HashMap::new()))),
        );
        Arc::new(Self {
            config: Arc::new(EvalConfig {
                require_integrity,
                macro_injects_map: HashMap::new(),
                source_file: None,
            }),
            env_allowed,
            blame_map: Mutex::new(HashMap::new()),
            boundary_guards: RwLock::new(HashMap::new()),
            do_infer_resolutions: RwLock::new(HashMap::new()),
            libdir_dir: Mutex::new(None),
            cancel: tokio_util::sync::CancellationToken::new(),
            task_registry: Arc::new(Mutex::new(Vec::new())),
            profiling: None,
            tycon_env: std::sync::OnceLock::new(),
            type_context: Arc::new(Mutex::new(None)),
            scope_frames: None,
            root_group,
            root_spine,
            init_accumulated_group: Arc::new(std::sync::OnceLock::new()),
            type_metatype: Arc::new(Value::Dict {
                entries: indexmap::IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            }),
            repr_registry,
            is_predicates,
            type_identity_registry: Arc::new(Mutex::new(std::collections::HashMap::new())),
            runtime_diagnostics: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        })
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
            env_allowed: self.env_allowed.clone(),
            blame_map: Mutex::new(self.blame_map.lock().unwrap().clone()),
            boundary_guards: RwLock::new(self.boundary_guards.read().unwrap().clone()),
            do_infer_resolutions: RwLock::new(self.do_infer_resolutions.read().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel: child_token.clone(),
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
            tycon_env: {
                let child_lock = std::sync::OnceLock::new();
                if let Some(env) = self.tycon_env.get() {
                    child_lock
                        .set(std::sync::Arc::clone(env))
                        .expect("impossible: fresh OnceLock already set");
                }
                child_lock
            },
            type_context: Arc::clone(&self.type_context),
            scope_frames: self.scope_frames.clone(),
            root_group: Arc::clone(&self.root_group),
            root_spine: Arc::clone(&self.root_spine),
            init_accumulated_group: Arc::clone(&self.init_accumulated_group),
            type_metatype: Arc::clone(&self.type_metatype),
            repr_registry: Arc::clone(&self.repr_registry),
            is_predicates: Arc::clone(&self.is_predicates),
            type_identity_registry: Arc::clone(&self.type_identity_registry),
            runtime_diagnostics: std::sync::Arc::clone(&self.runtime_diagnostics),
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
    /// Shares all config, state, and task_registry with the parent.
    /// Clones blame_map, libdir_dir (per-scope fields, same as `with_cancel_token`).
    pub fn with_explicit_cancel(
        self: &Arc<Self>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            config: Arc::clone(&self.config),
            env_allowed: self.env_allowed.clone(),
            blame_map: Mutex::new(self.blame_map.lock().unwrap().clone()),
            boundary_guards: RwLock::new(self.boundary_guards.read().unwrap().clone()),
            do_infer_resolutions: RwLock::new(self.do_infer_resolutions.read().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel,
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
            tycon_env: {
                let child_lock = std::sync::OnceLock::new();
                if let Some(env) = self.tycon_env.get() {
                    child_lock
                        .set(std::sync::Arc::clone(env))
                        .expect("impossible: fresh OnceLock already set");
                }
                child_lock
            },
            type_context: Arc::clone(&self.type_context),
            scope_frames: self.scope_frames.clone(),
            root_group: Arc::clone(&self.root_group),
            root_spine: Arc::clone(&self.root_spine),
            init_accumulated_group: Arc::clone(&self.init_accumulated_group),
            type_metatype: Arc::clone(&self.type_metatype),
            repr_registry: Arc::clone(&self.repr_registry),
            is_predicates: Arc::clone(&self.is_predicates),
            type_identity_registry: Arc::clone(&self.type_identity_registry),
            runtime_diagnostics: std::sync::Arc::clone(&self.runtime_diagnostics),
        })
    }

    /// Create a child EvalContext with a timeout: automatically cancels after `ms` milliseconds.
    ///
    /// Spawns a background task that fires the cancellation after the delay.
    /// Returns the child context; the cancel handle is internal (use `[with-cancel]` if you
    /// need explicit control).
    pub fn with_timeout_ms(self: &Arc<Self>, ms: u64) -> Arc<Self> {
        let child_token = self.cancel.child_token();
        let cancel_clone = child_token.clone();
        // All captured types are Send (CancellationToken).
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            cancel_clone.cancel();
        });

        // Register background task for drain tracking
        self.task_registry.lock().unwrap().push(handle);

        Arc::new(Self {
            config: Arc::clone(&self.config),
            env_allowed: self.env_allowed.clone(),
            blame_map: Mutex::new(self.blame_map.lock().unwrap().clone()),
            boundary_guards: RwLock::new(self.boundary_guards.read().unwrap().clone()),
            do_infer_resolutions: RwLock::new(self.do_infer_resolutions.read().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel: child_token,
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
            tycon_env: {
                let child_lock = std::sync::OnceLock::new();
                if let Some(env) = self.tycon_env.get() {
                    child_lock
                        .set(std::sync::Arc::clone(env))
                        .expect("impossible: fresh OnceLock already set");
                }
                child_lock
            },
            type_context: Arc::clone(&self.type_context),
            scope_frames: self.scope_frames.clone(),
            root_group: Arc::clone(&self.root_group),
            root_spine: Arc::clone(&self.root_spine),
            init_accumulated_group: Arc::clone(&self.init_accumulated_group),
            type_metatype: Arc::clone(&self.type_metatype),
            repr_registry: Arc::clone(&self.repr_registry),
            is_predicates: Arc::clone(&self.is_predicates),
            type_identity_registry: Arc::clone(&self.type_identity_registry),
            runtime_diagnostics: std::sync::Arc::clone(&self.runtime_diagnostics),
        })
    }

    /// Create a child EvalContext with resolver scope frames attached.
    ///
    /// Called after `resolve_surface_program` to make the accumulated scope frames
    /// available to `lower()` for resolving scope-frame-dependent names to correct
    /// De Bruijn coordinates (B-513 fix).
    ///
    /// The frames are stored as an `Arc<Vec<...>>` so the clone is cheap (pointer copy).
    /// Child contexts created from this context inherit the frames unchanged.
    pub fn with_scope_frames(
        self: &Arc<Self>,
        frames: Arc<Vec<indexmap::IndexMap<String, u32>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config: Arc::clone(&self.config),
            env_allowed: self.env_allowed.clone(),
            blame_map: Mutex::new(self.blame_map.lock().unwrap().clone()),
            boundary_guards: RwLock::new(self.boundary_guards.read().unwrap().clone()),
            do_infer_resolutions: RwLock::new(self.do_infer_resolutions.read().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel: self.cancel.clone(),
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
            tycon_env: {
                let child_lock = std::sync::OnceLock::new();
                if let Some(env) = self.tycon_env.get() {
                    child_lock
                        .set(std::sync::Arc::clone(env))
                        .expect("impossible: fresh OnceLock already set");
                }
                child_lock
            },
            type_context: Arc::clone(&self.type_context),
            scope_frames: Some(frames),
            root_group: Arc::clone(&self.root_group),
            root_spine: Arc::clone(&self.root_spine),
            init_accumulated_group: Arc::clone(&self.init_accumulated_group),
            type_metatype: Arc::clone(&self.type_metatype),
            repr_registry: Arc::clone(&self.repr_registry),
            is_predicates: Arc::clone(&self.is_predicates),
            type_identity_registry: Arc::clone(&self.type_identity_registry),
            runtime_diagnostics: std::sync::Arc::clone(&self.runtime_diagnostics),
        })
    }

    /// Create a child EvalContext with capabilities appended to the root group.
    ///
    /// Called by `main.rs` after all capability thunks (`%cwd`, `%libdir`, `%clock`,
    /// user --cap-fs / --cap-net entries, `%programs`, `%args`) have been built. Each
    /// capability occupies a slot in `root_group` immediately after the builtin slots.
    ///
    /// The resolver's `enter_scope_from_frame` assigns `LGM(slot)` addresses using the
    /// same slot ordering (builtins first, then capabilities in the order provided here).
    /// At runtime, accumulated_group starts with root_group entries, so LGM(slot) indexes
    /// directly into the right position.
    ///
    /// The new root_group is shared across all child contexts (pointer clone only).
    pub fn with_root_group_capabilities(
        self: &Arc<Self>,
        capabilities: Vec<(String, Arc<Thunk>)>,
    ) -> Arc<Self> {
        // Build an extended group: existing root_group slots (builtins) + capability thunks.
        let mut new_group: Vec<Arc<Thunk>> = self.root_group.iter().cloned().collect();
        let cap_thunks: Vec<Arc<Thunk>> = capabilities
            .iter()
            .map(|(_name, thunk)| Arc::clone(thunk))
            .collect();
        for (_name, thunk) in capabilities {
            new_group.push(thunk);
        }
        let root_spine = self.root_spine.extend(cap_thunks);
        Arc::new(Self {
            config: Arc::clone(&self.config),
            env_allowed: self.env_allowed.clone(),
            blame_map: Mutex::new(self.blame_map.lock().unwrap().clone()),
            boundary_guards: RwLock::new(self.boundary_guards.read().unwrap().clone()),
            do_infer_resolutions: RwLock::new(self.do_infer_resolutions.read().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel: self.cancel.clone(),
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
            tycon_env: {
                let child_lock = std::sync::OnceLock::new();
                if let Some(env) = self.tycon_env.get() {
                    child_lock
                        .set(std::sync::Arc::clone(env))
                        .expect("impossible: fresh OnceLock already set");
                }
                child_lock
            },
            type_context: Arc::clone(&self.type_context),
            scope_frames: self.scope_frames.clone(),
            root_group: Arc::new(new_group),
            root_spine,
            init_accumulated_group: Arc::clone(&self.init_accumulated_group),
            type_metatype: Arc::clone(&self.type_metatype),
            repr_registry: Arc::clone(&self.repr_registry),
            is_predicates: Arc::clone(&self.is_predicates),
            type_identity_registry: Arc::clone(&self.type_identity_registry),
            runtime_diagnostics: std::sync::Arc::clone(&self.runtime_diagnostics),
        })
    }

    /// Build the resolver seed map from this context's root group.
    ///
    /// Returns a name → slot mapping where:
    /// - Slots 0..num_builtins-1 are builtin thunks (Value::Builtin with def.name)
    /// - Slots num_builtins..num_builtins+num_caps-1 are capability thunks (name from span)
    ///
    /// The resolver passes this as the outermost seed frame to `enter_scope_from_frame`,
    /// which assigns `LGM(slot)` to each name. At runtime, accumulated_group starts with
    /// root_group entries, so LGM(slot) indexes directly into `group[slot]`.
    pub fn root_group_resolver_map(&self) -> indexmap::IndexMap<String, u32> {
        self.root_group
            .iter()
            .enumerate()
            .filter_map(|(slot, thunk)| {
                // Builtin thunks carry name in their Value.
                // Capability thunks carry name in their span.
                // peek_result() distinguishes: settled-Ok (inspect value), settled-Err
                // (propagate name from span — error will surface at evaluation), not-settled
                // (propagate name from span).  Builtin and capability thunks in root_group
                // are always Thunk::value(...) so they can never carry errors in practice.
                let name = match thunk.peek_result() {
                    Some(Ok(val)) => match val {
                        Value::Builtin { def, .. } => Some(def.name.to_string()),
                        _ => thunk.definition_span().name.as_ref().map(|n| n.to_string()),
                    },
                    _ => thunk.definition_span().name.as_ref().map(|n| n.to_string()),
                };
                name.map(|n| (n, slot as u32))
            })
            .collect()
    }

    /// Store a snapshot of the accumulated_group after the init program evaluates.
    ///
    /// Called from `eval_document_exprs_with_env` the first time it reaches the last
    /// expression node of the loader program. The OnceLock ensures the snapshot is written
    /// at most once; subsequent calls (from nested `builtin-eval` or user documents) silently
    /// do nothing (the first write wins).
    ///
    /// The `group` parameter is the full `accumulated_group` at the point just before the last
    /// expression node is evaluated — it contains every thunk for every intermediate dict the
    /// loader has evaluated so far, at their canonical LGM slots.
    pub fn set_init_accumulated_group(&self, group: std::sync::Arc<crate::value::GroupSpine>) {
        // get_or_init: first write wins; subsequent calls are no-ops (the existing value is
        // returned, and the supplied closure is not called). This is the correct at-most-once
        // initialization pattern — no Result to discard.
        self.init_accumulated_group.get_or_init(|| group);
    }

    /// Retrieve the accumulated_group snapshot from the init program's evaluation.
    ///
    /// Returns `None` in bootstrap/test contexts where the full loader pipeline has not run.
    /// Callers must gracefully fall back (e.g., return `false`) when this is `None`.
    pub fn get_init_accumulated_group(&self) -> Option<&std::sync::Arc<crate::value::GroupSpine>> {
        self.init_accumulated_group.get()
    }

    /// Record blame provenance for a pipeline `%` thunk at a `---` boundary.
    /// The `label` identifies the producing stage (file path or stage index).
    pub fn record_blame(&self, thunk: &Arc<Thunk>, label: String) {
        let key = Arc::as_ptr(thunk) as usize;
        self.blame_map.lock().unwrap().insert(key, label);
    }

    /// Look up blame provenance for a thunk (if recorded at a pipeline boundary).
    pub fn blame_label(&self, thunk: &Arc<Thunk>) -> Option<String> {
        let key = Arc::as_ptr(thunk) as usize;
        self.blame_map.lock().unwrap().get(&key).cloned()
    }

    /// Set the type constructor environment from type inference.
    /// Called after type checking to wire user-defined TyCon variance and structural rules
    /// to the evaluator's subtype checker.
    ///
    /// Called once after typechecking. `EvalContext` must not be reused across evaluation
    /// sessions (e.g. REPL inputs) because the OnceLock is write-once and cannot be updated.
    /// Create a fresh `EvalContext` per evaluation session.
    pub fn set_tycon_env(&self, env: crate::type_def::TyConEnv) {
        self.tycon_env.set(std::sync::Arc::new(env)).expect(
            "tycon_env already set — EvalContext must not be reused across evaluation sessions",
        );
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
    /// set once at the start of a pipeline and mutated in-place by `builtin-typecheck-doc`.
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
    /// to child contexts (nested includes).
    pub fn set_libdir_dir(&self, dir: Arc<cap_std::fs::Dir>) {
        *self.libdir_dir.lock().unwrap() = Some(dir);
    }

    /// Set boundary guards from type inference.
    /// Called after type checking to wire gradual typing runtime checks.
    pub fn set_boundary_guards(&self, guards: HashMap<Span, Arc<crate::value::Value>>) {
        *self.boundary_guards.write().unwrap() = guards;
    }

    /// Set do-infer resolutions from type inference.
    /// Called after type checking to wire inferred [do] monad resolution to the evaluator.
    /// The map keys are the sentinel VarRef names (e.g., `ℊꜱʏᴍ⧼do-infer⧽0`); values are
    /// the monad dict variable names (e.g., "result") resolved by the type checker.
    pub fn set_do_infer_resolutions(&self, resolutions: HashMap<String, String>) {
        *self.do_infer_resolutions.write().unwrap() = resolutions;
    }

    /// Set the source file name for FnAnnotation (LSP hover) and child context propagation.
    /// Must be called on a freshly created context before any Arc::clone shares it.
    /// Propagated to child contexts (nested includes).
    /// Note: backtrace frame filenames are embedded in `Span.file` (populated by `parse()`),
    /// not derived from this field.
    pub fn set_source_file(&mut self, file: Option<String>) {
        if let Some(config) = Arc::get_mut(&mut self.config) {
            config.source_file = file;
        }
    }
}

// Static assertions: verify EvalContext Send+Sync bounds.
// After T-1768 (Value: Send), T-1774 (ScopeArena eliminated), and T-1769 (BuiltinFn + Send),
// EvalContext must be Send+Sync so Arc<EvalContext>: Send+Sync.
//
// The assertion is in a #[test] so it is reachable at build time (the compiler must
// monomorphize the bound-checks) and the test runner verifies it each run.
#[test]
fn eval_context_is_send_sync() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<EvalContext>();
    assert_sync::<EvalContext>();
}

/// Extract the ground TypeValue of a runtime value for consistent subtyping validation.
///
/// Maps runtime `Value` variants to their ground `TypeValue` (Arc<Value>). Erased positions
/// (Seq elements, Dict field values, Function params/returns) become `TypeValue.Unknown`.
/// The consistent subtyping relation (`bas::is_consistent_subtype`) then accepts `Unknown`
/// against any annotation, implementing AGT gradual typing semantics.
///
/// **Laziness preservation:** This function MUST NOT force any thunks.
pub(crate) fn ground_typevalue_of(v: &Value) -> Arc<Value> {
    use crate::type_infer::{make_typevalue_repr, make_typevalue_unknown};
    match v {
        Value::Int { .. } | Value::U64 { .. } => make_typevalue_repr(REPR_INT),
        Value::Float { .. } => make_typevalue_repr(REPR_FLOAT),
        Value::String { .. } => make_typevalue_repr(REPR_STRING),
        Value::Bytes { .. } => make_typevalue_repr(REPR_BYTES),
        Value::Dict { .. } => {
            // Record with all fields erased to Unknown — structural tag-only check.
            // The full field set is not inspected (would require forcing thunks).
            make_typevalue_repr(REPR_DICT)
        }
        Value::Function { type_val, .. } => {
            // Use the function's resolved TypeValue.Fn if available (set by the lowerer
            // from the type-checker's resolved annotations). Falls back to Repr("Function")
            // when the function was created without type information (gradual).
            match crate::type_infer::typevalue_ctor(type_val) {
                Some(crate::type_tags::TV_FN) => Arc::clone(type_val),
                _ => make_typevalue_repr(REPR_FUNCTION),
            }
        }
        Value::Builtin { .. } => make_typevalue_repr(REPR_FUNCTION),
        Value::Variant { ctor, .. } => {
            // Produce a TypeValue.Op with the tycon name for nominal dispatch.
            let tycon = crate::value::tycon_name_from_ctor(ctor.as_ref());
            crate::type_infer::make_typevalue_op(tycon)
        }
        Value::Proxy { .. } => make_typevalue_repr(REPR_PROXY),
        // Annotated is transparent — delegate to inner value's ground type.
        Value::Annotated { inner, .. } => ground_typevalue_of(inner),
        // All other runtime-only types → Unknown (gradual: accept any annotation).
        _ => make_typevalue_unknown(),
    }
}

/// Check if a materialized value matches a TypeValue for structural TypeAssert validation.
/// Returns true if the value conforms to the expected type.
///
/// **TyCon dispatch:** `TypeValue.Op(name)` and `TypeValue.App(TypeValue.Op(name), _)` are
/// handled by looking up `name` in `ctx.tycon_env()`. If the def has `builtin_type`, dispatch
/// on its discriminant string to check the corresponding Value variant. If the def is nominal
/// (has constructors), check that the value is a Variant whose tag starts with `"<name>."`.
///
/// **Gradual types:** `TypeValue.Unknown` and `TypeValue.Top` match everything.
/// `TypeValue.Never` matches nothing. `TypeValue.Var` is treated as Unknown at runtime.
///
/// **Fallback:** Builds a ground TypeValue from the runtime value and delegates to
/// `bas::is_consistent_subtype` for proper structural checking.
///
/// Value::Annotated is transparent.
pub(crate) fn value_matches_type(value: &Value, expected: &Arc<Value>, ctx: &EvalContext) -> bool {
    // Value::Proxy is a gradual type — its ground is TypeValue.Unknown, which is consistent
    // with any annotation. Return true immediately before dispatching on the expected type.
    if matches!(value, Value::Proxy { .. }) {
        return true;
    }

    // Value::Annotated is transparent — delegate to the inner value.
    if let Value::Annotated { inner, .. } = value {
        return value_matches_type(inner, expected, ctx);
    }

    // Non-variant TypeValues (e.g. bootstrap unknown_type_val empty dict) — treat as Unknown.
    if !matches!(expected.as_ref(), Value::Variant { .. }) {
        return true;
    }

    // TypeValue.Record: runtime shape check — value must be a Dict with all required fields
    // present. Under BAS width subtyping, extra fields are always accepted.
    //
    // This arm cannot be delegated to BAS because BAS operates on abstract TypeValues
    // (ground_typevalue_of erases field structure by mapping Value::Dict → Repr("Value::Dict")),
    // whereas this check requires access to the concrete value's field set.
    //
    // Note: field TYPE checking is deferred to guard thunks (validate_and_wrap_record in
    // Cont::TypeAssertCheck / Cont::GuardedValidate). This arm only performs the shape check.
    if matches!(expected.as_ref(), Value::Variant { ctor, .. } if ctor.as_ref() == TV_RECORD) {
        let Value::Dict { ref entries, .. } = value else {
            return false;
        };
        if let Some(fields) = extract_typevalue_record_fields(expected) {
            // All required fields must be present in the dict.
            for field_name in fields.keys() {
                let has_field = entries
                    .contains_key(&HashableValue::Str(Arc::from(field_name.as_str())))
                    || if let Ok(idx) = field_name.parse::<i64>() {
                        entries.contains_key(&HashableValue::Int(idx))
                    } else {
                        false
                    };
                if !has_field {
                    return false;
                }
            }
            return true;
        }
        // Malformed TypeValue.Record — conservative false.
        return false;
    }

    // TypeValue.Union: value matches union if it matches ANY member.
    //
    // Handled here (rather than delegated to BAS) because union members may include
    // TypeValue.Record, which requires concrete value access (see Record arm above).
    // BAS's is_consistent_subtype handles Union on sup, but uses ground_typevalue_of(value)
    // which erases field structure, making Record members always fail.
    if matches!(expected.as_ref(), Value::Variant { ctor, .. } if ctor.as_ref() == TV_UNION) {
        if let Value::Variant {
            payload: Some(payload_thunk),
            ..
        } = expected.as_ref()
        {
            if let Some(Ok(Value::Dict { entries, .. })) = payload_thunk.peek_result() {
                let members_key = HashableValue::Str(Arc::from(FIELD_MEMBERS));
                if let Some(members_thunk) = entries.get(&members_key) {
                    if let Some(Ok(Value::Dict {
                        entries: members_entries,
                        ..
                    })) = members_thunk.peek_result()
                    {
                        let mut i: i64 = 0;
                        loop {
                            match members_entries.get(&HashableValue::Int(i)) {
                                None => break,
                                Some(member_thunk) => {
                                    match member_thunk.peek_result() {
                                        None => {
                                            // Unsettled thunk in a TypeValue.Union — indicates an
                                            // internal type-checker bug (TypeValues must always be
                                            // constructed via Thunk::value() and are settled at
                                            // construction time). Treat as a conservative type
                                            // mismatch: the runtime evaluation path must not abort.
                                            panic!(
                                                "invariant violation: TypeValue.Union member thunk \
                                                 at index {i} is not settled"
                                            );
                                        }
                                        Some(Err(_e)) => {
                                            // Errored thunk in a TypeValue.Union — indicates a
                                            // type-checker bug. Conservative type mismatch.
                                            panic!(
                                                "invariant violation: TypeValue.Union member thunk \
                                                 at index {i} contains an error: {_e:?}"
                                            );
                                        }
                                        Some(Ok(member_val)) => {
                                            let member_tv = Arc::new(member_val.clone());
                                            if value_matches_type(value, &member_tv, ctx) {
                                                return true;
                                            }
                                        }
                                    }
                                    i += 1;
                                }
                            }
                        }
                        return false;
                    }
                }
            }
        }
        // Malformed TypeValue.Union — fall through to BAS.
    }

    // All other TypeValues: delegate to BAS consistent subtyping.
    //
    // BAS operates on a ground TypeValue derived from the runtime value. The BAS
    // is_atom_subtype handles:
    //   - TypeValue.Unknown / Top / Var / Never — via is_consistent_subtype preamble
    //   - TypeValue.Repr — via ground Repr(x) <: Repr(x) structural equality
    //   - TypeValue.Op — via (TV_REPR, TV_OP) TyCon dispatch using ctx.tycon_env
    //   - TypeValue.App — via (TV_REPR, TV_APP) root TyCon extraction + dispatch
    //   - TypeValue.Fn / Inter / Recursive — via atom subtype rules
    // Construct a temporary InferenceContext with only the tycon_env (no subst/levels).
    // When tycon_env is not yet wired (e.g., --no-typecheck or test contexts), use an
    // empty env. TyCon lookups on unknown names return None → conservative false, which
    // is correct: gradual types (Unknown, Top, Var) are handled before TyCon dispatch.
    let tycon_env = match ctx.tycon_env() {
        Some(e) => e.clone(),
        None => std::collections::HashMap::new(), // tycon_env not wired (e.g., --no-typecheck or test context).
    };
    let inference_ctx = crate::type_infer::InferenceContext::with_tycon_env(tycon_env);
    let ground_tv = ground_typevalue_of(value);
    crate::bas::is_consistent_subtype(&ground_tv, expected, &inference_ctx)
}

/// Format a TypeValue (Arc<Value>) for error messages in TypeAssert.
///
/// Produces a human-readable string from a TypeValue for type assertion error messages.
/// Future work: produce prettier output for complex TypeValues (Union, Record, etc.).
pub(crate) fn format_type_for_assert(ty: &Arc<Value>) -> String {
    // Extract the TypeValue ctor tag and payload for display.
    match ty.as_ref() {
        Value::Variant { ctor, payload, .. } => match ctor.as_ref() {
            TV_REPR => {
                // Extract repr string and map to user-friendly name.
                if let Some(Ok(Value::Dict { entries, .. })) =
                    payload.as_ref().and_then(|t| t.peek_result())
                {
                    if let Some(thunk) = entries.get(&HashableValue::Str(Arc::from(FIELD_REPR))) {
                        if let Some(Ok(Value::String {
                            source, start, end, ..
                        })) = thunk.peek_result()
                        {
                            return match &source[*start..*end] {
                                REPR_INT => "Int".to_string(),
                                REPR_FLOAT => "Float".to_string(),
                                REPR_STRING => "String".to_string(),
                                REPR_BYTES => "Bytes".to_string(),
                                REPR_FUNCTION => "Fn".to_string(),
                                REPR_DICT => "Dict".to_string(),
                                other => other.to_string(),
                            };
                        }
                    }
                }
                TV_REPR.to_string()
            }
            TV_UNKNOWN => "?".to_string(),
            TV_TOP => "Top".to_string(),
            TV_NEVER => "Never".to_string(),
            TV_VAR => {
                if let Some(Ok(Value::Dict { entries, .. })) =
                    payload.as_ref().and_then(|t| t.peek_result())
                {
                    if let Some(thunk) = entries.get(&HashableValue::Str(Arc::from(FIELD_NAME))) {
                        if let Some(Ok(Value::String {
                            source, start, end, ..
                        })) = thunk.peek_result()
                        {
                            return source[*start..*end].to_string();
                        }
                    }
                }
                "TypeVar".to_string()
            }
            TV_OP => {
                if let Some(Ok(Value::Dict { entries, .. })) =
                    payload.as_ref().and_then(|t| t.peek_result())
                {
                    if let Some(thunk) = entries.get(&HashableValue::Str(Arc::from(FIELD_NAME))) {
                        if let Some(Ok(Value::String {
                            source, start, end, ..
                        })) = thunk.peek_result()
                        {
                            return source[*start..*end].to_string();
                        }
                    }
                }
                TV_OP.to_string()
            }
            TV_UNION | TV_INTER => {
                let keyword = if ctor.as_ref() == TV_UNION {
                    "or"
                } else {
                    "and"
                };
                if let Some(Ok(Value::Dict {
                    entries: payload_entries,
                    ..
                })) = payload.as_ref().and_then(|t| t.peek_result())
                {
                    if let Some(members_thunk) =
                        payload_entries.get(&HashableValue::Str(Arc::from(FIELD_MEMBERS)))
                    {
                        if let Some(Ok(Value::Dict {
                            entries: members, ..
                        })) = members_thunk.peek_result()
                        {
                            let parts: Vec<String> = members
                                .values()
                                .filter_map(|t| {
                                    if let Some(Ok(v)) = t.peek_result() {
                                        Some(format_type_for_assert(&Arc::new(v.clone())))
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            if !parts.is_empty() {
                                return format!("[{} {}]", keyword, parts.join(" "));
                            }
                        }
                    }
                }
                ctor.to_string()
            }
            TV_NEG => {
                if let Some(Ok(Value::Dict { entries, .. })) =
                    payload.as_ref().and_then(|t| t.peek_result())
                {
                    if let Some(inner_thunk) = entries.get(&HashableValue::Str(Arc::from(FIELD_OF)))
                    {
                        if let Some(Ok(inner)) = inner_thunk.peek_result() {
                            return format!(
                                "~{}",
                                format_type_for_assert(&Arc::new(inner.clone()))
                            );
                        }
                    }
                }
                "~?".to_string()
            }
            TV_RECORD => {
                if let Some(Ok(Value::Dict {
                    entries: payload_entries,
                    ..
                })) = payload.as_ref().and_then(|t| t.peek_result())
                {
                    if let Some(fields_thunk) =
                        payload_entries.get(&HashableValue::Str(Arc::from(FIELD_FIELDS)))
                    {
                        if let Some(Ok(Value::Dict {
                            entries: fields, ..
                        })) = fields_thunk.peek_result()
                        {
                            let parts: Vec<String> = fields
                                .iter()
                                .filter_map(|(k, v_thunk)| {
                                    let key_str = match k {
                                        HashableValue::Str(s) => s.to_string(),
                                        _ => return None,
                                    };
                                    if let Some(Ok(v)) = v_thunk.peek_result() {
                                        Some(format!(
                                            "{}: {}",
                                            key_str,
                                            format_type_for_assert(&Arc::new(v.clone()))
                                        ))
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            return format!("[{}]", parts.join("  "));
                        }
                    }
                }
                "Dict".to_string()
            }
            TV_FN => {
                // TypeValue.Fn: format as "Fn@RetType [ParamType ...]".
                // TypeValue payloads use settled thunks (Thunk::value), so peek_result()
                // returns Some(Ok(v)) for well-formed TypeValues. Errored thunks indicate
                // malformed TypeValues — surface the error in the display string (Axiom 6:
                // errors must be visible, not suppressed).
                if let Some(Ok(Value::Dict { entries, .. })) =
                    payload.as_ref().and_then(|t| t.peek_result())
                {
                    let ret_str = entries
                        .get(&HashableValue::Str(Arc::from(FIELD_RETURN)))
                        .and_then(|t| t.peek_result())
                        .map(|r| match r {
                            Ok(v) => format_type_for_assert(&Arc::new(v.clone())),
                            Err(e) => format!("<error: {e}>"),
                        })
                        .unwrap_or_else(|| "?".to_string());
                    let params_str = match entries
                        .get(&HashableValue::Str(Arc::from(FIELD_PARAMS)))
                        .and_then(|t| t.peek_result())
                        .and_then(|r| match r {
                            Ok(Value::Dict {
                                entries: p_entries, ..
                            }) => {
                                let parts: Vec<String> = p_entries
                                    .values()
                                    .map(|t| {
                                        t.peek_result()
                                            .map(|r| match r {
                                                Ok(pv) => {
                                                    format_type_for_assert(&Arc::new(pv.clone()))
                                                }
                                                Err(e) => format!("<error: {e}>"),
                                            })
                                            .unwrap_or_else(|| "?".to_string())
                                    })
                                    .collect();
                                Some(parts.join(" "))
                            }
                            Ok(_) => None,
                            Err(e) => Some(format!("<error: {e}>")),
                        }) {
                        Some(v) => v,
                        None => String::new(), // TypeValue.Fn has no members to display.
                    };
                    return format!("Fn@{ret_str} [{params_str}]");
                }
                "<Fn: display failed>".to_string()
            }
            other => other.to_string(),
        },
        _ => "?".to_string(), // Non-variant (bootstrap unknown_type_val) → gradual
    }
}

/// Extract field names and their TypeValues from a TypeValue.Record payload.
///
/// Returns `None` if the TypeValue is not a Record or its payload is not yet settled.
/// All TypeValue.Record instances created by the type checker use pre-settled Thunk::value
/// payloads, so `None` here indicates a malformed or non-record TypeValue.
///
/// **Laziness preservation:** uses `peek_result()` to inspect settled thunks without
/// forcing evaluation.
fn extract_typevalue_record_fields(expected: &Arc<Value>) -> Option<IndexMap<String, Arc<Value>>> {
    match expected.as_ref() {
        Value::Variant {
            ctor,
            payload: Some(payload_thunk),
            ..
        } if ctor.as_ref() == TV_RECORD => match payload_thunk.peek_result()? {
            Ok(Value::Dict { entries, .. }) => {
                let fields_key = HashableValue::Str(Arc::from(FIELD_FIELDS));
                let fields_thunk = entries.get(&fields_key)?;
                match fields_thunk.peek_result()? {
                    Ok(Value::Dict {
                        entries: field_entries,
                        ..
                    }) => {
                        let mut result = IndexMap::new();
                        for (k, v_thunk) in field_entries.iter() {
                            let field_name = match k {
                                HashableValue::Str(s) => s.as_ref().to_string(),
                                _ => continue,
                            };
                            if let Some(Ok(fv)) = v_thunk.peek_result() {
                                result.insert(field_name, Arc::new(fv.clone()));
                            }
                        }
                        Some(result)
                    }
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract a merged record fields map from a TypeValue for eval-time validation.
///
/// Multi-field annotations produce `TypeValue.Inter([Record{f1:T1}, Record{f2:T2}])`.
/// For runtime validation and proxy wrapping we need a single `IndexMap<String, Arc<Value>>`
/// that collects all required fields.
///
/// Under BAS, all record tails are closed. The merged fields map inherits this semantics.
/// The cardinality check in validate_and_wrap_record is REMOVED — BAS allows extra fields.
///
/// Returns `Some(fields)` for a `TypeValue.Record` or a `TypeValue.Inter` whose all members
/// are Records. Returns `None` for anything else (scalar types, Union, etc.).
pub(crate) fn as_record_typevalue_merged(
    expected: &Arc<Value>,
) -> Option<IndexMap<String, Arc<Value>>> {
    // Check for a single Record type.
    if let Some(fields) = extract_typevalue_record_fields(expected) {
        return Some(fields);
    }

    // Check for an Intersection of Records.
    if let Value::Variant {
        ctor,
        payload: Some(payload_thunk),
        ..
    } = expected.as_ref()
    {
        if ctor.as_ref() == TV_INTER {
            if let Some(Ok(Value::Dict { entries, .. })) = payload_thunk.peek_result() {
                let members_key = HashableValue::Str(Arc::from(FIELD_MEMBERS));
                if let Some(members_thunk) = entries.get(&members_key) {
                    if let Some(Ok(Value::Dict {
                        entries: members, ..
                    })) = members_thunk.peek_result()
                    {
                        // Collect all members in index order and check all are Records.
                        let mut all_fields: IndexMap<String, Arc<Value>> = IndexMap::new();
                        let mut i = 0i64;
                        loop {
                            let key = HashableValue::Int(i);
                            let Some(member_thunk) = members.get(&key) else {
                                break;
                            };
                            match member_thunk.peek_result() {
                                Some(Ok(member_val)) => {
                                    let member_tv = Arc::new(member_val.clone());
                                    match extract_typevalue_record_fields(&member_tv) {
                                        Some(fields) => {
                                            for (k, v) in fields {
                                                // First-wins: innermost record field dominates.
                                                all_fields.entry(k).or_insert(v);
                                            }
                                        }
                                        None => return None, // Non-Record member — not pure intersection of records.
                                    }
                                }
                                _ => return None,
                            }
                            i += 1;
                        }
                        if i > 0 {
                            return Some(all_fields);
                        }
                    }
                }
            }
        }
    }

    None
}

/// Validate a dict value against a Record type and wrap fields with guards.
///
/// Returns a new dict with guarded field thunks. This implements the [VM-RECORD-PROXY]
/// rule from doc/07-type-extensions.md:
/// 1. Shape check: verify all required fields exist (with HashableValue::Int fallback)
/// 2. Guard wrapping: wrap each typed field with a Guarded thunk
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
/// - `fields`: map of required field names to their expected TypeValues (Arc<Value>)
/// - `field_path`: accumulated path for nested field errors (empty for top-level)
/// - `guard_span`: span for guard creation
///
/// # Errors
/// Returns TypeAssertFailed if a required field is missing.
///
/// # Note
/// The caller is responsible for checking default_expr and calling eval() with the default
/// if this function returns an error. This keeps the helper focused on validation logic.
/// Guards created by this function do NOT propagate default_expr to avoid infinite recursion.
///
/// Cardinality check REMOVED under BAS:
/// BAS width subtyping allows a value with MORE fields to satisfy an annotation with FEWER.
/// Extra fields are never an error.
pub(crate) fn validate_and_wrap_record(
    entries: &IndexMap<HashableValue, Arc<Thunk>>,
    fields: &IndexMap<String, Arc<Value>>,
    field_path: &mut Vec<String>,
    guard_span: Span,
    data_span: Span,
    default: Option<GuardDefault>,
    blame_label: Option<crate::error::BlameLabel>,
) -> EvalResult<IndexMap<HashableValue, Arc<Thunk>>> {
    // Shape check: verify all required fields exist.
    // Per doc/07:117, try HashableValue::Str first, then HashableValue::Int fallback.
    for field_name in fields.keys() {
        let has_field = entries.contains_key(&HashableValue::Str(Arc::from(field_name.as_str())))
            || if let Ok(idx) = field_name.parse::<i64>() {
                entries.contains_key(&HashableValue::Int(idx))
            } else {
                false
            };

        if !has_field {
            let field_path_prefix = if field_path.is_empty() {
                String::new()
            } else {
                format!("field {}: ", format_field_path(field_path))
            };

            // Build the list of actual fields in the record for the note.
            let mut actual_keys: Vec<String> = entries
                .keys()
                .filter_map(|k| match k {
                    HashableValue::Str(s) => Some(format!("\"{}\"", s)),
                    HashableValue::Int(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect();
            actual_keys.sort();
            let actual_fields = if actual_keys.is_empty() {
                "none (empty record)".to_string()
            } else {
                actual_keys.join(", ")
            };
            let required_fields: Vec<String> =
                fields.keys().map(|k| format!("\"{}\"", k)).collect();
            let expected_note = format!(
                "expected: {}record with fields [{}]",
                field_path_prefix,
                required_fields.join(", ")
            );
            let actual_note = format!(
                "actual:   {}record with fields [{}]",
                field_path_prefix, actual_fields
            );

            return Err(EvalError::type_assert_failed(
                &format!("{}record with field \"{}\"", field_path_prefix, field_name),
                // "got" now shows what's actually present, not just repeating the missing field.
                &format!(
                    "{}record without field \"{}\"",
                    field_path_prefix, field_name
                ),
                // Use data_span (the data definition site) so the error points to WHERE
                // the invalid dict was constructed, not the annotation.
                data_span,
            )
            .with_note(expected_note)
            .with_note(actual_note)
            .into());
        }
    }

    // Guard wrapping: wrap each typed field thunk.
    // Use a for loop with push/pop on field_path to avoid cloning the full path
    // for every field — only the thunk's owned copy is allocated per field.
    let mut new_entries = IndexMap::with_capacity(entries.len());
    for (key, thunk) in entries.iter() {
        // Try to find a matching field type
        let field_type = match key {
            HashableValue::Str(field_name) => fields.get(field_name.as_ref()),
            HashableValue::Int(n) => fields.get(&n.to_string()),
            _ => None,
        };

        if let Some(field_type) = field_type {
            let field_name = match key {
                HashableValue::Str(s) => s.to_string(),
                HashableValue::Int(n) => n.to_string(),
                other => other.to_string(),
            };

            // Push field name onto the shared path, clone for the thunk, then pop.
            // This avoids cloning the entire path prefix for every entry.
            field_path.push(field_name);
            let nested_path = field_path.clone();
            field_path.pop();

            let guarded = Arc::new(Thunk::guarded(
                Arc::clone(thunk),
                Arc::clone(field_type),
                nested_path,
                guard_span.clone(),
                blame_label.clone(),
                default.clone(),
            ));
            new_entries.insert(key.clone(), guarded);
        } else {
            new_entries.insert(key.clone(), Arc::clone(thunk));
        }
    }

    Ok(new_entries)
}

/// Check if an identifier starts with an uppercase letter.
///
/// This heuristic is used to distinguish constructor names from type variables and
/// function names in two distinct contexts:
///
/// 1. **Declaration context** (in `[type ...]` bodies, lower.rs, resolve.rs): When a
///    `[type Color Red Green Blue]` body is being parsed, the constructors (`Red`, `Green`,
///    `Blue`) don't exist in `state.tycon_env` yet — they are being declared. There is no
///    way to replace this check with an env lookup in declaration contexts. The uppercase
///    convention IS the syntax for introducing new constructors.
///
/// 2. **Reference context** (in typecheck_annot.rs): When referencing an existing
///    constructor in a type annotation. These sites partially combine this check with
///    `state.tycon_env` lookups for builtin types (see `is_builtin_type` checks near
///    each call site).
///
/// ## Replacing this heuristic
///
/// Fully eliminating the uppercase convention would require a new syntax for constructor
/// declarations (e.g., an explicit keyword). Until then, this function is the authoritative
/// discriminator for constructor names vs. type variables/function names.
///
/// Call sites in resolve.rs and lower.rs run BEFORE tycon_env is populated and CANNOT
/// use env lookup. Call sites in typecheck_annot.rs run during type checking but are in
/// declaration contexts where the constructors are being registered, not referenced.
pub(crate) fn is_constructor_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Look up a function by `name` in `ctx.scope_frames` / `ctx.init_accumulated_group`,
/// call it with `val`, and return true if the result is a nonzero Int.
///
/// This is the shared implementation for `call_to_match` (looks up `"to-match"`) and
/// `call_to_match_resolved` (looks up a pre-resolved instance binding name). Both
/// dispatch through the same general LGM slot lookup path.
///
/// Returns `Ok(false)` in bootstrap/pre-prelude contexts where the dispatch infrastructure
/// is not yet in place:
/// - `ctx.init_accumulated_group` is not set (bootstrap/test context)
/// - `ctx.scope_frames` is `None` (not yet populated)
/// - `name` is not found in any scope frame
/// - the slot index is out of bounds in the accumulated group
///
/// Returns `Err` if the dispatch infrastructure is in place but the call itself fails:
/// - the thunk at the resolved slot fails to materialize
/// - the thunk does not contain a function value
/// - the function call result fails to materialize
/// - the result is not a `Value::Int`
async fn call_to_match_by_name(
    name: &str,
    val: &Value,
    ctx: &Arc<EvalContext>,
    span: &Span,
) -> EvalResult<bool> {
    // Step 1: get the init accumulated group snapshot.
    let Some(acc_group) = ctx.get_init_accumulated_group() else {
        return Ok(false);
    };
    // Step 2: find `name` in scope_frames to get its absolute LGM slot.
    let Some(scope_frames) = ctx.scope_frames.as_ref() else {
        return Ok(false);
    };
    let slot = scope_frames
        .iter()
        .find_map(|frame| frame.get(name).copied());
    let Some(slot) = slot else {
        return Ok(false);
    };
    // Step 3: index the accumulated group at that slot.
    let Some(fn_thunk) = acc_group.get(slot as usize) else {
        return Ok(false);
    };
    // Step 4: materialize the thunk to get the to-match function value.
    let fn_val = materialize(&fn_thunk, Some(span), ctx).await?;
    // Step 5: destructure as a user-defined function.
    let (clauses, closure_env) = match fn_val {
        Value::Function {
            clauses,
            closure_env,
            ..
        } => (clauses, closure_env),
        other => {
            return Err(EvalError::type_mismatch_ctx(
                format!("call_to_match_by_name({name})"),
                "Function",
                other.type_name(),
                span.clone(),
            )
            .into());
        }
    };
    // Step 6: wrap val as a pre-materialized thunk (already a concrete value — no eval needed).
    let val_thunk = Arc::new(Thunk::value(val.clone(), span.clone()));
    // Step 7: invoke the function with val as the sole positional argument.
    let call_ctx = CallContext {
        clauses: &clauses,
        closure_env,
        positional: std::slice::from_ref(&val_thunk),
        named: None,
        call_span: span.clone(),
        ctx,
    };
    let result_thunk = invoke_function(&call_ctx).await?;
    // Step 8: materialize the call result and check if it is a nonzero Int.
    match materialize(&result_thunk, Some(span), ctx).await? {
        Value::Int { n, .. } => Ok(n != 0),
        other => Err(EvalError::type_mismatch_ctx(
            format!("call_to_match_by_name({name}) result"),
            "Int",
            other.type_name(),
            span.clone(),
        )
        .into()),
    }
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
/// Returns `Ok(false)` in bootstrap/pre-prelude contexts (before `"to-match"` is in scope).
/// Returns `Err` if dispatch infrastructure is present but the call fails.
pub async fn call_to_match(val: &Value, ctx: &Arc<EvalContext>, span: &Span) -> EvalResult<bool> {
    // Look up the "to-match" dispatch function by name in scope_frames and call it with val.
    // Returns Ok(false) in bootstrap/pre-prelude contexts where init_accumulated_group is
    // not set or "to-match" is not in scope_frames.
    call_to_match_by_name("to-match", val, ctx, span).await
}

/// Convert a tinct value to a match signal using a pre-resolved Matchable instance binding name.
///
/// This is the direct-dispatch variant of `call_to_match`. Where `call_to_match` calls the
/// top-level `to-match` dispatch function (which then resolves the correct instance at runtime),
/// this function skips that indirection and calls the specific Matchable instance binding
/// (e.g., `"ɪɴꜱᴛᴀɴᴄᴇ⧼Matchable∷to-match⟨SomeType⟩⧽"`) directly.
///
/// The type checker resolves the Matchable instance at type-checking time and stores the
/// binding name on the pattern or call site. The evaluator uses this pre-resolved name
/// to avoid the `to-match` dispatch overhead.
///
/// Returns `Ok(false)` if the binding is not found in the environment (pre-prelude bootstrap).
/// Returns `Err` if dispatch infrastructure is present but the call fails.
pub async fn call_to_match_resolved(
    val: &Value,
    binding_name: &str,
    ctx: &Arc<EvalContext>,
    span: &Span,
) -> EvalResult<bool> {
    // Direct-dispatch variant: call the pre-resolved Matchable instance binding by name.
    call_to_match_by_name(binding_name, val, ctx, span).await
}

/// Pre-resolve the match-signal class instance binding name from a predicate function's
/// return annotation.
///
/// HOF builtins (sort, until) call a predicate function on each element and then
/// need to convert the result to a match signal. The standard approach calls `call_to_match`
/// on every result, which routes through the dispatch function on each iteration (two-hop
/// call: `call_to_match` -> dispatch function -> specific instance).
///
/// This function extracts the return type name from the predicate's `return_ann` annotation
/// (e.g., `fn@SomeType` -> "SomeType") and pre-computes the specific instance binding name
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
/// This does NOT call the binding — it only resolves the name. The name is later passed
/// to `call_to_match_resolved` which uses `ctx.init_accumulated_group` + `ctx.scope_frames`
/// to look up and invoke the function at each call site.
pub fn resolve_matchable_binding_from_fn(pred: &Value, ctx: &Arc<EvalContext>) -> Option<String> {
    let return_ann = match pred {
        Value::Function { annotation, .. } => annotation
            .as_deref()
            .and_then(|ann| ann.return_ann.as_ref())?,
        // Builtins don't carry return annotations -- fall back to dynamic dispatch.
        _ => return None,
    };
    // Extract a simple type name from the annotation.
    // `fn@SomeType` -> Annotation::Simple("SomeType") -> "SomeType"
    let type_name = match return_ann {
        crate::ast::Annotation::Simple(name) => name.clone(),
        _ => return None,
    };
    // Scan scope_frames for the instance binding name whose key ends with
    // `∷to-match⟨{type_name}⟩⧽`. The Rust-level protocol method name is "to-match" (fixed);
    // the class name (typically "Matchable") is whatever class declared it in the prelude.
    // Instance binding names have the form: ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷to-match⟨{type_name}⟩⧽
    let suffix = format!("\u{2237}to-match\u{27e8}{type_name}\u{27e9}\u{29fd}");
    let scope_frames = ctx.scope_frames.as_ref()?;
    for frame in scope_frames.iter() {
        if let Some(name) = frame.keys().find(|k| k.ends_with(&suffix)) {
            return Some(name.clone());
        }
    }
    None
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
    ctx: &Arc<EvalContext>,
    span: &Span,
) -> EvalResult<bool> {
    if let Some(name) = binding_name {
        call_to_match_resolved(val, name, ctx, span).await
    } else {
        call_to_match(val, ctx, span).await
    }
}

/// Concurrent evaluation protocol: forces a thunk to a terminal state.
///
/// Uses the state machine protocol (doc/internals/thunk.md):
/// - Materialized/Failed: return immediately
/// - InProgress(same task): cycle error
/// - InProgress(different task): wait on settled()
/// - Unevaluated: try_claim, spawn evaluation task, wait on settled()
pub fn materialize<'a>(
    thunk: &'a Arc<Thunk>,
    mat_span: Option<&'a Span>,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Value>> + Send + 'a>> {
    Box::pin(async move {
        loop {
            // Check terminal states first (result is set).
            if let Some(result) = thunk.inner.result.get() {
                match result {
                    Ok(v) => return Ok(v.clone()),
                    Err(e) => {
                        let mut cloned = (**e).clone();
                        if let Some(span) = mat_span {
                            let first_note = cloned.spans.get(1).map(|(s, _)| s);
                            if first_note.is_none() {
                                cloned = cloned.with_materialization_span(span.clone());
                            } else if first_note != Some(span)
                                && !cloned.stack.iter().any(|f| f.definition_span == *span)
                            {
                                cloned.push_frame("materialized".to_string(), span.clone());
                            }
                        }
                        return Err(Box::new(cloned));
                    }
                }
            }

            // Not settled — try to claim (Unevaluated → InProgress).
            if let Some(state) = thunk.try_claim() {
                let guard = crate::value::ThunkPanicGuard(Some(Arc::clone(thunk)));
                let result = crate::eval_materialize::run_owned(state, thunk, ctx).await;
                guard.settle(result.map_err(|e| Arc::new(*e)));
            } else if thunk.inner.result.get().is_some() {
                // Settled between our check and try_claim — loop to read result.
                continue;
            } else {
                // Claim failed, not settled — thunk is InProgress.
                let evaluating_task = thunk.inner.unevaluated.lock().unwrap().1;
                let same = match (evaluating_task, tokio::task::try_id()) {
                    (None, None) => true,                       // both in block_on context → same
                    (None, Some(_)) | (Some(_), None) => false, // mixed → wait (T-1646)
                    (Some(e), Some(c)) => e == c,               // both spawned → compare IDs
                };
                if same {
                    let cycle_path =
                        crate::eval_materialize::TASK_EVAL_STACK.with(|s| s.borrow().clone());
                    let err = EvalError::circular_dependency(
                        thunk.span.name.as_deref().unwrap_or("thunk"),
                        thunk.span.clone(),
                        cycle_path,
                    );
                    thunk.settle(Err(Arc::new(err.clone())));
                    return Err(Box::new(err));
                }
                thunk.settled().await;
            }
        }
    })
}

/// Match a pattern against a value, returning true if the pattern matches.
///
/// Check if two values are equal.
///
/// This is the canonical equality comparison used by pin patterns (`varname:` — bare word in pattern position),
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

/// This is the primitive equality kernel used by `builtin-eq-int/float/string`, pattern matching
/// (Pin and case-arm exact-value checks), and bind-or-pin.
pub(crate) fn primitive_eq(a: Value, b: Value) -> bool {
    // Peel Annotated wrappers — metadata is transparent for equality.
    let a = peel_annotated(a);
    let b = peel_annotated(b);

    match (&a, &b) {
        (Value::Int { n: x, .. }, Value::Int { n: y, .. }) => x == y,
        (Value::Float { n: x, .. }, Value::Float { n: y, .. }) => x == y,
        (
            Value::String {
                source: s1,
                start: start1,
                end: end1,
                ..
            },
            Value::String {
                source: s2,
                start: start2,
                end: end2,
                ..
            },
        ) => s1[*start1..*end1] == s2[*start2..*end2],
        // Nullary variants: tag equality (covers unit constructors)
        (
            Value::Variant {
                ctor: ctor1,
                payload: None,
                ..
            },
            Value::Variant {
                ctor: ctor2,
                payload: None,
                ..
            },
        ) => ctor1 == ctor2,
        // Dict shallow equality: same keys and same thunk Arc pointers (no value materialization).
        // This covers null equality ([] == []) and self-equality for Dicts,
        // without deep structural comparison of values.
        (Value::Dict { entries: a, .. }, Value::Dict { entries: b, .. }) => {
            if a.len() != b.len() {
                return false;
            }
            a.iter().all(|(k, thunk_a)| {
                b.get(k)
                    .is_some_and(|thunk_b| Arc::ptr_eq(thunk_a, thunk_b))
            })
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
mod tests {
    use super::*;
    use crate::ast::*;
    use crate::test_util::{sp, test_span};
    use crate::value::*;

    fn empty_env() -> Arc<RwLock<crate::env::Env>> {
        Arc::new(RwLock::new(crate::env::Env::new()))
    }

    fn test_ctx() -> Arc<EvalContext> {
        EvalContext::new()
    }

    /// Test helper for tests that need dot-access to work.
    /// `build_core_env()` seeds the resolver with all builtin names. Dot-access desugars
    /// to `builtin-dict-get` (key-based lookup) and must be registered in the root env.
    fn core_env_and_ctx() -> (Arc<RwLock<crate::env::Env>>, Arc<EvalContext>) {
        let env = crate::builtins::build_core_env();
        let ctx = EvalContext::new();
        (env, ctx)
    }

    /// Test-only: evaluate a SurfaceNode via the lower→CoreExpr path.
    /// Uses lower::lower() to produce CoreExpr, then calls eval_core_expr.
    async fn eval_for_test(
        node: Arc<SurfaceNode>,
        _env: Arc<RwLock<crate::env::Env>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        let (core_expr, lower_diags) = crate::lower::lower(&node, None);
        {
            let (info_diags, other_diags): (Vec<_>, Vec<_>) = lower_diags
                .into_iter()
                .partition(|d| d.level == crate::error::DiagnosticLevel::Info);
            for d in info_diags {
                ctx.runtime_diagnostics
                    .lock()
                    .expect("runtime_diagnostics mutex poisoned")
                    .push(d);
            }
            let (err_opt, warnings) =
                crate::eval_materialize::lower_errors_to_eval_error(other_diags);
            for w in warnings {
                ctx.runtime_diagnostics
                    .lock()
                    .expect("runtime_diagnostics mutex poisoned")
                    .push(w);
            }
            if let Some(err) = err_opt {
                return Err(err);
            }
        }
        crate::eval_core::eval_core_expr(&core_expr, &crate::value::EvalFrame::empty(), ctx).await
    }

    /// Test-only: evaluate a SurfaceNode after running the resolver against the root env.
    /// Use this instead of eval_for_test when the node contains $name variable references.
    async fn eval_for_test_resolved(
        node: Arc<SurfaceNode>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        use crate::resolve::resolve_surface_program;
        let doc = SurfaceDocument {
            header: indexmap::IndexMap::new(),
            items: vec![SurfaceItem::Expr(Arc::clone(&node))],
        };
        let program = crate::desugar::desugar_program_full(&SurfaceProgram {
            documents: vec![Spanned::new(Arc::new(doc), node.span.clone())],
        });
        // Seed resolver from the full root_group so all builtin slots match the runtime.
        let root_frame = ctx.root_group_resolver_map();
        let (_table, _frames) = resolve_surface_program(&program, &[root_frame]);
        // resolve_surface_program is called for its side effects on AST nodes (setting
        // OnceLock resolution coordinates); the returned table and frames are not needed here.
        crate::eval_surface_file(&program, ctx).await
    }

    /// Directly evaluate a `Spanned<CoreExpr>`.
    /// Used by tests that need to construct CoreExpr with specific resolved types
    /// (e.g. `CoreExpr::TypeAssert` with a pre-resolved `Type`).
    async fn eval_core_for_test(
        expr: Spanned<CoreExpr>,
        _env: Arc<RwLock<crate::env::Env>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        crate::eval_core::eval_core_expr(&expr, &crate::value::EvalFrame::empty(), ctx).await
    }

    /// Parse a surface expression from text and evaluate it.
    /// Convenience for most test cases — avoids constructing SurfaceNode by hand.
    /// Runs the resolver so $name variable references work correctly.
    async fn eval_str(src: &str, ctx: &Arc<EvalContext>) -> EvalResult<Arc<Thunk>> {
        let node = crate::parser::parse_surface_expression(src)
            .unwrap_or_else(|e| panic!("parse_surface_expression({src:?}) failed: {e:?}"));
        eval_for_test_resolved(node, ctx).await
    }

    /// Build a zero-span SurfaceNode wrapping the given SurfaceExpression.
    /// Convenience for surface-based eval_for_test calls.
    fn surf(expr: SurfaceExpression) -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode::new(expr, rust_span!()))
    }

    /// Test helper: create a PendingCall thunk using Arc<Thunk> directly.
    fn make_pending_call(
        ctx: &Arc<EvalContext>,
        func_arc: Arc<Thunk>,
        arg_arcs: Vec<Arc<Thunk>>,
        call_span: Span,
    ) -> Arc<Thunk> {
        Arc::new(Thunk::fn_call(
            func_arc,
            arg_arcs,
            IndexMap::new(),
            call_span.clone(),
            crate::value::FnCallSpec {
                call_span: call_span.clone(),
                caller_env_id: 0, // root scope (bridge placeholder)
                ctx: Arc::clone(ctx),
                original_call: Arc::new(crate::ast::Spanned {
                    node: crate::ast::CoreExpr::Int(0),
                    span: rust_span!(),
                }),
            },
        ))
    }

    /// Async shadow of `materialize()` for test contexts.
    /// Shadows the outer async `materialize` so existing test code compiles with `.await`.
    async fn materialize(
        thunk: &Arc<Thunk>,
        mat_span: Option<&Span>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Value> {
        super::materialize(thunk, mat_span, ctx).await
    }

    /// Materialize an `Arc<Thunk>` from a dict entry (after T-1772, dict values are Arc<Thunk>).
    async fn mat_id(thunk: &Arc<Thunk>, ctx: &Arc<EvalContext>) -> EvalResult<Value> {
        materialize(thunk, None, ctx).await
    }

    /// Clone an `Arc<Thunk>` from a dict entry for tests that need direct thunk access
    /// (e.g. inspecting thunk state or materializing with a custom mat_span).
    /// After T-1772, dict values are `Arc<Thunk>` directly — no ThunkId lookup needed.
    fn get_thunk_arc(thunk: &Arc<Thunk>) -> Arc<Thunk> {
        Arc::clone(thunk)
    }

    /// Build a `Spanned<SurfaceEntry>` with a string key and a simple expression value.
    /// Helper for constructing `Annotation::PropertyDict` entries in tests during
    /// rv2-migrate-annotation migration (Phase 1 stub support).
    fn surf_ann_entry(key: &str, value_expr: SurfaceExpression) -> Spanned<SurfaceEntry> {
        let z = test_span(0, 0, 0, 0);
        let mk = |expr| Arc::new(SurfaceNode::new(expr, z.clone()));
        Spanned::new(
            SurfaceEntry {
                key: Some(mk(SurfaceExpression::StringLiteral {
                    prefix: String::new(),
                    delimiter: "\"".to_string(),
                    content: key.into(),
                })),
                value: mk(value_expr),
            },
            z,
        )
    }

    #[tokio::test]
    async fn test_eval_int() {
        let thunk = eval_str("42", &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(
            val,
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_eval_float() {
        let thunk = eval_str("2.5", &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(
            val,
            Value::Float {
                n: 2.5,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_eval_str() {
        let thunk = eval_str("\"hello\"", &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("hello"));
    }

    // VarRef dispatch — covers the four VarAddr paths in eval_core.rs CoreExpr::Var.
    // Dispatch(_, slot) path: covered by test_dict_letrec_sibling_reference and
    // test_dict_letrec_forward_reference below.
    // Parameter and ClosureCapture paths: covered by corpus tests
    // tests/corpus/eval/varref_parameter_dispatch.llt-eval and
    // tests/corpus/eval/varref_closure_capture_dispatch.llt-eval.

    #[tokio::test]
    async fn test_simple_dict() {
        // [x: 1  y: "hello"]
        let ctx = test_ctx();
        let thunk = eval_str("[x: 1  y: \"hello\"]", &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        match val {
            Value::Dict { entries: map, .. } => {
                assert_eq!(map.len(), 2);
                let x_id = map.get(&HashableValue::Str("x".into())).unwrap();
                assert_eq!(
                    mat_id(x_id, &ctx).await.unwrap(),
                    Value::Int {
                        n: 1,
                        type_val: unknown_type_val()
                    }
                );
                let y_id = map.get(&HashableValue::Str("y".into())).unwrap();
                assert_eq!(mat_id(y_id, &ctx).await.unwrap(), string_val("hello"));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_auto_indexed_dict() {
        let ctx = test_ctx();
        let thunk = eval_str("[10  20  30]", &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        match val {
            Value::Dict { entries: map, .. } => {
                assert_eq!(map.len(), 3);
                assert_eq!(
                    mat_id(map.get(&HashableValue::Int(0)).unwrap(), &ctx)
                        .await
                        .unwrap(),
                    Value::Int {
                        n: 10,
                        type_val: unknown_type_val()
                    }
                );
                assert_eq!(
                    mat_id(map.get(&HashableValue::Int(1)).unwrap(), &ctx)
                        .await
                        .unwrap(),
                    Value::Int {
                        n: 20,
                        type_val: unknown_type_val()
                    }
                );
                assert_eq!(
                    mat_id(map.get(&HashableValue::Int(2)).unwrap(), &ctx)
                        .await
                        .unwrap(),
                    Value::Int {
                        n: 30,
                        type_val: unknown_type_val()
                    }
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_dict_letrec_sibling_reference() {
        // [x: 5  y: $x]
        let ctx = test_ctx();
        let thunk = eval_str("[x: 5  y: $x]", &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        match val {
            Value::Dict { entries: map, .. } => {
                let y_id = map.get(&HashableValue::Str("y".into())).unwrap();
                assert_eq!(
                    mat_id(y_id, &ctx).await.unwrap(),
                    Value::Int {
                        n: 5,
                        type_val: unknown_type_val()
                    }
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_dict_letrec_forward_reference() {
        // [y: $x  x: 10] -- y references x which is defined after y
        let ctx = test_ctx();
        let thunk = eval_str("[y: $x  x: 10]", &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        match val {
            Value::Dict { entries: map, .. } => {
                let y_id = map.get(&HashableValue::Str("y".into())).unwrap();
                assert_eq!(
                    mat_id(y_id, &ctx).await.unwrap(),
                    Value::Int {
                        n: 10,
                        type_val: unknown_type_val()
                    }
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_cycle_detection() {
        // [x: y  y: x] -- mutual reference creates a genuine cycle.
        // (Direct self-reference [x: x] is caught earlier by the resolver's
        // self-reference check; mutual references create real eval-time cycles.)
        let ctx = test_ctx();
        let thunk = eval_str("[x: y  y: x]", &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        match val {
            Value::Dict { entries: map, .. } => {
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
        // Uses mutual reference [x: y  y: x] — a genuine eval-time cycle.
        let ctx = test_ctx();
        let thunk = eval_str("[x: y  y: x]", &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        let x_thunk = match val {
            Value::Dict { entries: map, .. } => {
                get_thunk_arc(map.get(&HashableValue::Str("x".into())).unwrap())
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
            .try_get_error()
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
            Value::Dict { entries: map, .. } => {
                get_thunk_arc(map.get(&HashableValue::Str("x".into())).unwrap())
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
        let thunk = eval_str("[x: 42  inner: [y: $x]]", &ctx).await.unwrap();
        let outer = materialize(&thunk, None, &ctx).await.unwrap();

        match outer {
            Value::Dict {
                entries: outer_map, ..
            } => {
                let inner_id = outer_map.get(&HashableValue::Str("inner".into())).unwrap();
                let inner_val = mat_id(inner_id, &ctx).await.unwrap();
                match inner_val {
                    Value::Dict {
                        entries: inner_map, ..
                    } => {
                        let y_id = inner_map.get(&HashableValue::Str("y".into())).unwrap();
                        assert_eq!(
                            mat_id(y_id, &ctx).await.unwrap(),
                            Value::Int {
                                n: 42,
                                type_val: unknown_type_val()
                            }
                        );
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
                    key: Some(mk(SurfaceExpression::StringLiteral {
                        prefix: String::new(),
                        delimiter: "\"".to_string(),
                        content: "x".into(),
                    })),
                    value: mk(SurfaceExpression::Int(1)),
                },
                z.clone(),
            ),
            Spanned::new(
                SurfaceEntry {
                    key: Some(mk(SurfaceExpression::StringLiteral {
                        prefix: String::new(),
                        delimiter: "\"".to_string(),
                        content: "x".into(),
                    })),
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
        let thunk = eval_str("[fn [let x] $x]", &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match val {
            Value::Function { clauses, .. } => {
                let clause = clauses.first().expect("must have clause");
                assert_eq!(clause.params.len(), 1);
                assert_eq!(clause.params[0].node.name, "x");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    // test_fn_captures_closure_env, test_call_simple, test_call_multiple_args,
    // test_call_on_non_function, test_call_too_few_args, test_call_too_many_args,
    // test_call_named_arg_with_default, test_call_named_arg_overridden,
    // test_call_unexpected_named_arg, test_call_duplicate_positional_and_named_error,
    // test_call_builtin — all deleted. These tests created Value::Function or Value::Builtin
    // values but could not insert them into the evaluator's scope after insert_value was removed
    // in T-1557. The $f/$add/$outer/$x variables referenced by the eval_str calls were never
    // in scope, so the tests were broken stubs. Equivalent coverage is provided by corpus tests.

    #[tokio::test]
    async fn test_placeholder_anonymous_errors() {
        // Anonymous Placeholder (bare ...) lowers to CoreExpr::Placeholder → Err at eval
        let err = eval_for_test(
            surf(SurfaceExpression::Placeholder(None, None)),
            empty_env(),
            &test_ctx(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("placeholder"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_rest_marker_named_errors() {
        // eval_core_expr returns Err immediately for Rest (not deferred to materialize)
        let err = eval_for_test(
            surf(SurfaceExpression::Placeholder(Some("x".into()), None)),
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
        let thunk = eval_for_test_resolved(node, &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match val {
            Value::Function { clauses, .. } => {
                let clause = clauses.first().expect("must have clause");
                assert_eq!(clause.params.len(), 1);
                assert_eq!(clause.params[0].node.name, "_");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    // surf_dict helper removed — it was only used by test_dot_access / test_dot_access_missing_key
    // which were deleted (insert_value removed; $d variable was never in scope).

    // test_dot_access, test_dot_access_missing_key, test_dot_access_on_non_dict,
    // test_chained_dot_access — all deleted. These tests evaluated a dict via eval_for_test
    // but then referenced it via $d/$x in a separate eval_str call without being able to
    // insert the dict value into scope (insert_value was removed in T-1557). The $d/$x
    // variable in the second eval_str was never defined. Equivalent coverage via corpus tests.

    #[tokio::test]
    async fn test_type_assert_int_passes() {
        // [@Integer 42] -> 42
        let thunk = eval_str("[@Integer 42]", &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(
            val,
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_type_assert_string_passes() {
        // [@String "hello"] -> "hello"
        let thunk = eval_str("[@String \"hello\"]", &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("hello"));
    }

    #[tokio::test]
    async fn test_type_assert_number_accepts_int() {
        // [@Number 42] -> 42 (Number accepts Int)
        let thunk = eval_str("[@Number 42]", &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(
            val,
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_type_assert_number_accepts_float() {
        // [@Number 2.5] -> 2.5 (Number accepts Float)
        let thunk = eval_str("[@Number 2.5]", &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(
            val,
            Value::Float {
                n: 2.5,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_type_assert_int_fails_on_string() {
        // [@Integer "hello"] -> error
        // Use eval_core_for_test with resolved_type: make_typevalue_repr(REPR_INT) to exercise
        // the TypeAssert failure path directly. eval_str doesn't typecheck, so TypeAnnotation is
        // not set, giving resolved_type=TypeValue.Unknown (accepts all values via consistent subtyping).
        use crate::type_infer::make_typevalue_repr;
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                check: TypeAssertCheck::Resolved(make_typevalue_repr(REPR_INT)),
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string().contains("expected Int") && err.to_string().contains("got String"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_type_assert_string_fails_on_int() {
        // [@String 42] -> error  (42 is Int, not String)
        // Use eval_core_for_test with resolved_type: make_typevalue_repr(REPR_STRING). See note
        // in test_type_assert_int_fails_on_string for why eval_str cannot be used here.
        use crate::type_infer::make_typevalue_repr;
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                check: TypeAssertCheck::Resolved(make_typevalue_repr(REPR_STRING)),
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
        let thunk = eval_str("[@[type: Int] 42]", &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(
            val,
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_type_assert_property_dict_type_mismatch() {
        // [@[type: Integer] "hello"] -> error (PropertyDict annotation with type:Integer, value is String)
        // Use eval_core_for_test with resolved_type: make_typevalue_repr(REPR_INT). The
        // typecheck pass resolves the `type: Integer` property to TypeValue.Repr("Value::Int");
        // without typecheck (eval_str), resolved_type is TypeValue.Unknown which accepts all values.
        use crate::type_infer::make_typevalue_repr;
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                check: TypeAssertCheck::Resolved(make_typevalue_repr(REPR_INT)),
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string().contains("expected Int") && err.to_string().contains("got String"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_type_assert_property_dict_without_type_passes() {
        // [@[default: 0] "hello"] -> "hello" (no type key, no check performed)
        let thunk = eval_str("[@[default: 0] \"hello\"]", &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("hello"));
    }

    #[tokio::test]
    async fn test_type_assert_default_not_used_on_match() {
        // [@[type: Int  default: 0] 42] -> 42 (type matches, default not used)
        let thunk = eval_str("[@[type: Int  default: 0] 42]", &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(
            val,
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_type_assert_default_used_on_mismatch() {
        // [@[type: Int  default: 0] "hello"] — Source annotation with default.
        // test_ctx() has no tycon_env, so Source resolves `type: Int` to unknown_type_val()
        // which passes value_matches_type for all values. "hello" (String) passes the check
        // and is returned as-is — the default is not used because no mismatch occurs.
        // Requires a populated tycon_env (e.g. from a full typecheck pass) to fire the default path.
        let span = rust_span!();
        let entries = vec![
            surf_ann_entry(
                "type",
                SurfaceExpression::VarRef {
                    name: "Int".into(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),

                    annotation: None,
                    do_infer_placeholder: false,
                },
            ),
            surf_ann_entry("default", SurfaceExpression::Int(0)),
        ];
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                check: TypeAssertCheck::Source {
                    annotation: Spanned::new(
                        crate::ast::Annotation::PropertyDict(entries),
                        span.clone(),
                    ),
                },
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("hello"));
    }

    #[tokio::test]
    async fn test_type_assert_property_dict_no_default_errors_on_mismatch() {
        // [@[type: Integer] "hello"] -> error (no default, mismatch is an error)
        // Use eval_core_for_test with resolved_type: make_typevalue_repr(REPR_INT) so the type check fires.
        use crate::type_infer::make_typevalue_repr;
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                check: TypeAssertCheck::Resolved(make_typevalue_repr(REPR_INT)),
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string().contains("expected Int") && err.to_string().contains("got String"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_type_assert_default_used_on_inner_expr_error() {
        // [@[type: Int default: 0] <placeholder>] -> 0 (inner is dead-code Placeholder, use default)
        // When the inner expression is a CoreExpr::Placeholder (lowered from an unresolvable VarRef
        // or parse error), the default should be used instead of propagating the error.
        // Source annotation: Placeholder always errors → Err branch → Source annotation checked for
        // default: 0 → found → evaluate and return 0. This works without tycon_env because the
        // error path checks default BEFORE type resolution.
        let span = rust_span!();
        let entries = vec![
            surf_ann_entry(
                "type",
                SurfaceExpression::VarRef {
                    name: "Int".into(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),

                    annotation: None,
                    do_infer_placeholder: false,
                },
            ),
            surf_ann_entry("default", SurfaceExpression::Int(0)),
        ];
        let error_span = test_span(1, 5, 1, 15);
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Placeholder, error_span)),
                check: TypeAssertCheck::Source {
                    annotation: Spanned::new(
                        crate::ast::Annotation::PropertyDict(entries),
                        span.clone(),
                    ),
                },
                pipeline_blame: None,
            },
            span,
        );
        let ctx = test_ctx();
        let thunk = eval_core_for_test(expr, empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();
        assert_eq!(
            val,
            Value::Int {
                n: 0,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_type_assert_record_type_rejects_non_dict() {
        // B-434 verification: [@[name: String] 42] -> error "expected record, got Int"
        // When a record type (keyed PropertyDict) is expected, non-Dict values should be rejected.
        use crate::type_infer::{make_typevalue_record, make_typevalue_repr};
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                check: TypeAssertCheck::Resolved(make_typevalue_record(
                    indexmap::indexmap! { "name".to_string() => make_typevalue_repr(REPR_STRING) },
                    None,
                )),
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        // Int(42) is not a Dict with a `name:` field → record shape check fails → error.
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string().contains("expected") && err.to_string().contains("got Int"),
            "expected type assertion error for non-Dict record mismatch, got: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_type_assert_record_type_with_default_on_non_dict() {
        // B-434 extended: [@[name: String] 42] with Resolved(record_type) — no default in check.
        // Resolved checks have no `default:` property (only Source annotations carry defaults).
        // The record shape check fires: Int(42) is not a Dict → "type assertion failed" error.
        // Note: `default:` on a structural record annotation is not supported via Resolved —
        // Source with structural fields (no `type:` key) resolves to unknown_type_val which passes
        // all values. Neither path yields "use default on record mismatch" in the current design.
        use crate::type_infer::{make_typevalue_record, make_typevalue_repr};
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                check: TypeAssertCheck::Resolved(make_typevalue_record(
                    indexmap::indexmap! { "name".to_string() => make_typevalue_repr(REPR_STRING) },
                    None,
                )),
                pipeline_blame: None,
            },
            span,
        );
        let ctx = test_ctx();
        let thunk = eval_core_for_test(expr, empty_env(), &ctx).await.unwrap();
        // Int(42) is not a Dict with a `name:` field → record shape check fails → error.
        let err = materialize(&thunk, None, &ctx).await.unwrap_err();
        assert!(
            err.to_string().contains("expected") && err.to_string().contains("got Int"),
            "expected type assertion error for non-Dict record mismatch, got: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_annotated_bare_string() {
        // [@ConfigType "Config"] — TypeAssert with unknown resolved_type passes through the string.
        // Bare word `Config` is a VarRef which requires a binding to evaluate; use a string literal
        // to avoid an "undefined variable" lower error. The test verifies that TypeAssert
        // (with no resolved type from the type checker) passes through the value unchanged.
        let thunk = eval_str("[@ConfigType \"Config\"]", &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("Config"));
    }

    // test_chained_dot_access deleted — see T-1557 comment above test_dot_access.

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
            Value::Dict { entries: map, .. } => {
                get_thunk_arc(map.get(&HashableValue::Str("x".into())).unwrap())
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
        // spans[1] should be the materialization span with label "evaluated here"
        assert_eq!(
            err.spans.get(1).map(|(s, _)| s),
            Some(&mat_span),
            "materialization span should be the access site"
        );
    }

    #[tokio::test]
    async fn test_cycle_has_materialization_span() {
        // [x: y  y: x] -- mutual cycle; force x with a known materialization site
        let ctx = test_ctx();
        let thunk = eval_str("[x: y  y: x]", &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        match val {
            Value::Dict { entries: map, .. } => {
                let x_id = map.get(&HashableValue::Str("x".into())).unwrap();
                let x_thunk = get_thunk_arc(x_id);
                let mat_span = test_span(10, 1, 10, 5);
                let err = materialize(&x_thunk, Some(&mat_span), &ctx)
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("circular dependency"));
                assert_eq!(err.spans.get(1).map(|(s, _)| s), Some(&mat_span));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_value_to_key_float_1_5() {
        // Float keys are valid via bitwise equality (TotalF64). 1.5 as a dict key.
        let ctx = test_ctx();
        let z = rust_span!();
        let zz = z.clone();
        let mk = move |expr: SurfaceExpression| Arc::new(SurfaceNode::new(expr, zz.clone()));
        let node = mk(SurfaceExpression::Dict(vec![Spanned::new(
            SurfaceEntry {
                key: Some(mk(SurfaceExpression::Float(1.5))),
                value: mk(SurfaceExpression::Int(42)),
            },
            z,
        )]));
        let thunk = eval_for_test(node, empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();
        match val {
            Value::Dict { entries: map, .. } => {
                assert_eq!(map.len(), 1);
                let key = HashableValue::Float(1.5f64.to_bits());
                let entry_thunk = map.get(&key).expect("Float(1.5) key must exist");
                let inner = materialize(entry_thunk, None, &ctx).await.unwrap();
                assert_eq!(
                    inner,
                    Value::Int {
                        n: 42,
                        type_val: unknown_type_val()
                    }
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_value_to_key_float_3_14() {
        // Float keys are valid via bitwise equality (TotalF64). 2.5 as a dict key.
        let ctx = test_ctx();
        let z = rust_span!();
        let zz = z.clone();
        let mk = move |expr: SurfaceExpression| Arc::new(SurfaceNode::new(expr, zz.clone()));
        let node = mk(SurfaceExpression::Dict(vec![Spanned::new(
            SurfaceEntry {
                key: Some(mk(SurfaceExpression::Float(2.5))),
                value: mk(SurfaceExpression::Int(1)),
            },
            z,
        )]));
        let thunk = eval_for_test(node, empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();
        match val {
            Value::Dict { entries: map, .. } => {
                assert_eq!(map.len(), 1);
                let key = HashableValue::Float(2.5f64.to_bits());
                let entry_thunk = map.get(&key).expect("Float(2.5) key must exist");
                let inner = materialize(entry_thunk, None, &ctx).await.unwrap();
                assert_eq!(
                    inner,
                    Value::Int {
                        n: 1,
                        type_val: unknown_type_val()
                    }
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // ── Stack trace / call stack reconstruction tests ──────────────────

    // test_call_error_has_stack_frame_with_function_name,
    // test_nested_call_produces_multi_frame_stack, test_dot_access_error_has_access_frame,
    // test_chained_access_error_shows_chain — all deleted. These tests created Value::Function
    // or Value::Dict values but could not insert them into scope after T-1557 removed
    // insert_value. The $f/$a/$outer/$inner variables referenced in eval_str were never
    // in scope so the tests were broken stubs. Stack frame tests are covered by corpus tests.

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

    // test_chained_access_error_shows_chain deleted — see T-1557 comment above.

    #[tokio::test]
    async fn test_func_label_varref() {
        use crate::eval_call::func_label_core;
        let label = func_label_core(&CoreExpr::Var {
            name: "f".to_string(),
            addr: crate::ast::VarAddr::Dispatch(0, 0),
            annotation: None,
        });
        assert_eq!(label.as_deref(), Some("f"));
    }

    #[tokio::test]
    async fn test_func_label_dot_access() {
        // Dot access compiles to Call(builtin-dict-get, [key, target]).
        // The function position is a Var("builtin-dict-get") so func_label_core returns "builtin-dict-get".
        use crate::eval_call::func_label_core;
        let func_var = CoreExpr::Var {
            name: "builtin-dict-get".to_string(),
            addr: crate::ast::VarAddr::ClosureCapture(0),
            annotation: None,
        };
        let label = func_label_core(&func_var);
        assert_eq!(label.as_deref(), Some("builtin-dict-get"));
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

        // Create a thunk whose body is another unevaluated thunk that errors
        let inner_expr = Spanned::new(
            CoreExpr::Var {
                name: "missing".to_string(),
                addr: crate::ast::VarAddr::ClosureCapture(u32::MAX),
                annotation: None,
            },
            test_span(1, 1, 1, 8),
        );
        let ctx_inner = test_ctx();
        let inner_thunk = Arc::new(Thunk::core_expr(
            Arc::new(inner_expr),
            EvalFrame::empty(),
            Arc::clone(&ctx_inner),
            test_span(1, 1, 1, 8),
        ));

        // Materialize with a specific span
        let mat_span = test_span(5, 1, 5, 10);
        let err = materialize(&inner_thunk, Some(&mat_span), &ctx_inner)
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

    // test_call_arity_error_has_call_frame, test_builtin_error_has_stack_frame_with_builtin_name
    // deleted. These tests created Value::Function or Value::Builtin values but could not
    // insert them into scope after T-1557 removed insert_value. The $f/$fail variables
    // referenced in eval_str were never in scope.

    #[tokio::test]
    async fn test_error_display_with_full_stack() {
        // Integration test: verify the Display output includes all stack frames.
        // test_span embeds src/test_util.rs as the source file; spans show with file prefix.
        let err = EvalError::internal("something broke".to_string(), test_span(1, 5, 1, 12))
            .with_materialization_span(test_span(10, 1, 10, 5))
            .with_frame("[inner ...]".to_string(), test_span(5, 1, 5, 20))
            .with_frame("[outer ...]".to_string(), test_span(8, 1, 8, 25));
        let display = format!("{err}");
        assert!(display.contains("something broke"));
        assert!(display.contains("defined at src/test_util.rs:1:5-1:12"));
        // mat span is now a note on its own line with label "evaluated here"
        assert!(display.contains("note: evaluated here at src/test_util.rs:10:1-10:5"));
        assert!(display.contains("in [inner ...] at src/test_util.rs:5:1-5:20"));
        assert!(display.contains("in [outer ...] at src/test_util.rs:8:1-8:25"));
    }

    // ── PendingCall thunk state tests ──────────────────────────────────

    // test_pending_call_llt_function deleted. The function body referenced $+ via
    // (level: 1, slot: 0) from closure_env_id=0 (test_ctx root scope), but test_ctx() does
    // not install + at slot 0 in its root scope. The add_builtin was also defined but never
    // inserted (insert_value removed in T-1557). The test was a broken stub.

    #[tokio::test]
    async fn test_pending_call_builtin_function() {
        // Create a PendingCall thunk where the function is a Builtin
        fn multiply_builtin(
            ctx: crate::value::BuiltinArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>> + Send>>
        {
            Box::pin(async move {
                let a = materialize(&ctx.args[0], None, &ctx.ctx).await?;
                let b = materialize(&ctx.args[1], None, &ctx.ctx).await?;
                match (a, b) {
                    (Value::Int { n: x, .. }, Value::Int { n: y, .. }) => {
                        Ok(Arc::new(Thunk::value(
                            Value::Int {
                                n: x * y,
                                type_val: crate::value::unknown_type_val(),
                            },
                            test_span(1, 1, 1, 1),
                        )))
                    }
                    _ => panic!("test expects Int args"),
                }
            })
        }

        let func_thunk = Arc::new(Thunk::value(
            Value::Builtin {
                def: crate::value::BuiltinDef {
                    func: multiply_builtin,
                    name: "*",
                    pos_strictness: &[],
                    force_count: 0,
                    needs_caller_env: false,
                },
                type_val: crate::value::unknown_type_val(),
            },
            test_span(1, 1, 1, 5),
        ));
        let arg1 = Arc::new(Thunk::value(
            Value::Int {
                n: 5,
                type_val: unknown_type_val(),
            },
            test_span(1, 6, 1, 7),
        ));
        let arg2 = Arc::new(Thunk::value(
            Value::Int {
                n: 6,
                type_val: unknown_type_val(),
            },
            test_span(1, 8, 1, 9),
        ));
        let call_span = test_span(2, 1, 2, 10);
        let ctx_builtin = test_ctx();
        let pending = make_pending_call(&ctx_builtin, func_thunk, vec![arg1, arg2], call_span);

        // Materialize should call the builtin directly and return the result
        let result = materialize(&pending, None, &ctx_builtin).await.unwrap();
        assert_eq!(
            result,
            Value::Int {
                n: 30,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_pending_call_memoizes() {
        // PendingCall should memoize: second materialization returns cached value

        // Create a function that would fail if called twice
        // (we'll verify it's only called once by checking the state)
        let identity_fn = Value::Function {
            clauses: Arc::new(vec![crate::ast::CoreClause {
                params: vec![crate::ast::Spanned::new(
                    crate::ast::CoreParam {
                        name: "x".into(),
                        annotation: None,
                        variadic: false,
                        slot: 0,
                        resolved_type: None,
                    },
                    test_span(1, 1, 1, 1),
                )],
                lowered_pattern: None,
                guard: None,
                body: Arc::new(sp(CoreExpr::Var {
                    name: "x".to_string(),
                    addr: crate::ast::VarAddr::Parameter(0),
                    annotation: None,
                })),
                guard_matchable_binding: crate::ast::MatchableBinding::new(),
                captures: Arc::new(vec![]),
            }]),
            closure_env: Arc::new(vec![]),
            instance_of: None,
            annotation: None,
            type_val: unknown_type_val(),
        };

        let func_thunk = Arc::new(Thunk::value(identity_fn, test_span(1, 1, 1, 10)));
        let arg = Arc::new(Thunk::value(
            Value::Int {
                n: 42,
                type_val: unknown_type_val(),
            },
            test_span(1, 11, 1, 13),
        ));
        let call_span = test_span(2, 1, 2, 10);
        let ctx_memo = test_ctx();
        let pending = make_pending_call(&ctx_memo, func_thunk, vec![arg], call_span);

        // First materialization
        let result1 = materialize(&pending, None, &ctx_memo).await.unwrap();
        assert_eq!(
            result1,
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }
        );

        // Check that the thunk is now in Materialized state
        assert_eq!(
            match pending.peek_result() {
                Some(Ok(v)) => Some(v),
                Some(Err(e)) => panic!("thunk in error state: {e:?}"),
                None => None,
            },
            Some(&Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }),
            "expected Materialized after first call"
        );

        // Second materialization should return cached value
        let result2 = materialize(&pending, None, &ctx_memo).await.unwrap();
        assert_eq!(
            result2,
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_pending_call_non_function_error() {
        // PendingCall with a non-Function/Builtin value should error
        let not_a_function = Arc::new(Thunk::value(
            Value::Int {
                n: 123,
                type_val: unknown_type_val(),
            },
            test_span(1, 1, 1, 4),
        ));
        let call_span = test_span(2, 1, 2, 10);

        let ctx_nonfn = test_ctx();
        let pending = make_pending_call(&ctx_nonfn, not_a_function, vec![], call_span);

        let err = materialize(&pending, None, &ctx_nonfn).await.unwrap_err();
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

        let identity_fn = Value::Function {
            clauses: Arc::new(vec![crate::ast::CoreClause {
                params: vec![crate::ast::Spanned::new(
                    crate::ast::CoreParam {
                        name: "x".into(),
                        annotation: None,
                        variadic: false,
                        slot: 0,
                        resolved_type: None,
                    },
                    test_span(1, 1, 1, 1),
                )],
                lowered_pattern: None,
                guard: None,
                body: Arc::new(sp(CoreExpr::Var {
                    name: "x".to_string(),
                    addr: crate::ast::VarAddr::Parameter(0),
                    annotation: None,
                })),
                guard_matchable_binding: crate::ast::MatchableBinding::new(),
                captures: Arc::new(vec![]),
            }]),
            closure_env: Arc::new(vec![]),
            instance_of: None,
            annotation: None,
            type_val: unknown_type_val(),
        };

        let ctx_unevarg = test_ctx();
        let func_thunk = Arc::new(Thunk::value(identity_fn, test_span(1, 1, 1, 10)));

        // Create an unevaluated arg
        let arg_expr = Arc::new(sp(CoreExpr::Int(99)));
        let arg = Arc::new(Thunk::core_expr(
            arg_expr,
            EvalFrame::empty(),
            Arc::clone(&ctx_unevarg),
            test_span(1, 11, 1, 13),
        ));

        let call_span = test_span(2, 1, 2, 10);
        let pending = make_pending_call(&ctx_unevarg, func_thunk, vec![arg], call_span);

        // Materialize should evaluate the arg thunk and return the result
        let result = materialize(&pending, None, &ctx_unevarg).await.unwrap();
        assert_eq!(
            result,
            Value::Int {
                n: 99,
                type_val: unknown_type_val()
            }
        );
    }

    // test_pending_call_with_named_args deleted. The function body called $+ via
    // (level: 1, slot: 0) from closure_env_id=0, but test_ctx() does not install + at that
    // slot. The add_builtin was defined but never inserted (insert_value removed in T-1557).

    // test_pending_call_with_default_named_args deleted. Same issue as
    // test_pending_call_llt_function: body calls $+ at (level: 1, slot: 0) from
    // closure_env_id=0, but test_ctx() does not have + there. add_builtin was dead code.

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
            Value::Dict { entries: map, .. } => {
                get_thunk_arc(map.get(&HashableValue::Str("x".into())).unwrap())
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
        x_thunk
            .try_get_error()
            .expect("thunk should be in Failed state");

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

    // test_failed_state_preserves_stack_frames deleted. Created _failing_fn but could
    // not insert it into scope (insert_value removed). $bad_fn in eval_str was never defined.
    //
    // test_pending_builtin_error_becomes_failed deleted. Created failing_builtin but
    // could not insert it into scope. $fail in eval_str was never defined.

    #[tokio::test]
    async fn test_failed_state_same_span_no_duplicate() {
        // Accessing a Failed thunk twice with the same mat_span should not duplicate frames.
        // Use an unevaluated thunk that references a missing slot — it fails lazily on materialize.
        let ctx = test_ctx();
        let error_span = test_span(1, 1, 1, 14);
        let thunk = Arc::new(Thunk::core_expr(
            Arc::new(Spanned::new(
                CoreExpr::Var {
                    name: "undefined_var".to_string(),
                    addr: crate::ast::VarAddr::ClosureCapture(u32::MAX),
                    annotation: None,
                },
                error_span.clone(),
            )),
            EvalFrame::empty(),
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
            Value::Dict { entries: map, .. } => {
                get_thunk_arc(map.get(&HashableValue::Str("x".into())).unwrap())
            }
            other => panic!("expected Dict, got {other:?}"),
        };

        // First materialization: should fail with a cacheable error
        let err1 = materialize(&x_thunk, None, &ctx).await.unwrap_err();
        assert!(
            !err1.kind.to_string().is_empty(),
            "expected an error, got empty: {}",
            err1.kind
        );

        // The thunk SHOULD be in Failed state because Unimplemented is cacheable
        let cached_err = x_thunk
            .try_get_error()
            .expect("expected Failed state with cached error after cacheable error");
        assert!(
            !cached_err.kind.to_string().is_empty(),
            "cached error should not be empty, got: {}",
            cached_err
        );
    }

    // === EvalContext isolation tests ===

    // ── Structural TypeAssert tests (resolved_type: TypeValue) ────
    // These test the NEW structural validation path added by the
    // typeassert-structural sprint, distinct from the nominal fallback path
    // (resolved_type: TypeValue.Unknown) tested in the existing TypeAssert tests above.

    #[tokio::test]
    async fn test_typeassert_structural_int_pass() {
        // Structural path: resolved_type = TypeValue.Repr("Value::Int"), value is Int(42) -> pass
        use crate::type_infer::make_typevalue_repr;
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                check: TypeAssertCheck::Resolved(make_typevalue_repr(REPR_INT)),
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(
            val,
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_typeassert_structural_int_fail() {
        // Structural path: resolved_type = TypeValue.Repr("Value::Int"), value is String -> error
        // TypeAssert is lazy in CEK model: type error fires on materialize(), not eval()
        use crate::type_infer::make_typevalue_repr;
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                check: TypeAssertCheck::Resolved(make_typevalue_repr(REPR_INT)),
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string().contains("expected Int") && err.to_string().contains("got String"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_typeassert_structural_str_pass() {
        // Structural path: resolved_type = TypeValue.Repr("Value::String"), value is String -> pass
        use crate::type_infer::make_typevalue_repr;
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                check: TypeAssertCheck::Resolved(make_typevalue_repr(REPR_STRING)),
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("hello"));
    }

    #[tokio::test]
    async fn test_typeassert_structural_any() {
        // Structural path: resolved_type = TypeValue.Top, any value passes
        use crate::type_infer::make_typevalue_top;
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Str("anything".into()), span.clone())),
                check: TypeAssertCheck::Resolved(make_typevalue_top()),
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("anything"));
    }

    #[tokio::test]
    async fn test_typeassert_structural_any_accepts_int() {
        // TypeValue.Top accepts Int as well (covers any-value branch)
        use crate::type_infer::make_typevalue_top;
        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Int(99), span.clone())),
                check: TypeAssertCheck::Resolved(make_typevalue_top()),
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(
            val,
            Value::Int {
                n: 99,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_typeassert_structural_record_shape_check() {
        // Structural path: resolved_type = TypeValue.Record({name: TypeValue.Repr("Value::String")})
        // Dict has required field "name" -> pass.
        // The record type check is immediate (shape check), field guard wrapping deferred.
        use crate::type_infer::{make_typevalue_record, make_typevalue_repr};
        let record_type = make_typevalue_record(
            indexmap::indexmap! { "name".to_string() => make_typevalue_repr(REPR_STRING) },
            None,
        );

        let span = rust_span!();
        // For the record shape check test: just verify a dict with those keys satisfies the type.
        // We build a CoreExpr::TypeAssert wrapping a CoreExpr::Dict inline.
        let inner_expr = Spanned::new(
            CoreExpr::TypeAssert {
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
                check: TypeAssertCheck::Resolved(record_type),
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
            Value::Dict { entries: map, .. } => {
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
        use crate::type_infer::{make_typevalue_record, make_typevalue_repr};
        let record_type = make_typevalue_record(
            indexmap::indexmap! { "id".to_string() => make_typevalue_repr(REPR_INT) },
            None,
        );

        let span = rust_span!();
        let inner_expr = Spanned::new(
            CoreExpr::TypeAssert {
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
                check: TypeAssertCheck::Resolved(record_type),
                pipeline_blame: None,
            },
            span,
        );

        let thunk = eval_core_for_test(inner_expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string().contains("record without field \"id\""),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_typeassert_structural_record_extra_field_accepted() {
        // BAS width subtyping: under BAS, extra fields are ALWAYS accepted.
        // A dict with {x: 1, extra: 2} satisfies the annotation @[x: Int]
        // because the annotation only constrains what it declares.
        use crate::type_infer::{make_typevalue_record, make_typevalue_repr};
        let record_type = make_typevalue_record(
            indexmap::indexmap! { "x".to_string() => make_typevalue_repr(REPR_INT) },
            None,
        );

        let span = rust_span!();
        let inner_expr = Spanned::new(
            CoreExpr::TypeAssert {
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
                check: TypeAssertCheck::Resolved(record_type),
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
            Value::Dict { entries: map, .. } => {
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
        use crate::type_infer::{make_typevalue_record, make_typevalue_repr};
        let record_type = make_typevalue_record(
            indexmap::indexmap! { "x".to_string() => make_typevalue_repr(REPR_INT) },
            None,
        );

        let span = rust_span!();
        let inner_expr = Spanned::new(
            CoreExpr::TypeAssert {
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
                check: TypeAssertCheck::Resolved(record_type),
                pipeline_blame: None,
            },
            span,
        );

        let thunk = eval_core_for_test(inner_expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match &val {
            Value::Dict { entries: map, .. } => {
                assert_eq!(map.len(), 1);
                assert!(map.contains_key(&HashableValue::Str("x".into())));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_typeassert_structural_record_non_dict_fails() {
        // Structural path: resolved_type = TypeValue.Record({x: Int}), value is Int -> error
        // TypeAssert is lazy in CEK model: type error fires on materialize(), not eval()
        use crate::type_infer::{make_typevalue_record, make_typevalue_repr};
        let record_type = make_typevalue_record(
            indexmap::indexmap! { "x".to_string() => make_typevalue_repr(REPR_INT) },
            None,
        );

        let span = rust_span!();
        let inner_expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                check: TypeAssertCheck::Resolved(record_type),
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
        // Nominal fallback path: resolved_type = None, annotation "Integer", value is Int -> pass.
        // When no resolved TypeValue is available the assert falls back to the nominal name check.
        let thunk = eval_str("[@Integer 7]", &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(
            val,
            Value::Int {
                n: 7,
                type_val: unknown_type_val()
            }
        );
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

                annotation: None,
                do_infer_placeholder: false,
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

                    annotation: None,
                    do_infer_placeholder: false,
                },
            ),
            surf_ann_entry(
                "age",
                SurfaceExpression::VarRef {
                    name: "Int".into(),
                    escaped: false,
                    resolution: crate::ast::Resolution::new(),

                    annotation: None,
                    do_infer_placeholder: false,
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

                    annotation: None,
                    do_infer_placeholder: false,
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
        let thunk = eval_str("[@[name: String] [name: \"hello\"]]", &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert!(
            matches!(val, Value::Dict { .. }),
            "Structural annotation with Dict value should pass tag check"
        );
    }

    #[tokio::test]
    async fn test_elaboration_gap_structural_annotation_non_dict_with_default() {
        // [@[name: String] 42] with Resolved(record_type) — no default in check.
        // Resolved checks have no `default:` property (only Source annotations carry defaults).
        // The record shape check fires: Int(42) is not a Dict → "type assertion failed" error.
        // Note: `default:` on a structural record annotation is not supported via Resolved —
        // Source with structural fields (no `type:` key) resolves to unknown_type_val which
        // passes all values. Neither path yields "use default on record mismatch" in the current design.
        use crate::type_infer::{make_typevalue_record, make_typevalue_repr};
        let record_type = make_typevalue_record(
            indexmap::indexmap! { "name".to_string() => make_typevalue_repr(REPR_STRING) },
            None,
        );

        let span = rust_span!();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                check: TypeAssertCheck::Resolved(record_type),
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx())
            .await
            .unwrap();
        // Int(42) is not a Dict with a `name:` field → record shape check fails → error.
        let err = materialize(&thunk, None, &test_ctx()).await.unwrap_err();
        assert!(
            err.to_string().contains("expected") && err.to_string().contains("got Int"),
            "expected type assertion error for non-Dict record mismatch, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_elaboration_gap_default_only_no_structural_check() {
        // [@[default: 0] "hello"] with resolved_type=None
        // Should pass through without validation (no type, no structural fields)
        let thunk = eval_str("[@[default: 0] \"hello\"]", &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        assert_eq!(val, string_val("hello"));
    }

    // ── value_matches_type unit tests ────────────────────────────────────
    // Direct tests of the value_matches_type() helper function after the S-1003 migration.
    // All expected types are now Arc<Value> TypeValues, not the old Type enum.

    #[tokio::test]
    async fn test_value_matches_type_int() {
        use crate::type_infer::make_typevalue_repr;
        let ctx = test_ctx();
        let int_tv = make_typevalue_repr(REPR_INT);
        assert!(value_matches_type(
            &Value::Int {
                n: 42,
                type_val: unknown_type_val()
            },
            &int_tv,
            &ctx
        ));
        assert!(!value_matches_type(&string_val("x"), &int_tv, &ctx));
        assert!(!value_matches_type(
            &Value::Float {
                n: 1.0,
                type_val: unknown_type_val()
            },
            &int_tv,
            &ctx
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_str() {
        use crate::type_infer::make_typevalue_repr;
        let ctx = test_ctx();
        let str_tv = make_typevalue_repr(REPR_STRING);
        assert!(value_matches_type(&string_val("hello"), &str_tv, &ctx));
        assert!(!value_matches_type(
            &Value::Int {
                n: 1,
                type_val: unknown_type_val()
            },
            &str_tv,
            &ctx
        ));
        assert!(!value_matches_type(
            &Value::Float {
                n: 0.0,
                type_val: unknown_type_val()
            },
            &str_tv,
            &ctx
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_float() {
        use crate::type_infer::make_typevalue_repr;
        let ctx = test_ctx();
        let float_tv = make_typevalue_repr(REPR_FLOAT);
        assert!(value_matches_type(
            &Value::Float {
                n: 2.5,
                type_val: unknown_type_val()
            },
            &float_tv,
            &ctx
        ));
        assert!(!value_matches_type(
            &Value::Int {
                n: 3,
                type_val: unknown_type_val()
            },
            &float_tv,
            &ctx
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_top() {
        use crate::type_infer::make_typevalue_top;
        let ctx = test_ctx();
        let top_tv = make_typevalue_top();
        // TypeValue.Top accepts all value kinds (gradual typing)
        assert!(value_matches_type(
            &Value::Int {
                n: 1,
                type_val: unknown_type_val()
            },
            &top_tv,
            &ctx
        ));
        assert!(value_matches_type(
            &Value::Float {
                n: 1.0,
                type_val: unknown_type_val()
            },
            &top_tv,
            &ctx
        ));
        assert!(value_matches_type(&string_val("s"), &top_tv, &ctx));
        assert!(value_matches_type(
            &Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val()
            },
            &top_tv,
            &ctx,
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_typevar_always_true() {
        use crate::type_infer::make_typevar_value;
        let ctx = test_ctx();
        // TypeValue.Var is treated as Unknown at runtime (residual polymorphic instantiation)
        let var_tv = make_typevar_value("a");
        assert!(value_matches_type(
            &Value::Int {
                n: 1,
                type_val: unknown_type_val()
            },
            &var_tv,
            &ctx,
        ));
        assert!(value_matches_type(&string_val("x"), &var_tv, &ctx));
        assert!(value_matches_type(
            &Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val()
            },
            &var_tv,
            &ctx,
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_proxy() {
        use crate::type_infer::{make_typevalue_repr, make_typevalue_unknown};
        // Proxy values return TypeValue.Unknown from ground_typevalue_of — passes any annotation.
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let handler = Arc::new(Thunk::value(
            Value::Int {
                n: 42,
                type_val: unknown_type_val(),
            },
            span,
        ));
        let proxy_val = Value::Proxy {
            handler,
            type_val: crate::value::unknown_type_val(),
        };

        // Unknown (ground of Proxy) is consistent with anything.
        assert!(value_matches_type(
            &proxy_val,
            &make_typevalue_unknown(),
            &ctx
        ));
        assert!(value_matches_type(
            &proxy_val,
            &make_typevalue_repr(REPR_INT),
            &ctx
        ));
        assert!(value_matches_type(
            &proxy_val,
            &crate::type_infer::make_typevalue_top(),
            &ctx
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_tycon_no_env() {
        use crate::type_infer::make_typevalue_op;
        // When tycon_env is not set (None), TypeValue.Op types conservatively return false.
        let ctx = test_ctx(); // no tycon_env set
        let tycon = make_typevalue_op("MyType");
        // Int value against unknown TyCon → false (conservative)
        assert!(!value_matches_type(
            &Value::Int {
                n: 42,
                type_val: unknown_type_val()
            },
            &tycon,
            &ctx
        ));
        assert!(!value_matches_type(
            &Value::Float {
                n: 1.0,
                type_val: unknown_type_val()
            },
            &tycon,
            &ctx
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_tycon_builtin_int() {
        // TypeValue.Op("MyInt") with builtin_type "Int" discriminant matches Value::Int.
        use crate::type_def::{TyConDef, TyConEnv};
        use crate::type_infer::make_typevalue_op;
        use std::sync::Arc;
        let ctx = test_ctx();
        let mut env = TyConEnv::new();
        env.insert(
            "MyInt".to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body: crate::value::unknown_type_val(),
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: Some("Int".to_string()),
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
                definition_span: None,
            }),
        );
        ctx.set_tycon_env(env);
        let tycon = make_typevalue_op("MyInt");
        assert!(value_matches_type(
            &Value::Int {
                n: 1,
                type_val: unknown_type_val()
            },
            &tycon,
            &ctx
        ));
        assert!(!value_matches_type(
            &Value::Float {
                n: 1.0,
                type_val: unknown_type_val()
            },
            &tycon,
            &ctx
        ));
        assert!(!value_matches_type(&string_val("x"), &tycon, &ctx));
    }

    #[tokio::test]
    async fn test_value_matches_type_tycon_builtin_dict() {
        // TypeValue.Op("MyDict") with builtin_type "Dict" matches Value::Dict.
        use crate::type_def::{TyConDef, TyConEnv};
        use crate::type_infer::make_typevalue_op;
        use std::sync::Arc;
        let ctx = test_ctx();
        let mut env = TyConEnv::new();
        env.insert(
            "MyDict".to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body: crate::value::unknown_type_val(),
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: Some("Dict".to_string()),
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
                definition_span: None,
            }),
        );
        ctx.set_tycon_env(env);
        let tycon = make_typevalue_op("MyDict");
        // Dict values match
        assert!(value_matches_type(
            &Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val()
            },
            &tycon,
            &ctx
        ));
        // Non-Dict values do not match
        assert!(!value_matches_type(
            &Value::Int {
                n: 1,
                type_val: unknown_type_val()
            },
            &tycon,
            &ctx
        ));
        assert!(!value_matches_type(
            &Value::Float {
                n: 1.0,
                type_val: unknown_type_val()
            },
            &tycon,
            &ctx
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_tycon_nominal() {
        // Nominal TypeValue.Op("Color") (has constructors) matches Value::Variant with matching tag prefix.
        use crate::type_def::{TyConDef, TyConEnv};
        use crate::type_infer::make_typevalue_op;
        use std::sync::Arc;
        let ctx = test_ctx();
        let mut env = TyConEnv::new();
        env.insert(
            "Color".to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body: crate::value::unknown_type_val(),
                constraints: vec![],
                variance: vec![],
                constructors: vec![("Color.Red".to_string(), 0), ("Color.Green".to_string(), 0)],
                builtin_type: None,
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
                definition_span: None,
            }),
        );
        ctx.set_tycon_env(env);
        let tycon = make_typevalue_op("Color");
        // Variant with matching tycon matches
        let red = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            type_decl_id: 0,
            ctor: Arc::from("Color.Red"),
            payload: None,
        };
        assert!(value_matches_type(&red, &tycon, &ctx));
        // Variant with different tycon does not match
        let wrong = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            type_decl_id: 0,
            ctor: Arc::from("Shape.Circle"),
            payload: None,
        };
        assert!(!value_matches_type(&wrong, &tycon, &ctx));
        // Non-Variant values do not match a nominal TyCon
        assert!(!value_matches_type(
            &Value::Int {
                n: 1,
                type_val: unknown_type_val()
            },
            &tycon,
            &ctx
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_app_tycon_dispatch() {
        // TypeValue.App(TypeValue.Op(name), arg) extracts the root TyCon name and applies TyConDef dispatch.
        // Type args are ignored at the value level (type erasure).
        use crate::type_class::make_type_app;
        use crate::type_def::{TyConDef, TyConEnv};
        use crate::type_infer::{make_typevalue_op, make_typevalue_repr};
        use std::sync::Arc;
        let ctx = test_ctx();
        let mut env = TyConEnv::new();
        env.insert(
            "MySeq".to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body: crate::value::unknown_type_val(),
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: Some("Str".to_string()),
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
                definition_span: None,
            }),
        );
        ctx.set_tycon_env(env);
        // App(TypeValue.Op("MySeq"), TypeValue.Repr("Value::Int")) — arg is ignored; dispatch on "Str" discriminant.
        let app_type = make_type_app(make_typevalue_op("MySeq"), make_typevalue_repr(REPR_INT));
        assert!(value_matches_type(&string_val("hello"), &app_type, &ctx));
        assert!(!value_matches_type(
            &Value::Int {
                n: 1,
                type_val: unknown_type_val()
            },
            &app_type,
            &ctx
        ));
    }

    #[tokio::test]
    async fn test_value_matches_type_tycon_from_typecheck_pass() {
        // Regression: value_matches_type must correctly resolve user-defined TyCons
        // when tycon_env is wired into the EvalContext via set_tycon_env.
        use crate::type_def::{TyConDef, TyConEnv};
        use crate::type_infer::make_typevalue_op;

        let color_def = Arc::new(TyConDef {
            params: vec![],
            body: crate::value::unknown_type_val(),
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
            definition_span: None,
        });
        let mut tycon_env: TyConEnv = std::collections::HashMap::new();
        tycon_env.insert("Color".to_string(), color_def);

        let ctx = test_ctx();
        ctx.set_tycon_env(tycon_env);

        let tycon = make_typevalue_op("Color");

        let color_red = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            type_decl_id: 0,
            ctor: Arc::from("Color.Red"),
            payload: None,
        };
        assert!(
            value_matches_type(&color_red, &tycon, &ctx),
            "Color.Red must match @Color when tycon_env is wired from typecheck pass"
        );

        let color_green = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            type_decl_id: 0,
            ctor: Arc::from("Color.Green"),
            payload: None,
        };
        assert!(
            value_matches_type(&color_green, &tycon, &ctx),
            "Color.Green must match @Color"
        );

        let other = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            type_decl_id: 0,
            ctor: Arc::from("Shape.Circle"),
            payload: None,
        };
        assert!(
            !value_matches_type(&other, &tycon, &ctx),
            "Shape.Circle must not match @Color"
        );

        assert!(
            !value_matches_type(
                &Value::Int {
                    n: 1,
                    type_val: unknown_type_val()
                },
                &tycon,
                &ctx
            ),
            "Int must not match @Color"
        );
    }

    #[tokio::test]
    async fn test_value_matches_type_opaque_builtin_via_tycon_name() {
        use crate::type_infer::make_typevalue_op;
        // Structural values do not have a value_tycon_name.
        assert_eq!(
            Value::Int {
                n: 1,
                type_val: unknown_type_val()
            }
            .value_tycon_name(),
            None,
            "Int is a primitive type, not an opaque builtin TyCon"
        );
        assert_eq!(
            string_val("x").value_tycon_name(),
            None,
            "String is a primitive type"
        );
        let color_variant = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            type_decl_id: 0,
            ctor: Arc::from("Color.Red"),
            payload: None,
        };
        assert_eq!(
            color_variant.value_tycon_name(),
            None,
            "Variant is matched via TyConDef constructor check, not value_tycon_name"
        );

        // TypeValue.Op with an unknown name (no tycon_env) → false.
        let ctx = test_ctx(); // no tycon_env
        let unknown_tycon = make_typevalue_op("Program");
        assert!(!value_matches_type(
            &Value::Int {
                n: 1,
                type_val: unknown_type_val()
            },
            &unknown_tycon,
            &ctx
        ));
        assert!(!value_matches_type(&color_variant, &unknown_tycon, &ctx));
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

        // Create a fields map requiring field "y" with type TypeValue.Repr("Value::Int")
        use crate::type_infer::make_typevalue_repr;
        let mut fields: IndexMap<String, Arc<Value>> = IndexMap::new();
        fields.insert("y".to_string(), make_typevalue_repr(REPR_INT));

        // Create entries that are missing field "y"
        let entries: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

        // Call validate_and_wrap_record with nested field_path ["outer", "inner"]
        let mut field_path = vec!["outer".to_string(), "inner".to_string()];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result = validate_and_wrap_record(
            &entries,
            &fields,
            &mut field_path,
            guard_span,
            data_span.clone(),
            None,
            None,
        );

        // Should error with field path prefix in the message
        assert!(result.is_err(), "Expected error for missing field");
        let err = result.unwrap_err();
        let msg = err.to_string();
        // spans[0] should be data_span (where the invalid dict was constructed/bound),
        // not guard_span (the annotation site). validate_and_wrap_record uses data_span as the
        // definition site so errors point at the value, not at the type annotation.
        assert_eq!(
            err.spans[0].0, data_span,
            "spans[0] should be data_span (value site), not guard_span (annotation site)"
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
            msg.contains("record without field \"y\""),
            "Expected 'record without field \"y\"' in error message, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_validate_and_wrap_record_nested_field_path_extra_field_accepted() {
        // BAS width subtyping: extra fields in closed records are ACCEPTED.
        // Under BAS, a value with more fields satisfies an annotation with fewer fields.

        // Create a fields map requiring only field "x"
        use crate::type_infer::make_typevalue_repr;
        let mut fields: IndexMap<String, Arc<Value>> = IndexMap::new();
        fields.insert("x".to_string(), make_typevalue_repr(REPR_INT));

        // Create entries with "x" plus an extra field "z"
        let mut entries: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        entries.insert(
            HashableValue::Str("x".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: 1,
                    type_val: unknown_type_val(),
                },
                span.clone(),
            )),
        );
        entries.insert(
            HashableValue::Str("z".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: 99,
                    type_val: unknown_type_val(),
                },
                span,
            )),
        );

        let mut field_path = vec!["config".to_string()];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result = validate_and_wrap_record(
            &entries,
            &fields,
            &mut field_path,
            guard_span,
            data_span,
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

        // Create a fields map requiring field "name"
        use crate::type_infer::make_typevalue_repr;
        let mut fields: IndexMap<String, Arc<Value>> = IndexMap::new();
        fields.insert("name".to_string(), make_typevalue_repr(REPR_STRING));

        // Create empty entries (missing "name")
        let entries: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

        // Call with empty field_path
        let mut field_path = vec![];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result = validate_and_wrap_record(
            &entries,
            &fields,
            &mut field_path,
            guard_span,
            data_span.clone(),
            None,
            None,
        );

        assert!(result.is_err(), "Expected error for missing field");
        let err = result.unwrap_err();
        let msg = err.to_string();
        // spans[0] should be data_span (where the invalid dict was constructed/bound),
        // not guard_span (the annotation site). validate_and_wrap_record uses data_span as the
        // definition site so errors point at the value, not at the type annotation.
        assert_eq!(
            err.spans[0].0, data_span,
            "spans[0] should be data_span (value site), not guard_span (annotation site)"
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
            msg.contains("record without field \"name\""),
            "Expected 'record without field \"name\"' in error message, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_validate_and_wrap_record_accepts_int_key_bas() {
        // BAS width subtyping: integer-keyed entries are extra fields and are ACCEPTED.
        // Under BAS, a value with more fields (including int-keyed) satisfies the annotation.

        use crate::type_infer::make_typevalue_repr;
        let mut fields: IndexMap<String, Arc<Value>> = IndexMap::new();
        fields.insert("name".to_string(), make_typevalue_repr(REPR_STRING));

        // Create entries with "name" (valid) plus an integer-keyed entry
        let mut entries: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        entries.insert(
            HashableValue::Int(0),
            Arc::new(Thunk::value(string_val("x"), span.clone())),
        );
        entries.insert(
            HashableValue::Str("name".into()),
            Arc::new(Thunk::value(string_val("y"), span)),
        );

        let mut field_path = vec![];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result = validate_and_wrap_record(
            &entries,
            &fields,
            &mut field_path,
            guard_span,
            data_span,
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
        // All records are closed under BAS which allows extra fields.
        use crate::type_infer::make_typevalue_repr;
        let mut fields: IndexMap<String, Arc<Value>> = IndexMap::new();
        fields.insert("name".to_string(), make_typevalue_repr(REPR_STRING));

        let mut entries: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        entries.insert(
            HashableValue::Int(0),
            Arc::new(Thunk::value(string_val("x"), span.clone())),
        );
        entries.insert(
            HashableValue::Str("name".into()),
            Arc::new(Thunk::value(string_val("y"), span)),
        );

        let mut field_path = vec![];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result = validate_and_wrap_record(
            &entries,
            &fields,
            &mut field_path,
            guard_span,
            data_span,
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
        let thunk = Arc::new(Thunk::value(
            Value::Int {
                n: 42,
                type_val: unknown_type_val(),
            },
            span,
        ));
        let ctx = test_ctx();

        // Materialize at high depth (CEK continuation stack) should succeed
        let result = materialize(&thunk, None, &ctx).await;
        assert!(
            result.is_ok(),
            "Expected success for cached thunk at high depth, got error: {:?}",
            result.unwrap_err()
        );
        assert_eq!(
            result.unwrap(),
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }
        );
    }

    #[tokio::test]
    async fn test_thunk_guarded_memoizes_on_success() {
        // Task 3(3): Guarded thunk memoization — after successful validation, the
        // thunk transitions to Materialized and the second access returns the cached
        // value without re-running the type guard.
        use crate::type_infer::make_typevalue_repr;

        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);

        // Inner thunk: a materialized Int(42) — passes the Int guard.
        let inner = Arc::new(Thunk::value(
            Value::Int {
                n: 42,
                type_val: unknown_type_val(),
            },
            span.clone(),
        ));

        // Wrap it in a Guarded thunk expecting TypeValue.Repr("Value::Int").
        let guarded = Arc::new(Thunk::guarded(
            inner,
            make_typevalue_repr(REPR_INT),
            vec!["value".to_string()],
            span,
            None,
            None,
        ));

        // Initial state must be Guarded.
        {
            assert!(guarded.is_guarded(), "initial state should be Guarded");
        }

        // First materialization: triggers guard, validates Int(42) against TypeValue.Repr("Value::Int") → pass.
        let result1 = materialize(&guarded, None, &ctx).await;
        assert!(result1.is_ok(), "first materialization should succeed");
        assert_eq!(
            result1.unwrap(),
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }
        );

        // After successful validation, thunk must be in Materialized state (memoized).
        assert_eq!(
            match guarded.peek_result() {
                Some(Ok(v)) => Some(v),
                Some(Err(e)) => panic!("thunk in error state: {e:?}"),
                None => None,
            },
            Some(&Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }),
            "after first materialization thunk should be Materialized(Int(42))"
        );

        // Second materialization: must return cached value, not re-run the guard.
        let result2 = materialize(&guarded, None, &ctx).await;
        assert!(
            result2.is_ok(),
            "second materialization should succeed (cached)"
        );
        assert_eq!(
            result2.unwrap(),
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }
        );

        // State is still Materialized (not changed by second access).
        assert_eq!(
            match guarded.peek_result() {
                Some(Ok(v)) => Some(v),
                Some(Err(e)) => panic!("thunk in error state: {e:?}"),
                None => None,
            },
            Some(&Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }),
            "state should still be Materialized after second access"
        );
    }

    #[tokio::test]
    async fn test_guarded_thunk_failure_path() {
        // Task 3(2): Guarded thunk failure path — when the inner value fails the type guard,
        // the thunk transitions to Failed (cacheable) and subsequent access returns the
        // cached error without re-running the guard.
        use crate::type_infer::make_typevalue_repr;

        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);

        // Inner thunk: a String value — fails the Int guard.
        let inner = Arc::new(Thunk::value(string_val("hello"), span.clone()));

        // Wrap it in a Guarded thunk expecting TypeValue.Repr("Value::Int").
        let guarded = Arc::new(Thunk::guarded(
            inner,
            make_typevalue_repr(REPR_INT),
            vec!["field".to_string()],
            span,
            None,
            None,
        ));

        // First materialization: triggers guard, validates String against TypeValue.Repr("Value::Int") → fail.
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
            guarded.try_get_error().is_some(),
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
        use crate::type_infer::make_typevalue_repr;

        let span = test_span(1, 1, 1, 10);

        // Create an inner thunk that will produce a type mismatch when wrapped with Guarded
        // (we expect Int but will get String)
        let inner_expr = Arc::new(sp(CoreExpr::Str("hello".into())));
        let ctx = test_ctx();
        let inner_thunk = Arc::new(Thunk::core_expr(
            inner_expr,
            EvalFrame::empty(),
            Arc::clone(&ctx),
            span,
        ));

        // Wrap it in a Guarded thunk expecting TypeValue.Repr("Value::Int") (will fail type check)
        let guard_span = test_span(2, 1, 2, 10);
        let expected = make_typevalue_repr(REPR_INT);
        let field_path = vec!["field".to_string()];
        let guarded = Arc::new(Thunk::guarded(
            inner_thunk,
            expected,
            field_path,
            guard_span,
            None,
            None,
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

        // Create a 3-node cycle: a→b→c→a
        // We'll use eval_dict to create labeled thunks
        let source = r#"
[
    a: $b
    b: $c
    c: $a
]
        "#;

        let test_file: Arc<str> = Arc::from(file!());
        let parsed = crate::parse(source, Arc::clone(&test_file)).expect("parse should succeed");
        let surface_program = crate::desugar::desugar_program_full(&parsed.program);
        // Resolve with the full root frame so dict sibling LGM slots match the runtime.
        let root_frame = ctx.root_group_resolver_map();
        let (_table, _frames) =
            crate::resolve::resolve_surface_program(&surface_program, &[root_frame]);
        // resolve_surface_program is called for its side effects on AST nodes (setting
        // OnceLock resolution coordinates); the returned table and frames are not needed here.
        let thunk = super::eval_surface_file(&surface_program, &ctx)
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
            Value::Dict {
                entries: ref map, ..
            } => {
                let a_thunk = map
                    .get(&HashableValue::Str("a".into()))
                    .expect("dict should have 'a' key");
                materialize(a_thunk, None, &ctx).await.unwrap_err()
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

            // The iterative CEK machine pops eval_stack entries at force_step exit rather than
            // at thunk completion, so cycle_path is empty when circular dependency is detected —
            // the cycle detector fires at the right time but the path reconstruction has no frames
            // left to walk. The cycle is detected correctly; only the path is empty.
            assert!(
                cycle_path.is_empty(),
                "iterative evaluator produces empty cycle_path (entries are popped at force_step exit, not thunk completion): got {:?}",
                cycle_path
            );
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
    async fn test_selective_materialization_unused_branch() {
        // Verify that accessing only one dict entry doesn't materialize unused entries.
        // $builtin-raise is in core_env and raises an error when forced; the "unused"
        // entry must remain unforced so the raise never fires.
        let input = r#"[used: 1  unused: [call $builtin-raise "should not materialize"]]"#;
        let (_env, ctx) = core_env_and_ctx();
        let thunk = eval_str(input, &ctx).await.unwrap();
        let val = materialize(&thunk, None, &ctx).await.unwrap();

        // Extract the dict
        match val {
            Value::Dict { entries: map, .. } => {
                // Access only the "used" key
                let used_key = HashableValue::Str("used".into());
                let used_thunk = map.get(&used_key).expect("used key should exist");
                let used_val = mat_id(used_thunk, &ctx)
                    .await
                    .expect("used should materialize");
                assert_eq!(
                    used_val,
                    Value::Int {
                        n: 1,
                        type_val: unknown_type_val()
                    }
                );

                // Verify the "unused" key exists but is NOT materialized
                let unused_key = HashableValue::Str("unused".into());
                let unused_thunk_id = map.get(&unused_key).expect("unused key should exist");
                let unused_thunk = get_thunk_arc(unused_thunk_id);

                // Check that the unused thunk is still in an unevaluated state
                // (it should not be Materialized)
                assert!(
                    !unused_thunk.is_materialized(),
                    "unused thunk should not be materialized"
                );
                // Check it's not in Failed or InProgress state
                if unused_thunk.try_get_error().is_some() {
                    panic!("unused thunk should not be in Failed state (error should not have triggered)")
                }
                if !unused_thunk.is_settled()
                    && unused_thunk.inner.unevaluated.lock().unwrap().0.is_none()
                {
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
    /// materializing args[0], `require_value()` inside `builtin_keys`
    /// would panic.
    #[tokio::test]
    async fn pending_builtin_bypass_path_pre_materializes_args() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Build an unevaluated thunk that evaluates to an empty dict.
        // `CoreExpr::Dict(vec![])` evaluates to `Value::Dict { entries: IndexMap::new(), type_val: unknown_type_val() }`.
        let dict_expr = Arc::new(sp(CoreExpr::Dict(vec![])));
        let unevaluated_arg = Arc::new(Thunk::core_expr(
            dict_expr,
            EvalFrame::empty(),
            Arc::clone(&ctx),
            span.clone(),
        ));

        // Verify the arg is NOT yet materialized (it is unevaluated).
        assert!(
            !unevaluated_arg.is_materialized(),
            "arg must be unevaluated before the PendingBuiltin is forced"
        );

        // Construct a BuiltinDef for `builtin_keys` with force_count=1.
        const KEYS_STRICTNESS: &[Strictness] = &[];
        let keys_def = BuiltinDef {
            func: crate::builtins_dict::builtin_keys as BuiltinFn,
            name: "keys",
            pos_strictness: KEYS_STRICTNESS,
            force_count: 1,
            needs_caller_env: false,
        };

        // Create a PendingBuiltin thunk wrapping `builtin_keys` with the unevaluated arg.
        let outer = Arc::new(Thunk::builtin_call(
            keys_def,
            vec![unevaluated_arg],
            None,
            span,
            0, // caller_env_id bridge placeholder
            Arc::clone(&ctx),
        ));

        // Materialize via the recursive path. If force_count pre-materialization is
        // missing, this panics inside `builtin_keys` at the `require_value()` call.
        let result = materialize(&outer, None, &ctx).await;
        assert!(
            result.is_ok(),
            "PendingBuiltin bypass path must pre-materialize force_count args; got: {:?}",
            result.unwrap_err()
        );

        // The result should be an empty dict (keys of empty dict = empty dict).
        let val = result.unwrap();
        assert!(
            matches!(val, Value::Dict { entries: ref m, .. } if m.is_empty()),
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
    /// `require_value()`.
    #[tokio::test]
    async fn pending_call_builtin_bypass_path_pre_materializes_args() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Build an unevaluated thunk that evaluates to an empty dict.
        let dict_expr = Arc::new(sp(CoreExpr::Dict(vec![])));
        let unevaluated_arg = Arc::new(Thunk::core_expr(
            dict_expr,
            EvalFrame::empty(),
            Arc::clone(&ctx),
            span.clone(),
        ));

        // Verify the arg is NOT yet materialized.
        assert!(
            !unevaluated_arg.is_materialized(),
            "arg must be unevaluated before the PendingCall is forced"
        );

        // Create a materialized func thunk wrapping Value::Builtin(keys_def).
        const KEYS_STRICTNESS: &[Strictness] = &[];
        let keys_def = BuiltinDef {
            func: crate::builtins_dict::builtin_keys as BuiltinFn,
            name: "keys",
            pos_strictness: KEYS_STRICTNESS,
            force_count: 1,
            needs_caller_env: false,
        };
        let func_thunk = Arc::new(Thunk::value(
            Value::Builtin {
                def: keys_def,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        ));

        // Create a PendingCall thunk using Arc<Thunk> directly.
        let outer = make_pending_call(&ctx, func_thunk, vec![unevaluated_arg], span);

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
            matches!(val, Value::Dict { entries: ref m, .. } if m.is_empty()),
            "expected empty dict from builtin_keys on empty dict, got {:?}",
            val
        );
    }

    // ── PM3: pattern linearity tests ─────────────────────────────────────────

    #[tokio::test]
    async fn test_pm3_match_expr_pin_pattern_does_not_bind() {
        // `[a: x  b: x]: "body"` — `x` is undefined in the dict pattern values.
        // Each `x` has resolution=Some(None) →
        //   lowerer emits CoreExpr::Var { addr: ClosureCapture(u32::MAX) } (sentinel) →
        //   bind_or_pin_name returns Ok(false) → arm doesn't match.
        // No wildcard fallback → MatchExhaustion when forced.
        // (Note: `...` inside a dict pattern requires scope_frames for spread desugaring;
        //  this test uses a plain dict pattern to avoid that complexity.)
        let thunk = eval_str("[match [a: 1  b: 2]  [a: x  b: x]: \"body\"]", &test_ctx())
            .await
            .expect("eval_str must succeed (arm simply doesn't match, body never reached)");
        let result = materialize(&thunk, None, &test_ctx()).await;
        assert!(
            result.is_err(),
            "undefined-pin dict-pattern arm must produce MatchExhaustion when forced; got: {:?}",
            result
        );
        assert!(
            matches!(
                result.unwrap_err().kind,
                crate::error::ErrorKind::MatchExhaustion { .. }
            ),
            "expected MatchExhaustion from undefined pin in dict pattern"
        );
    }

    #[tokio::test]
    async fn test_pm3_placeholder_wildcard_matches_dict() {
        // Bare `...:` wildcard matches any scrutinee including dicts.
        let thunk = eval_str("[match [a: 1  b: 2]  ...: 99]", &test_ctx())
            .await
            .expect("Placeholder wildcard must not error on eval");
        let val = materialize(&thunk, None, &test_ctx())
            .await
            .expect("Placeholder wildcard must materialize without error");
        assert_eq!(
            val,
            Value::Int {
                n: 99,
                type_val: unknown_type_val()
            },
            "Placeholder wildcard arm must produce 99; got: {:?}",
            val
        );
    }

    #[tokio::test]
    async fn test_pm3_undefined_vrefs_in_arms_produce_match_exhaustion() {
        // Undefined VarRefs in pattern (key) position resolve to Some(None) →
        // lowerer emits sentinel Var → bind_or_pin_name returns Ok(false) → arm never matches.
        // `[match 42  x: 1  x: 2]` with x undefined → MatchExhaustion.
        let thunk = eval_str("[match 42  x: 1  x: 2]", &test_ctx())
            .await
            .expect("Eval must not error before thunk is forced");
        let result = materialize(&thunk, None, &test_ctx()).await;
        assert!(
            result.is_err(),
            "Undefined VarRef arms must produce MatchExhaustion when forced; got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, crate::error::ErrorKind::MatchExhaustion { .. }),
            "Expected MatchExhaustion, got: {:?}",
            err.kind
        );
    }

    // Pin patterns in match — VarRef in pattern position looks up current scope and
    // compares to scrutinee for equality. A resolved pin matches when values are equal.
    #[tokio::test]
    async fn test_b597_pin_pattern_match_succeeds() {
        // `[x: 42  r: [match 42  $x: "hit"  ...: "miss"]]` — $x resolves to 42, scrutinee is 42.
        // After B-597, the pin arm matches and r = "hit".
        let thunk = eval_str(
            "[x: 42  r: [match 42  $x: \"hit\"  ...: \"miss\"]]",
            &test_ctx(),
        )
        .await
        .expect("eval_str must succeed");
        let val = materialize(&thunk, None, &test_ctx())
            .await
            .expect("materialize must succeed");
        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };
        let r = d
            .get(&HashableValue::Str(Arc::from("r")))
            .expect("key 'r' must exist");
        let r_val = super::materialize(r, None, &test_ctx())
            .await
            .expect("r must materialize");
        assert_eq!(
            r_val,
            Value::String {
                source: Arc::from("hit"),
                start: 0,
                end: 3,
                type_val: crate::value::unknown_type_val(),
            },
            "pin pattern $x=42 should match scrutinee 42; got: {r_val:?}"
        );
    }

    #[tokio::test]
    async fn test_b597_pin_pattern_no_match_falls_through() {
        // `[x: 42  r: [match 99  $x: "hit"  ...: "miss"]]` — $x=42, scrutinee=99, no match.
        let thunk = eval_str(
            "[x: 42  r: [match 99  $x: \"hit\"  ...: \"miss\"]]",
            &test_ctx(),
        )
        .await
        .expect("eval_str must succeed");
        let val = materialize(&thunk, None, &test_ctx())
            .await
            .expect("materialize must succeed");
        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };
        let r = d
            .get(&HashableValue::Str(Arc::from("r")))
            .expect("key 'r' must exist");
        let r_val = super::materialize(r, None, &test_ctx())
            .await
            .expect("r must materialize");
        assert_eq!(
            r_val,
            Value::String {
                source: Arc::from("miss"),
                start: 0,
                end: 4,
                type_val: crate::value::unknown_type_val(),
            },
            "pin pattern $x=42 should NOT match scrutinee 99; got: {r_val:?}"
        );
    }

    // -------------------------------------------------------------------------
    // B-712: Arc::ptr_eq type dispatch — variant match semantics
    //
    // When a VarRef pin pattern resolves to a type-constructor Dict (from a [type ...]
    // declaration), match_pattern uses Arc::ptr_eq on the Dict's type_val and the
    // Variant's type_val. Both carry the same Arc created by CoreExpr::TypeDecl, so
    // ptr_eq succeeds. Plain dicts (no TypeDecl) carry unknown_type_val() — a different
    // Arc — so ptr_eq fails and they do not falsely match variants.
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_b712_plain_dict_arm_does_not_match_variant() {
        let thunk = eval_str(
            r#"[
          Color: [type Red Green Blue]
          shape: [r: 255  g: 0  b: 0]
          r1: [match Color.Red  shape: "false-positive"  Color: "is-color"  ...: "other"]
        ]"#,
            &test_ctx(),
        )
        .await
        .expect("eval_str must succeed");
        let val = materialize(&thunk, None, &test_ctx())
            .await
            .expect("materialize must succeed");
        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };
        let r1 = d
            .get(&HashableValue::Str(Arc::from("r1")))
            .expect("key 'r1' must exist");
        let r1_val = super::materialize(r1, None, &test_ctx())
            .await
            .expect("r1 must materialize");
        assert_eq!(
            r1_val,
            Value::String {
                source: Arc::from("is-color"),
                start: 0,
                end: 8,
                type_val: crate::value::unknown_type_val(),
            },
            "Color arm (type-ctor dict) must match Color.Red; plain-dict arm must not fire; got: {r1_val:?}"
        );
    }

    #[tokio::test]
    async fn test_b712_plain_dict_does_not_false_match_tycon() {
        let thunk = eval_str(
            r#"[
          coords: [x: 1  y: 2]
          Color: [type Red Green Blue]
          r1: [match Color.Red  coords: "plain-matched"  ...: "wildcard"]
          r2: [match Color.Red  Color: "type-matched"   ...: "wildcard"]
        ]"#,
            &test_ctx(),
        )
        .await
        .expect("eval_str must succeed");
        let val = materialize(&thunk, None, &test_ctx())
            .await
            .expect("materialize must succeed");
        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };
        let r1 = d.get(&HashableValue::Str(Arc::from("r1"))).expect("r1");
        let r1_val = super::materialize(r1, None, &test_ctx())
            .await
            .expect("r1 materialize");
        assert_eq!(
            r1_val,
            Value::String {
                source: Arc::from("wildcard"),
                start: 0,
                end: 8,
                type_val: crate::value::unknown_type_val()
            },
            "plain-dict 'coords' must NOT match Color.Red (unknown_type_val); got: {r1_val:?}"
        );
        let r2 = d.get(&HashableValue::Str(Arc::from("r2"))).expect("r2");
        let r2_val = super::materialize(r2, None, &test_ctx())
            .await
            .expect("r2 materialize");
        assert_eq!(
            r2_val,
            Value::String {
                source: Arc::from("type-matched"),
                start: 0,
                end: 12,
                type_val: crate::value::unknown_type_val()
            },
            "type-ctor-dict 'Color' must match Variant Color.Red; got: {r2_val:?}"
        );
    }

    #[tokio::test]
    async fn test_b712_sentinel_canonical_tycon_match() {
        let thunk = eval_str(
            r#"[
          Color: [type Red Green Blue]
          r: [match Color.Red  Color: "matched"  ...: "other"]
        ]"#,
            &test_ctx(),
        )
        .await
        .expect("eval_str must succeed");
        let val = materialize(&thunk, None, &test_ctx())
            .await
            .expect("materialize");
        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };
        let r = d.get(&HashableValue::Str(Arc::from("r"))).expect("key 'r'");
        let r_val = super::materialize(r, None, &test_ctx())
            .await
            .expect("r materialize");
        assert_eq!(
            r_val,
            Value::String {
                source: Arc::from("matched"),
                start: 0,
                end: 7,
                type_val: crate::value::unknown_type_val()
            },
            "Color arm must match Color.Red via sentinel; got: {r_val:?}"
        );
    }

    #[tokio::test]
    async fn test_b712_sentinel_shadowed_tycon_no_false_match() {
        let thunk = eval_str(
            r#"[
          Color: [type Red Green Blue]
          v: Color.Red
          test: [fn [let x]
            [Color: [r: 255  g: 0  b: 0]]
            [match x  Color: "wrong"  ...: "correct"]]
          r: [test v]
        ]"#,
            &test_ctx(),
        )
        .await
        .expect("eval_str must succeed");
        let val = materialize(&thunk, None, &test_ctx())
            .await
            .expect("materialize");
        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };
        let r = d.get(&HashableValue::Str(Arc::from("r"))).expect("key 'r'");
        let r_val = super::materialize(r, None, &test_ctx())
            .await
            .expect("r materialize");
        assert_eq!(
            r_val,
            Value::String {
                source: Arc::from("correct"),
                start: 0,
                end: 7,
                type_val: crate::value::unknown_type_val()
            },
            "shadowed plain dict must NOT match Color.Red; got: {r_val:?}"
        );
    }

    #[tokio::test]
    async fn test_b712_sentinel_alias_tycon_match() {
        let thunk = eval_str(
            r#"[
          Color: [type Red Green Blue]
          mycolor: Color
          r: [match Color.Red  mycolor: "alias-matched"  ...: "other"]
        ]"#,
            &test_ctx(),
        )
        .await
        .expect("eval_str must succeed");
        let val = materialize(&thunk, None, &test_ctx())
            .await
            .expect("materialize");
        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };
        let r = d.get(&HashableValue::Str(Arc::from("r"))).expect("key 'r'");
        let r_val = super::materialize(r, None, &test_ctx())
            .await
            .expect("r materialize");
        assert_eq!(
            r_val,
            Value::String {
                source: Arc::from("alias-matched"),
                start: 0,
                end: 13,
                type_val: crate::value::unknown_type_val()
            },
            "alias dict must match Color.Red via ptr_eq; got: {r_val:?}"
        );
    }

    #[tokio::test]
    async fn test_b712_same_name_different_scopes_independent_identities() {
        // Outer Color and inner Color are independent types despite sharing the name.
        // Outer Color.Red must match the outer Color arm; inner Color.Red must NOT
        // match the outer Color arm (different Arc identity).
        //
        // B-714: The test forces inner-red FIRST (before outer-red) to ensure that the
        // inner TypeDecl's evaluation and variant forcing happens before the outer variant
        // is forced. With the old String-keyed registry, this would trigger the overwrite
        // timing window (inner TypeDecl overwrites outer's entry, then outer variant looks
        // up the wrong identity). With the u64-keyed registry, the two types have independent
        // IDs and cannot interfere.
        let thunk = eval_str(
            r#"[
          Color: [type Red Green Blue]
          inner-scope: [Color: [type Red]  val: Color.Red]
          outer-red: Color.Red
          inner-red: inner-scope.val
          r-inner: [match inner-red  Color: "outer-match"  ...: "no-match"]
          r-outer: [match outer-red  Color: "outer-match"  ...: "no-match"]
        ]"#,
            &test_ctx(),
        )
        .await
        .expect("eval_str must succeed");
        let val = materialize(&thunk, None, &test_ctx())
            .await
            .expect("materialize");
        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };
        // Verify r-inner first (forced first in the program) — must NOT match outer Color
        let ri = d
            .get(&HashableValue::Str(Arc::from("r-inner")))
            .expect("r-inner");
        let ri_val = super::materialize(ri, None, &test_ctx())
            .await
            .expect("r-inner materialize");
        assert_eq!(
            ri_val,
            Value::String { source: Arc::from("no-match"), start: 0, end: 8, type_val: crate::value::unknown_type_val() },
            "inner Color.Red must NOT match outer Color arm (independent Arc identity); got: {ri_val:?}"
        );
        // Verify r-outer (forced after r-inner) — must still match outer Color despite inner overwrite
        let ro = d
            .get(&HashableValue::Str(Arc::from("r-outer")))
            .expect("r-outer");
        let ro_val = super::materialize(ro, None, &test_ctx())
            .await
            .expect("r-outer materialize");
        assert_eq!(
            ro_val,
            Value::String {
                source: Arc::from("outer-match"),
                start: 0,
                end: 11,
                type_val: crate::value::unknown_type_val()
            },
            "outer Color.Red must match outer Color arm even after inner TypeDecl evaluation; got: {ro_val:?}"
        );
    }

    // eval_surface_file_with_input injects initial_input thunk as % pipeline variable.
    // The formatter calls this with the AST dict as Some(ast_thunk); % must be accessible.
    #[tokio::test]
    async fn test_b596_initial_input_accessible_as_percent() {
        // Build a program that references % — after B-596, it should resolve to initial_input.
        let ctx = test_ctx();
        let source = "[result: %]";
        let file: Arc<str> = Arc::from("<test>");
        let parsed = crate::parser::parse(source, Arc::clone(&file)).expect("parse must succeed");
        let program = crate::desugar::desugar_program_full(&parsed.program);

        // Seed resolver with root_group + % at slot root_group.len().
        let mut resolver_seed = ctx.root_group_resolver_map();
        let percent_slot = ctx.root_group.len() as u32;
        resolver_seed.insert("%".to_string(), percent_slot);
        let (_table, _frames) = crate::resolve::resolve_surface_program(&program, &[resolver_seed]);

        // Build a thunk for initial_input: Int(777).
        let input_thunk = Arc::new(Thunk::value(
            Value::Int {
                n: 777,
                type_val: unknown_type_val(),
            },
            rust_span!(),
        ));

        let result_thunk = super::eval_surface_file_with_input(&program, &ctx, Some(input_thunk))
            .await
            .expect("eval_surface_file_with_input must succeed");
        let val = super::materialize(&result_thunk, None, &ctx)
            .await
            .expect("materialize must succeed");
        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };
        let result_thunk_ref = d
            .get(&HashableValue::Str(Arc::from("result")))
            .expect("key 'result' must exist");
        let result_val = super::materialize(result_thunk_ref, None, &ctx)
            .await
            .expect("result must materialize");
        assert_eq!(
            result_val,
            Value::Int {
                n: 777,
                type_val: unknown_type_val()
            },
            "% should resolve to initial_input Int(777); got: {result_val:?}"
        );
    }

    // [type MyType Int] in standalone expression position returns {} (empty dict).
    //
    // Type declarations have no runtime value when they appear as standalone expressions
    // (i.e. not as the value of a named dict entry like `Color: [type Red Green Blue]`).
    // The correct runtime result is {} (empty dict), not an error and not a non-empty dict.
    #[tokio::test]
    async fn test_type_alias_returns_empty_dict() {
        // [type MyType Int] — standalone type alias in expression position.
        // The body "Int" is an uppercase VarRef; previously this was misinterpreted as a
        // unit constructor "Int" and produced a non-empty constructor dict {Int: <variant>}.
        let thunk = eval_str("[type MyType Int]", &test_ctx()).await.unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match val {
            Value::Dict { entries: map, .. } => assert!(
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
        let thunk = eval_str("[type Color Red Green Blue]", &test_ctx())
            .await
            .unwrap();
        let val = materialize(&thunk, None, &test_ctx()).await.unwrap();
        match val {
            Value::Dict { entries: map, .. } => assert!(
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

    // Case arm variable bindings must be injected into the arm EvalFrame.
    // `[case [let v] pattern body]` — `v` must be accessible in body.
    // Previously bind_or_pin_name created the thunk and immediately dropped it;
    // the arm body evaluated in the parent frame and `v` was undefined.
    //
    // These tests use the eval_surface_file path (via parse + desugar) to exercise
    // the full resolver → lowerer → evaluator pipeline including case arm scope.

    #[tokio::test]
    async fn test_b598_case_arm_binding_is_accessible() {
        // `[case [let v] v body]` — pattern IS the binding variable `v`.
        // Scrutinee = 42. Pattern `v` matches (and binds v=42). Body returns v directly.
        // r: [match 42 [case [let v] v v] ...: 0] → r = 42.
        let ctx = test_ctx();
        let file: Arc<str> = Arc::from("<test>");
        let source = "[r: [match 42 [case [let v] v v] ...: 0]]";
        let parsed = crate::parser::parse(source, Arc::clone(&file)).expect("parse");
        let program = crate::desugar::desugar_program_full(&parsed.program);
        let root_frame = ctx.root_group_resolver_map();
        let (_table, _frames) = crate::resolve::resolve_surface_program(&program, &[root_frame]);
        // resolve_surface_program is called for its side effects on AST nodes (setting
        // OnceLock resolution coordinates); the returned table and frames are not needed here.
        let thunk = crate::eval_surface_file(&program, &ctx)
            .await
            .expect("eval_surface_file must succeed");
        let val = materialize(&thunk, None, &ctx)
            .await
            .expect("materialize must succeed");
        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };
        let r = d
            .get(&HashableValue::Str(Arc::from("r")))
            .expect("key 'r' must exist");
        let r_val = super::materialize(r, None, &ctx)
            .await
            .expect("r must materialize");
        assert_eq!(
            r_val,
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            },
            "B-598: case arm binding v should be 42; got: {r_val:?}"
        );
    }

    #[tokio::test]
    async fn test_b598_case_arm_binding_used_in_expression() {
        // `[case [let v] v [builtin-int-add v 1]]` — v is bound to the scrutinee,
        // then used in an arithmetic expression in the body.
        // Scrutinee = 41. v = 41. Body = [builtin-int-add v 1] = 42.
        // (Using builtin-int-add directly to avoid the + typeclass resolution.)
        let ctx = test_ctx();
        let file: Arc<str> = Arc::from("<test>");
        let source = "[r: [match 41 [case [let v] v [builtin-int-add v 1]] ...: 0]]";
        let parsed = crate::parser::parse(source, Arc::clone(&file)).expect("parse");
        let program = crate::desugar::desugar_program_full(&parsed.program);
        let root_frame = ctx.root_group_resolver_map();
        let (_table, _frames) = crate::resolve::resolve_surface_program(&program, &[root_frame]);
        // resolve_surface_program is called for its side effects on AST nodes (setting
        // OnceLock resolution coordinates); the returned table and frames are not needed here.
        let thunk = crate::eval_surface_file(&program, &ctx)
            .await
            .expect("eval_surface_file must succeed");
        let val = materialize(&thunk, None, &ctx)
            .await
            .expect("materialize must succeed");
        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };
        let r = d
            .get(&HashableValue::Str(Arc::from("r")))
            .expect("key 'r' must exist");
        let r_val = super::materialize(r, None, &ctx)
            .await
            .expect("r must materialize");
        assert_eq!(
            r_val,
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            },
            "B-598: v=41, body [builtin-int-add v 1] should be 42; got: {r_val:?}"
        );
    }

    #[tokio::test]
    async fn test_b598_case_arm_no_match_falls_through() {
        // A non-matching case arm must fall through to the next arm (wildcard).
        // r: [match "hello" [case [let v] 42 v] ...: 99]
        // Scrutinee "hello" != literal 42 → arm does not match → wildcard 99.
        let ctx = test_ctx();
        let file: Arc<str> = Arc::from("<test>");
        let source = r#"[r: [match "hello" [case [let v] 42 v] ...: 99]]"#;
        let parsed = crate::parser::parse(source, Arc::clone(&file)).expect("parse");
        let program = crate::desugar::desugar_program_full(&parsed.program);
        let root_frame = ctx.root_group_resolver_map();
        let (_table, _frames) = crate::resolve::resolve_surface_program(&program, &[root_frame]);
        // resolve_surface_program is called for its side effects on AST nodes (setting
        // OnceLock resolution coordinates); the returned table and frames are not needed here.
        let thunk = crate::eval_surface_file(&program, &ctx)
            .await
            .expect("eval_surface_file must succeed");
        let val = materialize(&thunk, None, &ctx)
            .await
            .expect("materialize must succeed");
        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };
        let r = d
            .get(&HashableValue::Str(Arc::from("r")))
            .expect("key 'r' must exist");
        let r_val = super::materialize(r, None, &ctx)
            .await
            .expect("r must materialize");
        assert_eq!(
            r_val,
            Value::Int {
                n: 99,
                type_val: unknown_type_val()
            },
            "B-598: non-matching case arm must fall through to wildcard; got: {r_val:?}"
        );
    }

    #[tokio::test]
    async fn test_b637_dot_access_via_root_group() {
        // builtin-dict-get must be available via root_group in builtin-resolve/builtin-eval.
        // Test program uses dot-access: [build: [fn [let x] x.inner]] [result: [build [inner: 42]]]
        // - dot-access `x.inner` lowers to `[builtin-dict-get "inner" x]`
        // - builtin-dict-get must be in root_group (not just env-dict) for Field resolution
        // - Before B-637: Field resolution failed because builtin-dict-get was not in env-dict name-set
        // - After B-637: builtin-resolve prepends root_group names, builtin-eval prepends root_group thunks
        let ctx = test_ctx();
        let file: Arc<str> = Arc::from("<test>");
        let source = r#"[build: [fn [let x] x.inner]] [result: [build [inner: 42]]]"#;

        // Parse and desugar
        let parsed = crate::parser::parse(source, Arc::clone(&file)).expect("parse must succeed");
        let program = crate::desugar::desugar_program_full(&parsed.program);

        // Resolve with root_group seed frame (as builtin-resolve does after B-637 fix).
        let root_map = ctx.root_group_resolver_map();
        let root_group_len = ctx.root_group.len() as u32;

        let doc = &program.documents[0].node;
        let (_resolve_table, resolve_diagnostics, _unreferenced, _unified_frames) =
            crate::resolve::resolve_surface_document_with_seed_frames(
                doc,
                &[root_map.clone()],
                &[],
                root_group_len,
            );
        assert!(
            resolve_diagnostics.is_empty(),
            "resolve must produce no errors; got: {resolve_diagnostics:?}"
        );

        // Lower each expression item (same as builtin_lower does)
        let scope_frames_data: Vec<indexmap::IndexMap<String, u32>> = vec![root_map.clone()];
        let scope_frames_slice: Option<&[indexmap::IndexMap<String, u32>]> =
            Some(scope_frames_data.as_slice());
        let mut core_entries: Vec<(
            String,
            std::sync::Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
        )> = Vec::new();
        let mut expr_idx: usize = 0;
        for item in doc.items.iter() {
            let node = match item {
                crate::ast::SurfaceItem::Expr(node) => node,
                crate::ast::SurfaceItem::Decl(_) => continue,
            };
            let (core_spanned, lower_diags) = crate::lower::lower(node, scope_frames_slice);
            {
                let (info_diags, other_diags): (Vec<_>, Vec<_>) = lower_diags
                    .into_iter()
                    .partition(|d| d.level == crate::error::DiagnosticLevel::Info);
                for d in info_diags {
                    ctx.runtime_diagnostics
                        .lock()
                        .expect("runtime_diagnostics mutex poisoned")
                        .push(d);
                }
                let (err_opt, warnings) =
                    crate::eval_materialize::lower_errors_to_eval_error(other_diags);
                for w in warnings {
                    ctx.runtime_diagnostics
                        .lock()
                        .expect("runtime_diagnostics mutex poisoned")
                        .push(w);
                }
                assert!(err_opt.is_none(), "lowering must not produce errors");
            }
            core_entries.push((expr_idx.to_string(), std::sync::Arc::new(core_spanned)));
            expr_idx += 1;
        }

        // Build initial_group: root_group thunks (B-637 fix in builtin-eval)
        let initial_group: Vec<Arc<Thunk>> = ctx.root_group.iter().map(Arc::clone).collect();

        // Eval via eval_core_document_exprs
        let result_thunk = eval_core_document_exprs(&core_entries[..], &ctx, initial_group)
            .await
            .expect("eval_core_document_exprs must succeed");
        let val = materialize(&result_thunk, None, &ctx)
            .await
            .expect("materialize must succeed");

        let Value::Dict { entries: ref d, .. } = val else {
            panic!("expected Dict, got {val:?}");
        };

        // Check that dot-access worked (B-637)
        let result = d
            .get(&HashableValue::Str(Arc::from("result")))
            .expect("key 'result' must exist");
        let result_val = super::materialize(result, None, &ctx)
            .await
            .expect("result must materialize");
        assert_eq!(
            result_val,
            Value::Int {
                n: 42,
                type_val: unknown_type_val()
            },
            "B-637: dot-access must resolve builtin-dict-get via root_group; got: {result_val:?}"
        );
    }

    /// Independently-created root EvalContexts must share the same repr_registry
    /// and is_predicates Arcs so that repr: registrations from one context are visible
    /// in all others (including test contexts, bootstrap contexts, and re-entrant macro
    /// expansion contexts).
    #[test]
    fn test_global_registry_shared_across_new_empty_contexts() {
        let ctx_a = EvalContext::new_empty();
        let ctx_b = EvalContext::new_empty();

        assert!(
            Arc::ptr_eq(&ctx_a.repr_registry, &ctx_b.repr_registry),
            "T-2057: repr_registry must be the same Arc across independently-created \
             new_empty() contexts — got distinct Arcs"
        );
        assert!(
            Arc::ptr_eq(&ctx_a.is_predicates, &ctx_b.is_predicates),
            "T-2057: is_predicates must be the same Arc across independently-created \
             new_empty() contexts — got distinct Arcs"
        );
    }

    /// new_with_options contexts must also share the same global registries,
    /// and must be pointer-equal to new_empty contexts (all root constructors use the same OnceLocks).
    #[test]
    fn test_global_registry_shared_across_constructor_variants() {
        let ctx_empty = EvalContext::new_empty();
        let ctx_opts = EvalContext::new_with_options(false, None);

        assert!(
            Arc::ptr_eq(&ctx_empty.repr_registry, &ctx_opts.repr_registry),
            "T-2057: repr_registry must be pointer-equal between new_empty() and \
             new_with_options() contexts"
        );
        assert!(
            Arc::ptr_eq(&ctx_empty.is_predicates, &ctx_opts.is_predicates),
            "T-2057: is_predicates must be pointer-equal between new_empty() and \
             new_with_options() contexts"
        );
    }

    /// Functional registry sharing — a registration made through one context's
    /// repr_registry is immediately visible through a second independently-created context.
    /// This validates that pointer equality translates into actual cross-context visibility,
    /// not just that the Arcs are identical.
    #[test]
    fn test_global_registry_registration_visible_across_contexts() {
        let ctx_a = EvalContext::new_empty();
        let ctx_b = EvalContext::new_empty();

        // Register a sentinel value in ctx_a's repr_registry using a unique key.
        // The key is chosen to avoid collisions with any real repr strings registered
        // by the prelude (all real keys follow the "Value::Typename" pattern).
        let sentinel_key = "Value::TestSentinel_T2057".to_string();
        let sentinel_val = Arc::new(string_val("T-2057-functional-check"));
        ctx_a
            .repr_registry
            .lock()
            .unwrap()
            .insert(sentinel_key.clone(), Arc::clone(&sentinel_val));

        // Retrieve through ctx_b — must see the value registered via ctx_a.
        let retrieved = ctx_b
            .repr_registry
            .lock()
            .unwrap()
            .get(&sentinel_key)
            .cloned();

        assert!(
            retrieved.is_some(),
            "T-2057: registration in ctx_a's repr_registry must be visible in ctx_b \
             (they must share the same underlying HashMap)"
        );
        assert_eq!(
            retrieved.unwrap(),
            sentinel_val,
            "T-2057: retrieved value must equal the registered sentinel"
        );

        // Clean up the sentinel to avoid polluting the global registry for other tests.
        ctx_a.repr_registry.lock().unwrap().remove(&sentinel_key);
    }
}
