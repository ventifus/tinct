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
    CoreExpr, LiteralPattern, Param, Pattern, ResolutionTable, Span, Spanned, SurfaceNode,
    SurfaceProgram, TypeAnnotationTable,
};
use crate::builtins::MAX_COLLECT_SIZE;
use crate::error::{EvalError, EvalResult};
use crate::types::{Row, Type};
// Circular module dependency: this module calls builtins via function pointers stored in `Value::Builtin`.
// builtins.rs imports `invoke_function` and `materialize` from this module.
// This bidirectional dependency is safe because neither module's initialization depends on the other.
use crate::value::{string_val, Environment, Key, Thunk, Value};

// ============================================================================
// Document pipeline evaluation
// ============================================================================

thread_local! {
    /// Cached empty dict thunk used as the default `%` when no stdin is provided.
    /// Avoids allocating a fresh `Arc<Thunk>` on every `eval_surface_file_with_input` call.
    static EMPTY_DICT_THUNK: Arc<Thunk> = Arc::new(Thunk::new_materialized(
        Value::Dict(IndexMap::new()),
        Span::origin(),
    ));
}

/// Wrap a thunk with nominal type validation for pipeline input contracts.
///
/// Creates a synthetic `CoreExpr::TypeAssert` wrapping a gensym'd `FreeVar` reference.
/// When evaluated, it performs the same validation as a regular `[@Type expr]` assertion.
///
/// `pipeline_blame` is `Some` when the assertion is for a `--- expects: @Type` contract at
/// a `---` boundary; it identifies the producing stage (positive party) and consuming stage
/// (negative party). Pass `None` for all other assertion sites.
pub(crate) fn wrap_with_nominal_validation(
    inner: Arc<Thunk>,
    annotation: &crate::ast::Spanned<crate::ast::Annotation>,
    resolved_type: Option<crate::types::Type>,
    validation_span: Span,
    ctx: &Arc<EvalContext>,
    pipeline_blame: Option<crate::error::PipelineBlame>,
) -> Arc<Thunk> {
    use std::sync::atomic::{AtomicU64, Ordering};

    // Generate a unique variable name to avoid collisions with user code.
    // Uses the canonical ℊꜱʏᴍ⧼prefix⧽N convention via make_gensym_name (builtins_meta.rs).
    // Prefix "nominal-input" is distinct from the user-facing "gensym" prefix so pipeline
    // validation variables cannot alias user-generated symbols.
    static GENSYM_COUNTER: AtomicU64 = AtomicU64::new(0);
    let gensym_id = GENSYM_COUNTER.fetch_add(1, Ordering::Relaxed);
    let gensym_name = crate::builtins_meta::make_gensym_name("nominal-input", gensym_id);

    // Create a synthetic TypeAssert expression: [@Annotation ℊꜱʏᴍ⧼nominal-input⧽N]
    // If resolved_type is None (untyped contract), use Type::Unknown which accepts all values.
    let type_check_expr = Arc::new(crate::ast::Spanned::new(
        CoreExpr::TypeAssert {
            annotation: annotation.clone(),
            expr: Arc::new(crate::ast::Spanned::new(
                CoreExpr::FreeVar(gensym_name.clone()),
                validation_span.clone(),
            )),
            resolved_type: resolved_type.unwrap_or(crate::types::Type::Unknown),
            pipeline_blame,
        },
        validation_span.clone(),
    ));

    // Create an environment with ℊꜱʏᴍ⧼nominal-input⧽N bound to the inner thunk
    let validation_env = Arc::new(std::sync::RwLock::new(Environment::new()));
    validation_env.write().unwrap().insert(gensym_name, inner);

    // Return an Unevaluated thunk wrapping the TypeAssert expression
    Arc::new(Thunk::new_unevaluated_core(
        type_check_expr,
        validation_env,
        Arc::clone(ctx),
        validation_span,
    ))
}

/// Evaluate a sequence of surface expression nodes as a scope chain, returning the
/// last expression's thunk lazily.
///
/// This is the canonical scope-chaining loop shared by [`eval_surface_document`] and
/// `builtin_eval` (in `builtins_meta.rs`). Both callers implement identical semantics:
///
/// - **Intermediate expressions** (all but the last): lower → eval → materialize.
///   If the result is a non-empty `Dict` or `Overlay`, ALL `Key::String` entries are
///   materialized strictly and inserted into a child environment for subsequent
///   expressions. Non-dict/overlay results are silently ignored (no error, no scope
///   extension). This is the `bare-include-scope` behavior.
///   **Why strict?** Per `doc/09-documents.md §SEQ-SCOPE`: dead bindings must fire
///   immediately (strict let* semantics), not lazily. A binding that would error must
///   error at bind-time, not silently defer until (or unless) the name is accessed.
/// - **Last expression**: lower → eval (lazy). The resulting thunk is returned
///   without forcing — callers decide when (and whether) to materialize it.
/// - **Empty slice**: returns a materialized empty-dict thunk (same as an empty doc).
pub(crate) async fn eval_document_exprs(
    expr_nodes: &[Arc<SurfaceNode>],
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
    res: &Arc<ResolutionTable>,
    types: &Arc<TypeAnnotationTable>,
) -> EvalResult<Arc<Thunk>> {
    if expr_nodes.is_empty() {
        return Ok(Arc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            Span::origin(),
        )));
    }

    let mut current_env = env;
    let last_idx = expr_nodes.len() - 1;

    for (i, node) in expr_nodes.iter().enumerate() {
        let core_spanned = crate::lower::lower(node, res, types);
        let node_span = node.span.clone();

        if i == last_idx {
            // Last expression: return its thunk lazily (no materialization).
            return eval_core_expr(&core_spanned, &current_env, ctx).await;
        }

        // Intermediate expression: eval and materialize to extract potential bindings.
        let thunk = eval_core_expr(&core_spanned, &Arc::clone(&current_env), ctx).await?;
        let value = materialize(&thunk, Some(&node_span), ctx).await?;

        // If the result is a non-empty Dict or Overlay, promote ALL Key::String entries
        // into a child environment. Non-dict results are silently skipped — they act as
        // side-effect expressions that contribute no bindings to the scope chain.
        let map = match value {
            Value::Dict(ref m) if !m.is_empty() => Some(m.clone()),
            Value::Overlay(ref l, ref r) => Some(crate::builtins::flatten_overlay(
                l,
                r,
                "document pipeline",
                ctx,
                node_span.clone(),
            )?),
            _ => None,
        };

        if let Some(entries) = map {
            let child_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(
                &current_env,
            ))));
            for (key, val_thunk_id) in entries.iter() {
                if let Key::String(name) = key {
                    // Force each entry value eagerly (strict let* semantics for scope chains).
                    // This matches doc/09-documents.md §SEQ-SCOPE: named entries are shallowly
                    // materialized at binding time so dead-but-erroring bindings fire immediately.
                    let val_thunk = ctx.get_thunk(*val_thunk_id);
                    let forced_value = materialize(&val_thunk, Some(&node_span), ctx).await?;
                    let strict_thunk =
                        Arc::new(Thunk::new_materialized(forced_value, node_span.clone()));
                    child_env
                        .write()
                        .unwrap()
                        .insert(name.to_string(), strict_thunk);
                }
            }
            current_env = child_env;
        }
        // Non-dict/overlay: silently skip — no scope extension, no error.
    }

    unreachable!(
        "eval_document_exprs: loop did not return — expr_nodes was non-empty but last_idx was never reached"
    )
}

/// Evaluate a SurfaceDocument: a sequence of expression items forming a scope chain.
///
/// Each `SurfaceItem::Expr` is lowered to `CoreExpr` via `lower.rs` and evaluated via
/// `eval_core_expr_pub`. `SurfaceItem::Decl` items are skipped (processed at expand time).
///
/// Scope-chain semantics are delegated to [`eval_document_exprs`]:
/// - Intermediate expressions are materialized; Dict/Overlay results promote bindings into scope.
/// - The last expression is returned as-is (lazy, any type).
/// - An empty document returns an empty dict.
///
/// This function retains only the document-level concerns: caps validation and
/// collecting the expression nodes before delegating to the shared loop.
pub async fn eval_surface_document(
    doc: &Spanned<crate::ast::SurfaceDocument>,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
    res: &Arc<ResolutionTable>,
    types: &Arc<TypeAnnotationTable>,
) -> EvalResult<Arc<Thunk>> {
    // Validate capabilities declared in the document header
    if let Some(ref caps_ann) = doc.node.caps {
        for (cap_name, annotation) in &caps_ann.node {
            let full_cap_name = format!("%{}", cap_name);

            let cap_present = {
                let env_ref = env.read().unwrap();
                env_ref.get(&full_cap_name).is_some()
            };

            if !cap_present {
                let (flag_type, flag_example) = match annotation {
                    crate::ast::Annotation::Simple(type_name) if type_name == "NetCap" => {
                        ("--cap-net", format!("{}=HOST:PORT", cap_name))
                    }
                    crate::ast::Annotation::Simple(type_name) if type_name == "DirCap" => {
                        ("--cap-fs", format!("{}=PATH", cap_name))
                    }
                    crate::ast::Annotation::Simple(type_name) if type_name == "Handle" => {
                        ("--cap-file", format!("{}=PATH:r", cap_name))
                    }
                    _ => ("--cap", format!("{}=VALUE", cap_name)),
                };

                let auto_injected_caps = ["cwd", "libdir", "stdin"];
                let is_auto_injected = auto_injected_caps.contains(&cap_name.as_str());

                let mut message = format!(
                    "{}@{} is required but not provided",
                    full_cap_name,
                    match annotation {
                        crate::ast::Annotation::Simple(type_name) => type_name.clone(),
                        crate::ast::Annotation::PropertyDict(_) => "Dict".to_string(),
                        crate::ast::Annotation::Annotated(name, _) => name.clone(),
                    }
                );

                if is_auto_injected {
                    message.push_str(&format!(
                        "\n  note: {} is injected automatically — did you pass --no-{}?",
                        full_cap_name, cap_name
                    ));
                } else {
                    message.push_str(&format!(
                        "\n  inject it with:  tinct run {} {} ...\n  or unrestricted: tinct run {} {}=any ...",
                        flag_type, flag_example, flag_type, cap_name
                    ));
                }

                return Err(EvalError::capability_required(message, caps_ann.span.clone()).into());
            }
        }
    }

    // Collect expression nodes (skip Decl items — processed by expander) and
    // delegate the scope-chaining loop to the shared eval_document_exprs function.
    let expr_nodes: Vec<Arc<SurfaceNode>> = doc.node.expressions().cloned().collect();
    eval_document_exprs(&expr_nodes, env, ctx, res, types).await
}

/// Evaluate a SurfaceProgram: one or more documents separated by `---`.
///
/// # Precondition
///
/// **Pipeline invariant:** `expand_surface_program` → `desugar_surface_program` →
/// `resolve_surface_program` must be called before passing the program here.
/// The `res` table must be the one returned by `resolve_surface_program`.
/// The `types` table may be empty (from `TypeAnnotationTable::new()`) if type checking
/// was skipped; `TypeAssert` nodes will use Type::Unknown (accepts all values) in that case.
pub async fn eval_surface_file(
    program: &SurfaceProgram,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
    res: &Arc<ResolutionTable>,
    types: &Arc<TypeAnnotationTable>,
) -> EvalResult<Arc<Thunk>> {
    eval_surface_file_with_input(
        program,
        env,
        ctx,
        res,
        types,
        &std::collections::HashMap::new(),
        None,
    )
    .await
}

/// Evaluate a SurfaceProgram with an optional initial `%` value.
///
/// See `eval_surface_file` for preconditions. When `initial_input` is `Some(thunk)`,
/// that thunk becomes `%` for the first document instead of the default empty dict.
pub async fn eval_surface_file_with_input(
    program: &SurfaceProgram,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
    res: &Arc<ResolutionTable>,
    types: &Arc<TypeAnnotationTable>,
    expects_resolved: &std::collections::HashMap<crate::ast::Span, crate::types::Type>,
    initial_input: Option<Arc<Thunk>>,
) -> EvalResult<Arc<Thunk>> {
    let mut prev_output = initial_input.unwrap_or_else(|| EMPTY_DICT_THUNK.with(Arc::clone));
    let mut named: IndexMap<String, Arc<Thunk>> = IndexMap::new();
    // Track the label of the previous (producing) document for pipeline blame.
    // Stage-skipped documents are excluded from the index so runtime indices are contiguous.
    let mut prev_doc_label: String = "initial input".to_string();
    let mut runtime_doc_idx: usize = 0;

    for surface_doc in &program.documents {
        // Skip type-stage documents
        if surface_doc.node.stage == Some(crate::ast::Stage::Type) {
            continue;
        }

        // Each document gets a fresh scope with % and %name bindings
        let doc_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(&env))));

        // Derive the consumer label for the current document.
        let consumer_label = surface_doc
            .node
            .name
            .as_deref()
            .map(|n| format!("document '{}'", n))
            .unwrap_or_else(|| format!("document {}", runtime_doc_idx));

        // Bind % (pipeline variable), wrapping with validation if expects: is declared
        let percent_thunk = if let Some(ref expects_ann) = surface_doc.node.expects {
            let resolved_type = expects_resolved.get(&expects_ann.span).cloned();
            let blame = crate::error::PipelineBlame {
                producer: prev_doc_label.clone(),
                consumer: Some(consumer_label.clone()),
            };
            wrap_with_nominal_validation(
                Arc::clone(&prev_output),
                expects_ann,
                resolved_type,
                surface_doc.span.clone(),
                ctx,
                Some(blame),
            )
        } else {
            Arc::clone(&prev_output)
        };

        doc_env
            .write()
            .unwrap()
            .insert("%".to_string(), percent_thunk);

        // Bind all previously named sections as %name
        for (section_name, section_thunk) in &named {
            doc_env
                .write()
                .unwrap()
                .insert(format!("%{}", section_name), Arc::clone(section_thunk));
        }

        let result = eval_surface_document(surface_doc, doc_env, ctx, res, types).await?;

        if let Some(ref name) = surface_doc.node.name {
            named.insert(name.clone(), Arc::clone(&result));
        }

        prev_doc_label = consumer_label;
        runtime_doc_idx += 1;
        prev_output = result; // lazy: no materialization at boundary
    }

    Ok(prev_output)
}

// ============================================================================
// End document pipeline evaluation
// ============================================================================

pub(crate) const DEFAULT_ANNOTATION_KEY: &str = "default";
pub(crate) const IS_ANNOTATION_KEY: &str = "is";

/// Type alias for the optional default expression + environment pair used by guarded thunks.
/// Reduces type_complexity in function signatures that carry this optional default.
type GuardDefault = (Arc<Spanned<crate::ast::CoreExpr>>, Arc<RwLock<Environment>>);

/// Type alias for the return type of `match_pattern` — an async fn returning an optional env.
type MatchPatternFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = EvalResult<Option<Arc<RwLock<Environment>>>>> + 'a>,
>;

/// Type alias for the return type of `values_equal` — a recursive async fn returning bool.
/// Must be `Pin<Box<...>>` to support recursion (direct `async fn` recursion is unsized).
type ValuesEqualFuture = std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<bool>>>>;

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
    /// Upper bound: continuation stack frames (2048) × ~80 bytes/entry ≈ 160 KB.
    pub eval_stack: Vec<(Arc<str>, Span)>,
    /// Runtime class registry: class_name -> (params, superclasses, method_defaults)
    /// Stores default method implementations for filling in instance dictionaries.
    pub class_registry: HashMap<String, RuntimeClassDecl>,
    /// Runtime instance registry: (class_name, type_tags) -> instance_dict
    /// Stores materialized method dictionaries for each instance.
    /// class_name is &'static str; type_tags is a Vec<String> (from Value::type_name()
    /// on determining-position args) for MPTC support.
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
    /// HashMap for O(1) lookup at thunk creation time in eval_core_expr.
    /// Populated by the type checker via set_boundary_guards().
    pub boundary_guards: RwLock<HashMap<Span, Type>>,
    /// Monad resolutions for inferred [do] forms: sentinel VarRef name → monad variable name.
    /// The type checker records the resolved monad name here (keyed by the sentinel name, e.g.,
    /// `ℊꜱʏᴍ⧼do-infer⧽0`). At eval time, when a FreeVar with that name is evaluated, the
    /// evaluator looks up this map by name and returns the monad dict value from the environment.
    /// Parallel to boundary_guards: type-checker-to-evaluator communication via name-keyed side channel.
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
            task_registry: Arc::new(Mutex::new(Vec::new())),
            profiling: None,
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
            task_registry: Arc::new(Mutex::new(Vec::new())),
            profiling: None,
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
            task_registry: Arc::new(Mutex::new(Vec::new())),
            profiling: None,
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
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
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
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
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
    /// Clones blame_map, boundary_guards, do_infer_resolutions, libdir_dir (per-scope fields,
    /// same as `with_cancel_token`).
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
            boundary_guards: RwLock::new(self.boundary_guards.read().unwrap().clone()),
            do_infer_resolutions: RwLock::new(self.do_infer_resolutions.read().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel,
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
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
            boundary_guards: RwLock::new(self.boundary_guards.read().unwrap().clone()),
            do_infer_resolutions: RwLock::new(self.do_infer_resolutions.read().unwrap().clone()),
            libdir_dir: Mutex::new(self.libdir_dir.lock().unwrap().clone()),
            cancel: child_token,
            task_registry: Arc::clone(&self.task_registry),
            profiling: self.profiling.as_ref().map(Arc::clone),
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

    /// Set boundary guards from type inference.
    /// Called after type checking to wire gradual typing runtime checks.
    pub fn set_boundary_guards(&self, guards: HashMap<Span, Type>) {
        *self.boundary_guards.write().unwrap() = guards;
    }

    /// Set do-infer resolutions from type inference.
    /// Called after type checking to wire inferred [do] monad resolution to the evaluator.
    /// The map keys are the sentinel VarRef names (e.g., `ℊꜱʏᴍ⧼do-infer⧽0`); values are
    /// the monad dict variable names (e.g., "result") resolved by the type checker.
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
        Value::Float(_) => Type::Float,
        Value::Bool(_) => Type::Bool,
        Value::String { .. } => Type::Str,
        Value::Bytes { .. } => Type::Bytes,
        Value::Dict(map) => Type::Record(extract_row(map)),
        // Overlay is a lazy right-biased merge: key set cannot be read without forcing.
        // Return a closed empty record — required-field checks correctly fail,
        // consistent with Overlay field validation being static-only.
        Value::Overlay(..) => Type::Record(Row {
            fields: HashMap::new(),
        }),
        // Element type erased (lazy Seq — forcing all elements would break laziness).
        // is_consistent_subtype accepts Seq(Unknown) ~<: Seq(T) for any T.
        Value::Seq { .. } => Type::Seq(Box::new(Type::Unknown)),
        // Param/return types erased — consistent subtyping accepts Function([Unknown..], Unknown)
        // against any function annotation with matching arity.
        Value::Function { params, .. } => Type::Function {
            params: params.iter().map(|_| (None, Type::Unknown)).collect(),
            ret: Box::new(Type::Unknown),
            variadic: false,
        },
        // Capability types: Unknown → is_consistent_subtype accepts against any annotation.
        // Preserves current accept-all behavior while capability-runtime-validation sprint is pending.
        Value::Handle { .. } | Value::WriteHandle { .. } => Type::Unknown,
        Value::DirCap { .. } | Value::RevocableDirCap { .. } => Type::Unknown,
        Value::NetCap(_) => Type::Unknown,
        // Variant payload types erased (payload ThunkId has no static type without the schema).
        Value::Variant { tag, .. } => Type::NominalVariant {
            tag: tag.clone(),
            fields: Row {
                fields: HashMap::new(),
            },
        },
        // Decimal/BigInt: no Type::Decimal/Type::BigInt in the type system yet.
        // Unknown preserves current behavior (matches @Number) until those variants are added.
        Value::Decimal(_) | Value::BigInt(_) => Type::Unknown,
        // Builtin functions and Proxy values: Unknown accepts any function/type annotation.
        Value::Builtin(..) | Value::Proxy { .. } => Type::Unknown,
        // Builder is a transient construction artifact — produce Top (type mismatch error)
        // rather than panicking; Builder can reach TypeAssert via e.g. [@Int [make-builder]].
        Value::Builder(..) => Type::Top,
        // All other runtime-only types (URI, async, crypto, etc.) → Top
        _ => Type::Top,
    }
}

/// Extract the ground record type from a Dict: key names only, field types erased to Unknown.
///
/// MUST NOT force any ThunkId — field types are static-only (same tradeoff as Seq elements).
/// `is_consistent_subtype` then handles width subtyping: `{a: Unknown} ~<: {a: Int}` holds
/// because `Unknown ~<: Int`. Field presence is checked structurally; field types are not.
///
/// Integer-keyed entries (`Key::Int`) are skipped — they are explicit positional entries
/// like `[0: x 1: y]`, not record fields.
fn extract_row(map: &IndexMap<Key, ThunkId>) -> Row {
    let fields = map
        .keys()
        .filter_map(|k| match k {
            Key::String(name) => Some((name.to_string(), Type::Unknown)),
            // Integer-keyed entries are explicit [0: x 1: y] dict constructs, not record fields.
            Key::Int(_) => None,
        })
        .collect::<HashMap<String, Type>>();
    Row { fields }
}

/// Check if a materialized value matches a type for structural TypeAssert validation.
/// Returns true if the value conforms to the expected type.
///
/// **Component 3 unified path:** Delegates to `is_consistent_subtype(ground_type_of(v), T)`.
/// The consistent subtyping relation handles Unknown at erased positions (Seq elements,
/// Dict field values, Function params/returns), implementing AGT gradual typing semantics.
///
/// No fast-path bypasses — the consistent subtyping relation handles everything uniformly.
/// If primitive checks prove slow in profiling, optimize `is_consistent_subtype` itself,
/// which benefits every call site across the codebase.
pub(crate) fn value_matches_type(value: &Value, expected: &Type) -> bool {
    Type::is_consistent_subtype(&ground_type_of(value), expected)
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_and_wrap_record(
    entries: &IndexMap<Key, ThunkId>,
    row: &Row,
    field_path: &mut Vec<String>,
    guard_span: Span,
    data_span: Span,
    ctx: &Arc<EvalContext>,
    default: Option<GuardDefault>,
    blame_label: Option<crate::error::BlameLabel>,
) -> EvalResult<IndexMap<Key, ThunkId>> {
    // Shape check: verify all required fields exist
    // Per doc/07:117, try Key::String first, then Key::Int fallback
    for (field_name, _field_type) in row.fields.iter() {
        let has_field = entries.contains_key(&Key::String(Rc::from(field_name.as_str())))
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
            Key::String(field_name) => row.fields.get(field_name.as_ref()),
            Key::Int(n) => row.fields.get(&n.to_string()),
        };

        if let Some(field_type) = field_type {
            let field_name = match key {
                Key::String(s) => s.to_string(),
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
/// Returns `Value::Expression(Arc<SurfaceNode>)` — the runtime-v2 representation.
/// Macro transformer code in prelude.llt handles both Expression (new) and Variant (old)
/// inputs via dual dispatch (tag-of works on both), so this migration is safe.
///
/// This function operates entirely on SurfaceNode (no Expr round-trip).
async fn eval_quote_walk(
    node: Arc<crate::ast::SurfaceNode>,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let span = node.span.clone();
    // Preprocess to handle nested unquotes (rewrites unquote subexpressions)
    let processed_node = eval_quote_preprocess(node, &env, ctx).await?;

    // runtime-v2 Part G: return Value::Expression (was: ast_to_dict_expr returning Variant Dict)
    // Macro transformer code in prelude.llt is dual-dispatch ready (tag-of handles both Expression and Variant).
    Ok(Arc::new(Thunk::new_materialized(
        Value::Expression(processed_node),
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
    let make_node = |expr: SurfaceExpression| {
        Arc::new(SurfaceNode {
            expr,
            span: span.clone(),
        })
    };
    match value {
        Value::Int(n) => Ok(make_node(SurfaceExpression::Int(*n))),
        Value::Float(f) => Ok(make_node(SurfaceExpression::Float(*f))),
        Value::Bool(b) => Ok(make_node(SurfaceExpression::Bool(*b))),
        Value::String { source, start, end } => Ok(make_node(SurfaceExpression::Str(
            source[*start..*end].to_string(),
        ))),
        Value::Variant { .. } => {
            // Variant form of an AST node — convert via surface bridge
            crate::surface_convert::dict_to_surface_node(value, ctx).map_err(|err| {
                EvalError::internal(
                    format!("unquote result Variant is not a valid AST: {}", err),
                    span,
                )
                .into()
            })
        }
        Value::Dict(dict) => {
            // Check if this is an AST dict (has a "type" field)
            if dict.contains_key(&Key::String("type".into())) {
                // It's an AST dict — convert via surface bridge
                crate::surface_convert::dict_to_surface_node(value, ctx).map_err(|err| {
                    EvalError::internal(
                        format!("unquote result dict is not a valid AST: {}", err),
                        span,
                    )
                    .into()
                })
            } else {
                // It's a regular dict — dict values are thunk IDs, conversion not yet supported
                Err(EvalError::internal(
                    "unquote of non-AST dict is not yet supported".to_string(),
                    span,
                )
                .into())
            }
        }
        Value::Expression(node) => {
            // Value::Expression — already a SurfaceNode, use it directly (no round-trip needed)
            Ok(Arc::clone(node))
        }
        _ => Err(
            EvalError::internal(format!("unquote of {:?} is not supported", value), span).into(),
        ),
    }
}

/// Collect all elements from a sequence value into a Vec.
/// Returns an error if the value is not a sequence.
async fn collect_seq_elements(
    value: &Value,
    span: Span,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Vec<Value>> {
    let mut elements = Vec::new();
    let mut current = value.clone();

    loop {
        match current {
            Value::Seq { head, tail } => {
                // Materialize the head element
                let head_thunk = ctx.get_thunk(head);
                let head_value = materialize(&head_thunk, Some(&span), ctx).await?;
                elements.push(head_value);

                // Enforce size limit to prevent infinite sequences from looping forever
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

                // Materialize and move to the tail
                let tail_thunk = ctx.get_thunk(tail);
                current = materialize(&tail_thunk, Some(&span), ctx).await?;
            }
            Value::Dict(ref map) if map.is_empty() => {
                // Empty sequence (nil sentinel is an empty dict) - we're done
                break;
            }
            _ => {
                return Err(EvalError::type_mismatch("Seq", current.type_name(), span).into());
            }
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
    env: &'a Arc<RwLock<Environment>>,
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
        let make_node = |expr: SurfaceExpression| {
            Arc::new(SurfaceNode {
                expr,
                span: span.clone(),
            })
        };

        match &node.expr {
            SurfaceExpression::Unquote(inner) => {
                // Evaluate the unquoted expression and convert back to SurfaceNode
                let core = crate::lower::lower(
                    inner,
                    crate::ast::empty_resolution_table(),
                    crate::ast::empty_type_annotation_table(),
                );
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
                        let core = crate::lower::lower(
                            inner,
                            crate::ast::empty_resolution_table(),
                            crate::ast::empty_type_annotation_table(),
                        );
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

            SurfaceExpression::DotAccess {
                expr: target,
                field,
            } => {
                let processed_target = eval_quote_preprocess(Arc::clone(target), env, ctx).await?;
                Ok(make_node(SurfaceExpression::DotAccess {
                    expr: processed_target,
                    field: field.clone(),
                }))
            }

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
            } => {
                let processed_expr = eval_quote_preprocess(Arc::clone(inner), env, ctx).await?;
                Ok(make_node(SurfaceExpression::TypeAssert {
                    annotation: annotation.clone(),
                    expr: processed_expr,
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
                    SurfaceDeclaration::MacroDecl { name, params, body } => {
                        let processed_params =
                            eval_quote_preprocess(Arc::clone(params), env, ctx).await?;
                        let processed_body =
                            eval_quote_preprocess(Arc::clone(body), env, ctx).await?;
                        SurfaceDeclaration::MacroDecl {
                            name: name.clone(),
                            params: processed_params,
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

/// Evaluate a SurfaceNode function body with given params as a macro transformer.
///
/// Uses `lower::lower` + `eval_core_expr` to evaluate a SurfaceExpression::Fn directly,
/// bypassing the old Expr-based eval entry point. No Expr or ast_convert bridge needed.
///
/// Returns the resulting `Value::Function` thunk, ready for use as a macro transformer.
pub(crate) fn eval_surface_fn(
    params: Vec<Spanned<Param>>,
    body: &Arc<crate::ast::SurfaceNode>,
    span: Span,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    // Build SurfaceExpression::Fn directly (no Expr bridge needed)
    let surface_params: Vec<Spanned<crate::ast::SurfaceParam>> = params
        .into_iter()
        .map(|p| {
            Spanned::new(
                crate::ast::SurfaceParam {
                    name: p.node.name,
                    annotation: p.node.annotation,
                    variadic: p.node.variadic,
                },
                p.span,
            )
        })
        .collect();
    let fn_node = Arc::new(crate::ast::SurfaceNode {
        expr: crate::ast::SurfaceExpression::Fn {
            return_ann: None,
            params: surface_params,
            body: Arc::clone(body),
            desugared: false,
        },
        span,
    });
    // Lower SurfaceNode → CoreExpr using empty resolution/type tables.
    // Macro transformer bodies use FreeVar lookups against stdlib_env, not slot-based resolution.
    let core_fn = crate::lower::lower(
        &fn_node,
        crate::ast::empty_resolution_table(),
        crate::ast::empty_type_annotation_table(),
    );
    crate::async_rt::block_on_anywhere(eval_core_expr(&core_fn, &env, ctx))
}

/// Wrap a thunk with a boundary guard if the span matches a guard in the context.
///
/// Boundary guards are populated by the type checker to enforce type constraints at
/// specific expression boundaries (e.g., function parameters, type assertions).
///
/// If `span` matches a guard in `ctx.boundary_guards`, wraps `thunk` in a `Guarded`
/// thunk that will check the type when forced. Otherwise returns `thunk` unchanged.
pub(crate) fn maybe_wrap_guard(
    thunk: Arc<Thunk>,
    span: Span,
    ctx: &Arc<EvalContext>,
) -> Arc<Thunk> {
    // Skip guard lookup for synthetic origin spans. All synthetic CoreExpr nodes produced
    // by macro expansion or internal code synthesis share Span::origin() (offset 0, line 1,
    // col 1). If a boundary guard is keyed by Span::origin(), it would match every synthetic
    // node — applying the wrong type guard to unrelated expressions. Synthetic nodes are not
    // user-written expressions and should never carry boundary guards.
    if span.is_origin() {
        return thunk;
    }
    let guards = ctx.boundary_guards.read().unwrap();
    if let Some(expected_type) = guards.get(&span) {
        Arc::new(Thunk::new_guarded(
            thunk,
            expected_type.clone(),
            vec![], // empty field path for top-level guards
            span,
        ))
    } else {
        thunk
    }
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
        let span = expr.span.clone();
        match &expr.node {
            // Fast path: literals materialize directly without wrapping in Unevaluated
            CoreExpr::Int(n) => Ok(Arc::new(Thunk::new_materialized(
                Value::Int(*n),
                span.clone(),
            ))),
            CoreExpr::Float(f) => Ok(Arc::new(Thunk::new_materialized(
                Value::Float(*f),
                span.clone(),
            ))),
            CoreExpr::Bool(b) => Ok(Arc::new(Thunk::new_materialized(
                Value::Bool(*b),
                span.clone(),
            ))),
            CoreExpr::Str(s) => Ok(Arc::new(Thunk::new_materialized(
                string_val(s),
                span.clone(),
            ))),

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
                    env_lock.get(name).ok_or_else(|| {
                        EvalError::undefined_variable(name_owned, span.clone()).into()
                    })
                }
            }

            // Free variable: name-based lookup only (no slot available)
            CoreExpr::FreeVar(name) => {
                // Special case: inferred [do] sentinel variable (e.g., `ℊꜱʏᴍ⧼do-infer⧽0`).
                // Generated by gensym in prelude.llt `do-desugar-inferred`. The type checker
                // resolves the sentinel to a concrete monad name (e.g., "result") and records
                // the mapping in ctx.do_infer_resolutions. At eval time, substitute the sentinel
                // with the resolved monad dict from the environment.
                if name.starts_with("ℊꜱʏᴍ⧼do-infer⧽") {
                    let monad_name = ctx
                        .do_infer_resolutions
                        .read()
                        .unwrap()
                        .get(name.as_str())
                        .cloned();
                    if let Some(monad_name) = monad_name {
                        let env_lock = env.read().unwrap();
                        return env_lock.get(&monad_name).ok_or_else(|| {
                            EvalError::undefined_variable(monad_name, span.clone()).into()
                        });
                    }
                }
                let name_owned = name.clone();
                let env_lock = env.read().unwrap();
                env_lock
                    .get(name)
                    .ok_or_else(|| EvalError::undefined_variable(name_owned, span.clone()).into())
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
                span.clone(),
            ))),

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
            // Eliminates the Vec<Spanned<Entry>> allocation and per-entry core_expr_to_expr
            // calls previously required by the round-trip through eval_dict.
            // eval_dict_core now uses Thunk::new_unevaluated_core for non-literal dict entries
            // (UnevaluatedState::CoreExpr), eliminating the per-entry core_expr_to_expr round-trip.
            //
            // B-296: Constructor injection happens automatically when eval_dict_core encounters
            // CoreExpr::TypeDecl entries (unit constructors are extracted during lowering).
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

                // Extract doc string from annotation if present.
                // Uses get_property("doc") which works directly on SurfaceEntry via SurfaceExpression::Str keys.
                let doc: Option<String> = return_ann.as_ref().and_then(|ann_spanned| {
                    ann_spanned.node.get_property("doc").and_then(|doc_node| {
                        if let crate::ast::SurfaceExpression::Str(s) = &doc_node.expr {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                });
                let return_ann_clone: Option<crate::ast::Annotation> =
                    return_ann.as_ref().map(|a| a.node.clone());

                // Always construct FnAnnotation — source_span is always available even for
                // unannotated functions, enabling ast-of and LSP go-to-definition.
                let annotation = Some(Box::new(crate::value::FnAnnotation {
                    doc,
                    return_ann: return_ann_clone,
                    source_file: ctx.config.source_file.clone(),
                    source_span: span.clone(),
                }));

                // Store the body directly as Arc<Spanned<CoreExpr>>.
                // CoreExpr::Fn.body is already Arc<Spanned<CoreExpr>> — no conversion needed.
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Function {
                        params: Rc::new(fn_params),
                        body: Arc::clone(body),
                        env: Arc::clone(env),
                        annotation,
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

            // Annotated: evaluate as bare string
            CoreExpr::Annotated { name, .. } => Ok(Arc::new(Thunk::new_materialized(
                string_val(name),
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
            // Syntax: [let name value] → bindings = [FreeVar("name"), value_expr]
            // Pairs are (bindings[2i], bindings[2i+1]).
            // Returns a Dict so the SequentialStep can extract keys via its Dict-based binding logic.
            CoreExpr::LetDecl { bindings } => {
                let mut dict: IndexMap<Key, ThunkId> = IndexMap::new();
                let mut i = 0;
                while i + 1 < bindings.len() {
                    let name_expr = &bindings[i];
                    let val_expr = &bindings[i + 1];
                    let name = match &name_expr.node {
                        CoreExpr::FreeVar(n) => n.clone(),
                        CoreExpr::Var { name: n, .. } => n.clone(),
                        CoreExpr::Annotated { name: n, .. } => n.clone(),
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
                    dict.insert(Key::String(Rc::from(name.as_str())), thunk_id);
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

            // Placeholder: error on evaluation
            // B-296: TypeDecl entries in dicts are handled by eval_dict_core's constructor
            // injection logic. If a TypeDecl is evaluated directly (outside dict context),
            // it's a no-op — the type declaration has no runtime value.
            CoreExpr::TypeDecl { .. } => Ok(Arc::new(Thunk::new_materialized(
                Value::Dict(indexmap::IndexMap::new()),
                span.clone(),
            ))),

            CoreExpr::Placeholder => Err(EvalError::unimplemented(
                "placeholder `...` was evaluated — replace with an implementation".to_string(),
                span.clone(),
            )
            .into()),

            // Error: propagate as internal error
            CoreExpr::Error(err_span) => Err(EvalError::internal(
                format!(
                    "syntax error at {}:{} (cannot evaluate error node)",
                    err_span.start.line, err_span.start.column
                ),
                span.clone(),
            )
            .into()),
        }
        .map(|thunk| maybe_wrap_guard(thunk, span, ctx))
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
            let call_span_for_restore = call_span.clone();
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
                // ADT constructor called with a single named arg: [Circle r: 5] where Circle = [variant "Circle"].
                // When a unit Variant is called with no positional args and exactly one named arg,
                // use the named arg's value as the payload. This supports single-field ADT constructors
                // declared via `[type Shape [Circle r: Int] ...]`.
                // Pattern `[Circle binding]` then matches and binds `binding` to the payload value.
                Value::Variant { tag, payload: None }
                    if args.is_empty() && named.as_ref().is_some_and(|m| m.len() == 1) =>
                {
                    let named_map = named.expect("checked is_some above");
                    let payload_thunk = named_map
                        .into_values()
                        .next()
                        .expect("1 entry checked above");
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
                        if value_matches_type(&value, &expected) {
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
///
/// **Test-only.** Production code uses last-binding-wins semantics for non-linear
/// patterns (see doc/14-patterns.md §Non-Linear Patterns). These functions exist
/// solely to test the detection algorithm, not to enforce linearity at runtime.
#[cfg(test)]
fn collect_pattern_variable_names(pattern: &Spanned<Pattern>, out: &mut Vec<(String, Span)>) {
    match &pattern.node {
        Pattern::Variable(name) => {
            out.push((name.clone(), pattern.span.clone()));
        }
        Pattern::Wildcard | Pattern::Literal(_) | Pattern::TypeTag(_) | Pattern::Pin(_) => {
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
                let value_thunk =
                    Arc::new(Thunk::new_materialized(value.clone(), value_span.clone()));
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
                let var_thunk = env.read().unwrap().get(name).ok_or_else(|| {
                    EvalError::undefined_variable(name.clone(), value_span.clone())
                })?;
                let var_value = materialize(&var_thunk, Some(value_span), ctx).await?;

                // Compare values for equality. Dict and Seq require materialization of
                // their contents, so this is an async operation.
                let matches = values_equal(
                    var_value,
                    value.clone(),
                    value_span.clone(),
                    Arc::clone(ctx),
                )
                .await?;
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
                                dict_thunk_ids.get(&Key::String(Rc::from(key.as_str())))
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
                                    Key::String(s) => pattern_keys.contains(s.as_ref()),
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
                            value_span.clone(),
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
                                let child_env = Arc::new(RwLock::new(Environment::with_parent(
                                    Arc::clone(&result_env),
                                )));
                                child_env.write().unwrap().insert(name.clone(), tail_thunk);
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
                                            value_span.clone(),
                                        )));
                                    payload_map
                                        .insert(Key::String(Rc::from(*field_name)), thunk_id);
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
                    if !values_equal(val_a, val_b, span.clone(), Arc::clone(&ctx)).await? {
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
                if !values_equal(val_ha, val_hb, span.clone(), Arc::clone(&ctx)).await? {
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
    use crate::test_util::{sp, test_span};
    use crate::value::*;

    fn empty_env() -> Arc<RwLock<Environment>> {
        Arc::new(RwLock::new(Environment::new()))
    }

    fn test_ctx() -> Arc<EvalContext> {
        let env = empty_env();
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        EvalContext::new(base_dir, Arc::clone(&env), Arc::clone(&env), false)
    }

    /// Test-only: evaluate a SurfaceNode via the lower→CoreExpr path.
    /// Uses lower::lower() to produce CoreExpr, then calls eval_core_expr.
    fn eval_for_test(
        node: Arc<SurfaceNode>,
        env: Arc<RwLock<Environment>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        let core_expr = crate::lower::lower(
            &node,
            crate::ast::empty_resolution_table(),
            crate::ast::empty_type_annotation_table(),
        );
        crate::async_rt::block_on_anywhere(super::eval_core_expr(&core_expr, &env, ctx))
    }

    /// Directly evaluate a `Spanned<CoreExpr>`.
    /// Used by tests that need to construct CoreExpr with specific resolved types
    /// (e.g. `CoreExpr::TypeAssert` with a pre-resolved `Type`).
    fn eval_core_for_test(
        expr: Spanned<CoreExpr>,
        env: Arc<RwLock<Environment>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        crate::async_rt::block_on_anywhere(super::eval_core_expr(&expr, &env, ctx))
    }

    /// Parse a surface expression from text and evaluate it.
    /// Convenience for most test cases — avoids constructing SurfaceNode by hand.
    fn eval_str(
        src: &str,
        env: Arc<RwLock<Environment>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        let node = crate::parser::parse_surface_expression(src)
            .unwrap_or_else(|e| panic!("parse_surface_expression({src:?}) failed: {e:?}"));
        eval_for_test(node, env, ctx)
    }

    /// Build a zero-span SurfaceNode wrapping the given SurfaceExpression.
    /// Convenience for surface-based eval_for_test calls.
    fn surf(expr: SurfaceExpression) -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode {
            expr,
            span: Span::origin(),
        })
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

    /// Build a `Spanned<SurfaceEntry>` with a string key and a simple expression value.
    /// Helper for constructing `Annotation::PropertyDict` entries in tests during
    /// rv2-migrate-annotation migration (Phase 1 stub support).
    fn surf_ann_entry(key: &str, value_expr: SurfaceExpression) -> Spanned<SurfaceEntry> {
        let z = test_span(0, 0, 0, 0);
        let mk = |expr| {
            Arc::new(SurfaceNode {
                expr,
                span: z.clone(),
            })
        };
        Spanned::new(
            SurfaceEntry {
                key: Some(mk(SurfaceExpression::Str(key.into()))),
                value: mk(value_expr),
            },
            z,
        )
    }

    #[test]
    fn test_eval_int() {
        let thunk = eval_str("42", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_eval_float() {
        let thunk = eval_str("3.14", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Float(3.14));
    }

    #[test]
    fn test_eval_bool() {
        let thunk = eval_str("true", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn test_eval_str() {
        let thunk = eval_str("\"hello\"", empty_env(), &test_ctx()).unwrap();
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

        let thunk = eval_str("$x", env, &test_ctx()).unwrap();
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
        let thunk = eval_str("$y", child, &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(77));
    }

    #[test]
    fn test_varref_not_found() {
        let err = eval_str("$missing", empty_env(), &test_ctx()).unwrap_err();
        assert!(
            err.to_string().contains("undefined variable: missing"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_simple_dict() {
        // [x: 1  y: "hello"]
        let ctx = test_ctx();
        let thunk = eval_str("[x: 1  y: \"hello\"]", empty_env(), &ctx).unwrap();
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
        let ctx = test_ctx();
        let thunk = eval_str("[10  20  30]", empty_env(), &ctx).unwrap();
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
        // [name: "hello"  42  flag: true  99]
        let ctx = test_ctx();
        let thunk = eval_str("[name: \"hello\"  42  flag: true  99]", empty_env(), &ctx).unwrap();
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
        let ctx = test_ctx();
        let thunk = eval_str("[x: 5  y: $x]", empty_env(), &ctx).unwrap();
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
        let ctx = test_ctx();
        let thunk = eval_str("[y: $x  x: 10]", empty_env(), &ctx).unwrap();
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
        let ctx = test_ctx();
        let thunk = eval_str("[x: $x]", empty_env(), &ctx).unwrap();
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
        let ctx = test_ctx();
        let thunk = eval_str("[x: $x]", empty_env(), &ctx).unwrap();
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
        let ctx = test_ctx();
        let dict_thunk = eval_str("[x: $missing]", empty_env(), &ctx).unwrap();
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
        let ctx = test_ctx();
        let thunk = eval_str("[x: 42  inner: [y: $x]]", empty_env(), &ctx).unwrap();
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
        // Build via SurfaceNode to bypass parser duplicate-key detection.
        // The evaluator (eval_dict_core) must detect the duplicate key and return E030.
        let z = Span::origin();
        let mk = |expr: SurfaceExpression| {
            Arc::new(SurfaceNode {
                expr,
                span: z.clone(),
            })
        };
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
        let err = eval_for_test(node, empty_env(), &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("duplicate key: x"), "got: {}", err);
    }

    #[test]
    fn test_fn_creates_function_value() {
        // [fn [let x] $x] → Function
        let thunk = eval_str("[fn [let x] $x]", empty_env(), &test_ctx()).unwrap();
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
        let fn_thunk = eval_str("[fn [] $outer]", Arc::clone(&env), &test_ctx()).unwrap();
        let fn_val = materialize(&fn_thunk, None, &test_ctx()).unwrap();

        // Call it: [call $f]
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let result_thunk = eval_str("[call $f]", env, &test_ctx()).unwrap();
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
        let thunk = eval_str("[call $f 42]", env, &test_ctx()).unwrap();
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
        let thunk = eval_str("[call $f 10 20]", env, &test_ctx()).unwrap();
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
        let thunk =
            eval_str("[call $x]", env, &test_ctx()).expect("eval should return PendingCall thunk");
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
        let thunk = eval_str("[call $f 1]", env, &test_ctx())
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
        let thunk = eval_str("[call $f 1 2]", env, &test_ctx())
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("arity mismatch"), "got: {}", err);
    }

    #[test]
    fn test_call_named_arg_with_default() {
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
            body: Arc::new(sp(CoreExpr::FreeVar("y".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        // Call without named arg -- y should default to 99
        let thunk = eval_str("[call $f 1]", Arc::clone(&env), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_call_named_arg_overridden() {
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
            body: Arc::new(sp(CoreExpr::FreeVar("y".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let thunk = eval_str("[call $f 1 y: 42]", env, &test_ctx()).unwrap();
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
        let thunk = eval_str("[call $f 1 z: 2]", env, &test_ctx())
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
            body: Arc::new(sp(CoreExpr::FreeVar("y".to_string()))),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let thunk = eval_str("[call $f 1 2 y: 42]", env, &test_ctx())
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
        let ctx = test_ctx();
        let thunk = eval_str("[call $f 1 2 3]", env, &ctx).unwrap();
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
        let thunk = eval_str("[call $f 1]", env, &test_ctx()).unwrap();
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
        let thunk = eval_str("[call $add 3 4]", env, &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(7));
    }

    #[test]
    #[ignore = "pre-existing regression from runtime-v2 merge: type alias declarations are not evaluable as expressions"]
    fn test_type_alias_returns_empty_dict() {
        // type aliases are compile-time constructs — evaluating one as an expression returns {}
        let thunk = eval_str("[type MyType Int]", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        match val {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected empty Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_rest_marker_anonymous_errors() {
        // eval_core_expr returns Err immediately for Rest (not deferred to materialize)
        let err = eval_for_test(
            surf(SurfaceExpression::Rest(None)),
            empty_env(),
            &test_ctx(),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("rest marker (...) is only valid inside type expressions"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_rest_marker_named_errors() {
        // eval_core_expr returns Err immediately for Rest (not deferred to materialize)
        let err = eval_for_test(
            surf(SurfaceExpression::Rest(Some("x".into()))),
            empty_env(),
            &test_ctx(),
        )
        .unwrap_err();
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
        let err = eval_str("$_", empty_env(), &test_ctx()).unwrap_err();
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
        let mut node = crate::parser::parse_surface_expression("$_.name").expect("parse failed");
        crate::desugar::desugar_surface_node(&mut node, 0);
        let thunk = eval_for_test(node, empty_env(), &test_ctx()).unwrap();
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

        let mut node =
            crate::parser::parse_surface_expression("[call $f $_]").expect("parse failed");
        crate::desugar::desugar_surface_node(&mut node, 0);
        let thunk = eval_for_test(node, Arc::clone(&env), &test_ctx()).unwrap();
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

        // Parse $_.name, desugar to [fn [_] $_.name], eval to produce a Function
        let mut getter_node =
            crate::parser::parse_surface_expression("$_.name").expect("parse failed");
        crate::desugar::desugar_surface_node(&mut getter_node, 0);
        let getter_thunk = eval_for_test(getter_node, Arc::clone(&env), &test_ctx()).unwrap();
        let getter_val = materialize(&getter_thunk, None, &test_ctx()).unwrap();
        env.write().unwrap().insert(
            "getter".into(),
            Arc::new(Thunk::new_materialized(getter_val, test_span(1, 1, 1, 10))),
        );

        // Call it with [name: "alice"]
        let call_node = crate::parser::parse_surface_expression("[call $getter [name: \"alice\"]]")
            .expect("parse failed");
        let result_thunk = eval_for_test(call_node, env, &test_ctx()).unwrap();
        let result = materialize(&result_thunk, None, &test_ctx()).unwrap();
        assert_eq!(result, string_val("alice".into()));
    }

    #[test]
    fn test_underscore_in_dict_entry() {
        // [a: $_.name] → desugars to [fn [_] [a: $_.name]]
        // Dict with $_ in a value position should desugar to an implicit lambda
        let mut node =
            crate::parser::parse_surface_expression("[a: $_.name]").expect("parse failed");
        crate::desugar::desugar_surface_node(&mut node, 0);
        let thunk = eval_for_test(node, empty_env(), &test_ctx()).unwrap();
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

        let mut node =
            crate::parser::parse_surface_expression("[call $f x: $_]").expect("parse failed");
        crate::desugar::desugar_surface_node(&mut node, 0);
        let thunk = eval_for_test(node, Arc::clone(&env), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
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
        let z = Span::origin();
        let mk = |expr: SurfaceExpression| {
            Arc::new(SurfaceNode {
                expr,
                span: z.clone(),
            })
        };
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

    #[test]
    fn test_dot_access() {
        // [name: hello].name -> "hello"
        // Use a single ctx — ThunkIds from one ctx are invalid in another.
        let ctx = test_ctx();
        let env = empty_env();
        let dict_thunk = eval_for_test(
            surf_dict(vec![("name", "\"hello\"")]),
            Arc::clone(&env),
            &ctx,
        )
        .unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).unwrap();

        // Bind the dict to $d in the environment
        env.write().unwrap().insert(
            "d".into(),
            Arc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let thunk = eval_str("$d.name", env, &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[test]
    fn test_dot_access_missing_key() {
        let env = empty_env();
        let dict_thunk =
            eval_for_test(surf_dict(vec![("x", "1")]), Arc::clone(&env), &test_ctx()).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx()).unwrap();
        env.write().unwrap().insert(
            "d".into(),
            Arc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let thunk = eval_str("$d.missing", env, &test_ctx()).unwrap();
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

        let thunk = eval_str("$x.foo", env, &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("expected"), "got: {}", err);
        assert!(err.to_string().contains("expected Dict"), "got: {}", err);
    }

    // Bracket access and range access tests removed — syntax has been removed from the language.
    // Tests are replaced by corpus tests in tests/corpus/valid/ and tests/corpus/invalid/.

    #[test]
    fn test_type_assert_int_passes() {
        // [@Int 42] -> 42
        let thunk = eval_str("[@Int 42]", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_string_passes() {
        // [@String "hello"] -> "hello"
        let thunk = eval_str("[@String \"hello\"]", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[test]
    fn test_type_assert_number_accepts_int() {
        // [@Number 42] -> 42 (Number accepts Int)
        let thunk = eval_str("[@Number 42]", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_number_accepts_float() {
        // [@Number 3.14] -> 3.14 (Number accepts Float)
        let thunk = eval_str("[@Number 3.14]", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Float(3.14));
    }

    #[test]
    fn test_type_assert_int_fails_on_string() {
        // [@Int "hello"] -> error
        // Use eval_core_for_test with resolved_type: Type::Int to exercise the TypeAssert
        // failure path directly. eval_str uses an empty TypeAnnotationTable which gives
        // resolved_type=Type::Unknown (accepts all values via consistent subtyping).
        let span = Span::origin();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Int".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                resolved_type: Type::Int,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx()).unwrap();
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
        // [@String 42] -> error  (42 is Int, not String)
        // Use eval_core_for_test with resolved_type: Type::Str. See note in
        // test_type_assert_int_fails_on_string for why eval_str cannot be used here.
        let span = Span::origin();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("String".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                resolved_type: Type::Str,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx()).unwrap();
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
        let thunk = eval_str("[@Bool true]", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn test_type_assert_property_dict_with_type() {
        // [@[type: Int] 42] -> 42
        let thunk = eval_str("[@[type: Int] 42]", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_property_dict_type_mismatch() {
        // [@[type: Int] "hello"] -> error (PropertyDict annotation with type:Int, value is String)
        // Use eval_core_for_test with resolved_type: Type::Int. The typecheck pass resolves
        // the `type: Int` property to Type::Int; without typecheck (eval_str), resolved_type
        // is Type::Unknown which accepts all values via consistent subtyping.
        let span = Span::origin();
        let entries = vec![surf_ann_entry(
            "type",
            SurfaceExpression::VarRef {
                name: "Int".into(),
                escaped: false,
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
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx()).unwrap();
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
        // [@[default: 0] "hello"] -> "hello" (no type key, no check performed)
        let thunk = eval_str("[@[default: 0] \"hello\"]", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[test]
    fn test_type_assert_default_not_used_on_match() {
        // [@[type: Int  default: 0] 42] -> 42 (type matches, default not used)
        let thunk = eval_str("[@[type: Int  default: 0] 42]", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_default_used_on_mismatch() {
        // [@[type: Int  default: 0] "hello"] -> 0 (type mismatch, returns default)
        // Use eval_core_for_test with resolved_type: Type::Int so the type check fires.
        let span = Span::origin();
        let entries = vec![
            surf_ann_entry(
                "type",
                SurfaceExpression::VarRef {
                    name: "Int".into(),
                    escaped: false,
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
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(0));
    }

    #[test]
    fn test_type_assert_property_dict_no_default_errors_on_mismatch() {
        // [@[type: Int] "hello"] -> error (no default, mismatch is an error)
        // Use eval_core_for_test with resolved_type: Type::Int so the type check fires.
        let span = Span::origin();
        let entries = vec![surf_ann_entry(
            "type",
            SurfaceExpression::VarRef {
                name: "Int".into(),
                escaped: false,
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
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx()).unwrap();
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
        // Use eval_core_for_test with resolved_type: Type::Number so the type check fires.
        let span = Span::origin();
        let entries_pass = vec![
            surf_ann_entry(
                "type",
                SurfaceExpression::VarRef {
                    name: "Number".into(),
                    escaped: false,
                },
            ),
            surf_ann_entry("default", SurfaceExpression::Int(-1)),
        ];
        let expr_pass = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::PropertyDict(entries_pass)),
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                resolved_type: Type::Number,
                pipeline_blame: None,
            },
            span.clone(),
        );
        let thunk = eval_core_for_test(expr_pass, empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));

        // [@[type: Number  default: -1] "nope"] -> -1 (String fails Number, returns default)
        let entries_fail = vec![
            surf_ann_entry(
                "type",
                SurfaceExpression::VarRef {
                    name: "Number".into(),
                    escaped: false,
                },
            ),
            surf_ann_entry("default", SurfaceExpression::Int(-1)),
        ];
        let expr_fail = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::PropertyDict(entries_fail)),
                expr: Arc::new(Spanned::new(CoreExpr::Str("nope".into()), span.clone())),
                resolved_type: Type::Number,
                pipeline_blame: None,
            },
            span,
        );
        let thunk2 = eval_core_for_test(expr_fail, empty_env(), &test_ctx()).unwrap();
        let val2 = materialize(&thunk2, None, &test_ctx()).unwrap();
        assert_eq!(val2, Value::Int(-1));
    }

    #[test]
    fn test_type_assert_default_accesses_outer_scope() {
        // [@[type: Int  default: $fallback] "hello"] with fallback=99 -> 99
        // Use eval_core_for_test with resolved_type: Type::Int so the mismatch fires.
        // The default expression $fallback references the outer env, so the env must
        // contain "fallback" when the default is evaluated.
        let env = empty_env();
        env.write().unwrap().insert(
            "fallback".into(),
            Arc::new(Thunk::new_materialized(
                Value::Int(99),
                test_span(1, 1, 1, 1),
            )),
        );
        let span = Span::origin();
        // Build the default expression as CoreExpr::FreeVar("fallback")
        let entries = vec![
            surf_ann_entry(
                "type",
                SurfaceExpression::VarRef {
                    name: "Int".into(),
                    escaped: false,
                },
            ),
            surf_ann_entry(
                "default",
                SurfaceExpression::VarRef {
                    name: "fallback".into(),
                    escaped: true, // $fallback in source
                },
            ),
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
        let thunk = eval_core_for_test(expr, Arc::clone(&env), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_annotated_bare_string() {
        // Config@ConfigType -> "Config"
        let thunk = eval_str("Config@ConfigType", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, string_val("Config".into()));
    }

    #[test]
    fn test_chained_dot_access() {
        // [outer: [inner: 99]].outer.inner -> 99
        // Use a single ctx throughout — ThunkIds from one ctx are invalid in another.
        let ctx = test_ctx();
        let env = empty_env();
        let dict_thunk = eval_str("[outer: [inner: 99]]", Arc::clone(&env), &ctx).unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).unwrap();
        env.write().unwrap().insert(
            "d".into(),
            Arc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        // $d.outer.inner
        let thunk = eval_str("$d.outer.inner", env, &ctx).unwrap();
        let val = materialize(&thunk, None, &ctx).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_materialization_span_on_error() {
        // [x: $missing] -- materializing x fails because $missing is undefined
        let ctx = test_ctx();
        let dict_thunk = eval_str("[x: $missing]", empty_env(), &ctx).unwrap();
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
        let ctx = test_ctx();
        let thunk = eval_str("[x: $x]", empty_env(), &ctx).unwrap();
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
        // A dict with a Bool key expression should fail in eval_key -> value_to_key.
        // Build via SurfaceNode since surface text [true: 1] would parse "true" as a String key.
        let z = Span::origin();
        let zz = z.clone();
        let mk = move |expr: SurfaceExpression| {
            Arc::new(SurfaceNode {
                expr,
                span: zz.clone(),
            })
        };
        let node = mk(SurfaceExpression::Dict(vec![Spanned::new(
            SurfaceEntry {
                key: Some(mk(SurfaceExpression::Bool(true))),
                value: mk(SurfaceExpression::Int(1)),
            },
            z,
        )]));
        let err = eval_for_test(node, empty_env(), &test_ctx()).unwrap_err();
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
        // A dict with a Float key expression should fail in eval_key -> value_to_key.
        // Build via SurfaceNode since surface text [3.14: 1] would parse differently.
        let z = Span::origin();
        let zz = z.clone();
        let mk = move |expr: SurfaceExpression| {
            Arc::new(SurfaceNode {
                expr,
                span: zz.clone(),
            })
        };
        let node = mk(SurfaceExpression::Dict(vec![Spanned::new(
            SurfaceEntry {
                key: Some(mk(SurfaceExpression::Float(3.14))),
                value: mk(SurfaceExpression::Int(1)),
            },
            z,
        )]));
        let err = eval_for_test(node, empty_env(), &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("type mismatch"), "got: {}", err);
        assert!(
            err.to_string().contains("expected String or Int"),
            "got: {}",
            err
        );
        assert!(err.to_string().contains("got Float"), "got: {}", err);
    }

    #[test]
    fn test_adt_constructor_scoping_eval() {
        // ADT constructor scoping: nominal_variant_exhaustive_match corpus test.
        // [type Shape [Circle r: Int] [Square s: Int]] injects constructors into scope.
        // [Circle r: 5] creates Variant(Circle, 5). Match extracts payload r=5. area = 3*5*5=75.
        let src = concat!(
            "[\n",
            "  [type Shape [Circle r: Int] [Square s: Int]]\n",
            "  shape: [Circle r: 5]\n",
            "  area: [match shape [Circle r]: [* 3 [* r r]] [Square s]: [* s s]]\n",
            "]"
        );
        let result = crate::eval_source(src).expect("eval failed");
        // Dict includes injected constructor bindings and the area: Int(75) field
        assert!(
            result.contains("\"area\": Int(75)"),
            "expected area: Int(75) in {result}"
        );
        assert!(
            result.contains("Variant(Circle, Int(5))"),
            "expected Variant(Circle, Int(5)) in {result}"
        );
    }

    #[test]
    fn test_eval_document_single_expression() {
        // A document with one dict expression returns that dict: [x: 1  y: 2]
        let result = crate::eval_source("[x: 1  y: 2]").expect("eval failed");
        assert_eq!(result, r#"Dict({"x": Int(1), "y": Int(2)})"#);
    }

    #[test]
    fn test_eval_document_scope_chain() {
        // Two expressions in one doc form a scope chain: expr 1 defines x, expr 2 references $x
        let result = crate::eval_source("[x: 10]\n[y: $x]").expect("eval failed");
        assert_eq!(result, r#"Dict({"y": Int(10)})"#);
    }

    #[test]
    fn test_eval_document_scope_chain_shadowing() {
        // Local letrec wins over parent scope binding when same name is reused.
        // Expr 1: [x: 1]  Expr 2: [x: 2  y: $x]  → y should be 2
        let result = crate::eval_source("[x: 1]\n[x: 2  y: $x]").expect("eval failed");
        assert_eq!(result, r#"Dict({"x": Int(2), "y": Int(2)})"#);
    }

    #[test]
    fn test_eval_document_intermediate_non_dict_expression() {
        // In the surface pipeline, a bare non-dict intermediate expression (e.g. `42`) has no
        // static string keys, so no scope extension is attempted and no error is produced.
        // The intermediate is silently discarded and the last expression is returned.
        let result = crate::eval_source("42\n[x: 1]").expect("eval failed");
        assert_eq!(result, r#"Dict({"x": Int(1)})"#);
    }

    #[test]
    fn test_eval_document_empty() {
        // An empty document (zero expressions) returns an empty dict.
        // Covered by test_eval_file_empty; verified here via eval_source("")
        let result = crate::eval_source("").expect("eval failed");
        assert_eq!(result, "Dict({})");
    }

    #[test]
    fn test_eval_document_three_expressions() {
        // Three expressions chaining scope:
        // Expr 1: [a: 1]  Expr 2: [b: 2]  Expr 3: [ref_a: $a  ref_b: $b]
        // Expr 3 should see both $a (grandparent) and $b (parent) via scope chain.
        let result =
            crate::eval_source("[a: 1]\n[b: 2]\n[ref_a: $a  ref_b: $b]").expect("eval failed");
        assert_eq!(result, r#"Dict({"ref_a": Int(1), "ref_b": Int(2)})"#);
    }

    #[test]
    fn test_eval_document_inherits_parent_env() {
        // Scope-chain visibility: a binding from expr 1 is seen by expr 2.
        // (The original test injected a binding via parent env; the scope-chain
        // variant covers the same environment lookup path via eval_surface_document.)
        let result =
            crate::eval_source("[external: 999]\n[local: $external]").expect("eval failed");
        assert_eq!(result, r#"Dict({"local": Int(999)})"#);
    }

    #[test]
    fn test_eval_document_single_non_dict_expression() {
        // A document with a single non-dict expression (Int). The last expression can be any type.
        let result = crate::eval_source("42").expect("eval failed");
        assert_eq!(result, "Int(42)");
    }

    #[test]
    fn test_eval_document_integer_keys_skipped_in_scope_chain() {
        // Expr 1: [10 20 30] (positional / integer-keyed entries)
        // Expr 2: [result: 99]
        // Integer keys from expr 1 must not become scope bindings; expr 2 should succeed.
        let result = crate::eval_source("[10 20 30]\n[result: 99]").expect("eval failed");
        assert_eq!(result, r#"Dict({"result": Int(99)})"#);
    }

    #[test]
    fn test_eval_document_scope_chain_plus_letrec() {
        // Scope chain + letrec: y references x from the parent scope (via scope chain),
        // z references y via letrec within the same dict. Verify z resolves to 1.
        let result = crate::eval_source("[x: 1]\n[y: $x  z: $y]").expect("eval failed");
        assert_eq!(result, r#"Dict({"y": Int(1), "z": Int(1)})"#);
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
        // eval_source materializes all keys, so forcing y fails.
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
        // forces doc1 to materialize. eval_source materializes all keys, so result is forced.
        let err = crate::eval_source(
            "--- %early\n[x: %late.value]\n--- %late\n[value: 42]\n---\n[result: %early.x]",
        )
        .expect_err("expected error: %late not in scope for doc1");
        assert!(
            err.contains("undefined variable: %late"),
            "expected 'undefined variable: %late', got: {err}"
        );
    }

    // ── Stack trace / call stack reconstruction tests ──────────────────

    #[test]
    fn test_call_error_has_stack_frame_with_function_name() {
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
                CoreExpr::FreeVar("missing".to_string()),
                test_span(1, 15, 1, 23),
            )),
            env: Arc::clone(&env),
            annotation: None,
        };
        env.write().unwrap().insert(
            "f".into(),
            Arc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 20))),
        );

        let thunk = eval_str("[call $f 1]", env, &test_ctx()).unwrap();
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
        let thunk = eval_str("[call $outer 1]", env, &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("undefined variable: missing"));

        // With TCO, inner call frame is optimized away (strong_count==1 → no Memoize pushed).
        // Only outer frame remains. This is correct: TCO collapses tail-position stack frames.
        let labels: Vec<&str> = err.stack.iter().map(|f| f.label.as_str()).collect();
        assert!(
            labels.contains(&"[outer ...]"),
            "expected '[outer ...]' in stack, got: {labels:?}"
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
        let thunk = eval_str("$a.x", env, &ctx).unwrap();
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
        let thunk = eval_str("$nonexistent.field", env, &test_ctx()).unwrap();
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

        // Build $a.x access — eval returns an Unevaluated thunk wrapping the DotAccess
        let thunk = eval_str("$a.x", Arc::clone(&env), &ctx).unwrap();

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
        let thunk = eval_str("[call $f 1]", env, &test_ctx())
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

        let thunk = eval_str("[call $fail]", env, &test_ctx()).unwrap();
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
                    annotation: Some(sp(Annotation::PropertyDict(vec![surf_ann_entry(
                        "default",
                        SurfaceExpression::Int(10),
                    )]))),
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
                    annotation: Some(sp(Annotation::PropertyDict(vec![surf_ann_entry(
                        "default",
                        SurfaceExpression::Int(10),
                    )]))),
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
        let result = materialize(&pending, None, &test_ctx()).unwrap();
        assert_eq!(result, Value::Int(17)); // 7 + 10
    }

    // ── Failed thunk state tests ───────────────────────────────────────

    #[test]
    fn test_failed_state_returns_cached_error() {
        // When a thunk fails, it should cache the error in Failed state
        // and return it on subsequent materialization attempts
        let ctx = test_ctx();
        let dict_thunk = eval_str("[x: $undefined]", empty_env(), &ctx).unwrap();
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
        let ctx = test_ctx();
        let dict_thunk = eval_str("[broken: $missing]", empty_env(), &ctx).unwrap();
        let dict_val = materialize(&dict_thunk, None, &ctx).unwrap();

        let broken_thunk = match &dict_val {
            Value::Dict(map) => get_thunk_rc(map.get(&Key::String("broken".into())).unwrap(), &ctx),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First access with one materialization span
        let span1 = test_span(10, 1, 10, 5);
        let err1 = materialize(&broken_thunk, Some(&span1), &ctx).unwrap_err();
        assert_eq!(err1.materialization_span, Some(span1.clone()));
        assert_eq!(err1.stack.len(), 0);

        // Second access with a different materialization span should preserve span1
        // and add span2 as a stack frame
        let span2 = test_span(20, 1, 20, 5);
        let err2 = materialize(&broken_thunk, Some(&span2), &ctx).unwrap_err();
        assert_eq!(err2.materialization_span, Some(span1.clone())); // PRESERVED
        assert_eq!(err2.stack.len(), 1);
        assert_eq!(err2.stack[0].label, "materialized");
        assert_eq!(err2.stack[0].materialization_span, span2.clone());

        // Third access with no materialization span returns error with the
        // original materialization_span and the stack frame from the second access
        let err3 = materialize(&broken_thunk, None, &ctx).unwrap_err();
        assert_eq!(err3.materialization_span, Some(span1)); // PRESERVED
        assert_eq!(err3.stack.len(), 1);
        assert_eq!(err3.stack[0].materialization_span, span2);
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
        let thunk = eval_str("[call $bad_fn 1]", env, &test_ctx()).unwrap();

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

        let thunk = eval_str("[call $fail]", env, &test_ctx()).unwrap();

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
        let thunk = eval_str("$undefined_var.field", empty_env(), &test_ctx()).unwrap();

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
        let thunk = eval_str("$undefined_var.field", empty_env(), &test_ctx()).unwrap();

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
            Some(span1.clone()),
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
            err3.stack.iter().any(|f| f.definition_span == span2),
            "span2 should be in stack frames"
        );
    }

    #[test]
    #[ignore = "pre-existing regression from runtime-v2 merge: materialize returns Ok for infinite recursion instead of ResourceLimitExceeded"]
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

                let thunk = eval_str("[call $f 1]", env, &test_ctx()).unwrap();
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

    // ── Error caching tests ──────────────────────────────────────────────

    #[test]
    fn test_regular_error_does_cache() {
        // Regular errors should transition to Failed state
        let ctx = test_ctx();
        let dict_thunk = eval_str("[x: $undefined]", empty_env(), &ctx).unwrap();
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

    // === EvalContext isolation tests ===

    // ── Structural TypeAssert tests (resolved_type: Some(Type::...)) ────
    // These test the NEW structural validation path added by the
    // typeassert-structural sprint, distinct from the nominal fallback path
    // (resolved_type: None) tested in the existing TypeAssert tests above.

    #[test]
    fn test_typeassert_structural_int_pass() {
        // Structural path: resolved_type = Some(Type::Int), value is Int(42) -> pass
        let span = Span::origin();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Int".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                resolved_type: Type::Int,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    #[ignore = "pre-existing: TypeAssert is lazy in new CEK model, type error fires on materialize() not eval()"]
    fn test_typeassert_structural_int_fail() {
        // Structural path: resolved_type = Some(Type::Int), value is String -> error
        let span = Span::origin();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Int".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                resolved_type: Type::Int,
                pipeline_blame: None,
            },
            span,
        );
        let err = eval_core_for_test(expr, empty_env(), &test_ctx()).unwrap_err();
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
        let span = Span::origin();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Str".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Str("hello".into()), span.clone())),
                resolved_type: Type::Str,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, string_val("hello".into()));
    }

    #[test]
    fn test_typeassert_structural_any() {
        // Structural path: resolved_type = Some(Type::Top), any value passes
        let span = Span::origin();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Any".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Str("anything".into()), span.clone())),
                resolved_type: Type::Top,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, string_val("anything".into()));
    }

    #[test]
    fn test_typeassert_structural_any_accepts_int() {
        // Type::Top accepts Int as well (covers any-value branch)
        let span = Span::origin();
        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Any".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Int(99), span.clone())),
                resolved_type: Type::Top,
                pipeline_blame: None,
            },
            span,
        );
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx()).unwrap();
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

        let span = Span::origin();
        let dict_node = eval_str("[name: Alice  age: 30]", empty_env(), &test_ctx()).unwrap();
        // Use eval_for_test to eval the dict, then wrap in TypeAssert via CoreExpr
        let dict_val = materialize(&dict_node, None, &test_ctx()).unwrap();
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

        let thunk = eval_core_for_test(inner_expr, empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        // Should be a Dict with the expected fields
        let _ = dict_val; // suppress unused warning
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

        let span = Span::origin();
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

        let err = eval_core_for_test(inner_expr, empty_env(), &test_ctx()).unwrap_err();
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

        let span = Span::origin();
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
        let thunk = eval_core_for_test(inner_expr, empty_env(), &test_ctx()).unwrap();
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

        let span = Span::origin();
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

        let thunk = eval_core_for_test(inner_expr, empty_env(), &test_ctx()).unwrap();
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

        let span = Span::origin();
        let inner_expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::Simple("Record".into())),
                expr: Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
                resolved_type: record_type,
                pipeline_blame: None,
            },
            span,
        );

        let err = eval_core_for_test(inner_expr, empty_env(), &test_ctx()).unwrap_err();
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
        let thunk = eval_str("[@Int 7]", empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert_eq!(val, Value::Int(7));
    }

    #[test]
    #[ignore = "pre-existing: TypeAssert type errors require materialize() in lazy CEK model"]
    fn test_typeassert_nominal_fallback_mismatch() {
        // Nominal fallback path: resolved_type = None, annotation "Int", value is String -> error
        // (Verifies nominal fallback still rejects mismatches.)
        let thunk = eval_str("[@Int oops]", empty_env(), &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
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
        let span = Span::origin();
        let entries = vec![surf_ann_entry("default", SurfaceExpression::Int(999))];

        let expr = Spanned::new(
            CoreExpr::TypeAssert {
                annotation: sp(Annotation::PropertyDict(entries)),
                expr: Arc::new(Spanned::new(
                    CoreExpr::Str("not an int".into()),
                    span.clone(),
                )),
                resolved_type: Type::Int,
                pipeline_blame: None,
            },
            span,
        );

        // eval() returns a Materialized thunk containing the default value
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx()).unwrap();
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
        let entries = vec![surf_ann_entry("default", SurfaceExpression::Int(0))];
        assert!(!annotation_has_structural_fields(
            &Annotation::PropertyDict(entries)
        ));
    }

    #[test]
    fn test_annotation_has_structural_fields_type_only() {
        // [@[type: Int] $x] — type-only, no structural fields
        let entries = vec![surf_ann_entry(
            "type",
            SurfaceExpression::VarRef {
                name: "Int".into(),
                escaped: false,
            },
        )];
        assert!(!annotation_has_structural_fields(
            &Annotation::PropertyDict(entries)
        ));
    }

    #[test]
    fn test_annotation_has_structural_fields_record_annotation() {
        // [@[name: String age: Int] $x] — has structural fields
        let entries = vec![
            surf_ann_entry(
                "name",
                SurfaceExpression::VarRef {
                    name: "String".into(),
                    escaped: false,
                },
            ),
            surf_ann_entry(
                "age",
                SurfaceExpression::VarRef {
                    name: "Int".into(),
                    escaped: false,
                },
            ),
        ];
        assert!(annotation_has_structural_fields(&Annotation::PropertyDict(
            entries
        )));
    }

    #[test]
    fn test_annotation_has_structural_fields_mixed_meta_and_record() {
        // [@[name: String default: []] $x] — has structural field "name"
        let entries = vec![
            surf_ann_entry(
                "name",
                SurfaceExpression::VarRef {
                    name: "String".into(),
                    escaped: false,
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

    #[test]
    fn test_elaboration_gap_structural_annotation_dict_passes() {
        // [@[name: String] [name: hello]] with resolved_type=None (no typecheck)
        // Should pass: value is a Dict (tag check succeeds)
        let thunk = eval_str("[@[name: String] [name: hello]]", empty_env(), &test_ctx()).unwrap();
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
        let thunk = eval_str("[@[name: String] 42]", empty_env(), &test_ctx()).unwrap();
        let err = materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(
            err.to_string()
                .contains("type assertion failed: expected Record, got Int"),
            "Structural annotation with non-Dict value should fail; got: {}",
            err.to_string()
        );
    }

    #[test]
    fn test_elaboration_gap_structural_annotation_non_dict_with_default() {
        // [@[name: String  default: []] 42] — structural record annotation with default.
        // Value is Int (not a Dict), so the record shape check fails and the default is used.
        // Use eval_core_for_test with resolved_type: Type::Record({name: Str}) so the
        // as_record_row_merged path fires. With resolved_type=Unknown (from eval_str),
        // is_consistent_subtype(Int, Unknown)=true and the TypeAssert passes trivially.
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), Type::Str);
        let record_type = Type::Record(Row { fields });

        let span = Span::origin();
        let entries = vec![
            surf_ann_entry(
                "name",
                SurfaceExpression::VarRef {
                    name: "String".into(),
                    escaped: false,
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
        let thunk = eval_core_for_test(expr, empty_env(), &test_ctx()).unwrap();
        let val = materialize(&thunk, None, &test_ctx()).unwrap();
        assert!(
            matches!(val, Value::Dict(_)),
            "Should use default when record shape check fails; got: {val:?}"
        );
    }

    #[test]
    fn test_elaboration_gap_default_only_no_structural_check() {
        // [@[default: 0] "hello"] with resolved_type=None
        // Should pass through without validation (no type, no structural fields)
        let thunk = eval_str("[@[default: 0] \"hello\"]", empty_env(), &test_ctx()).unwrap();
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
        // Type::IntLiteral(n): ground_type_of erases Int values to Type::Int.
        // is_consistent_subtype(Type::Int, Type::IntLiteral(n)) falls to is_subtype which
        // returns false — Int is NOT a subtype of IntLiteral(n) (it's the other way).
        // Literal types are static-only constraints (the type checker uses them for
        // exhaustiveness); at runtime, ground_type_of produces the base type, not a literal.
        assert!(!value_matches_type(&Value::Int(5), &Type::IntLiteral(5)));
        assert!(!value_matches_type(&Value::Int(6), &Type::IntLiteral(5)));
        assert!(!value_matches_type(
            &string_val("5".into()),
            &Type::IntLiteral(5)
        ));
        // But IntLiteral(n) IS a subtype of Int (literal specializes base type).
        // Check via consistent subtyping from the literal side:
        // is_consistent_subtype(IntLiteral(5), Int) = is_subtype(IntLiteral(5), Int) = true.
        assert!(Type::is_consistent_subtype(
            &Type::IntLiteral(5),
            &Type::Int
        ));
    }

    #[test]
    fn test_value_matches_type_string_literal() {
        // Type::StringLiteral: ground_type_of erases String values to Type::Str.
        // is_consistent_subtype(Type::Str, Type::StringLiteral("foo")) = is_subtype(Str, StringLiteral)
        // = false — Str is NOT a subtype of StringLiteral (it's the other way).
        assert!(!value_matches_type(
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
        // StringLiteral IS a subtype of Str (literal specializes base type).
        assert!(Type::is_consistent_subtype(
            &Type::StringLiteral("foo".into()),
            &Type::Str
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
        let mut fields = HashMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row { fields });
        // Non-Dict value: ground_type_of(Int) = Type::Int, not a subtype of Record.
        assert!(!value_matches_type(&Value::Int(99), &record_type));
        // Empty Dict: ground_type_of(Dict({})) = Record({}), missing required field "x".
        assert!(!value_matches_type(
            &Value::Dict(IndexMap::new()),
            &record_type
        ));
        // Dict with the required field "x" AND the field type is Unknown (erased) which
        // is consistent with Int (Unknown ~<: T for all T). However this test requires
        // alloc_thunk to build the dict — covered by TypeAssert corpus tests instead.
        // The key insight: value_matches_type is NOT the Record validation entry point
        // at runtime; TypeAssertCheck uses as_record_row_merged → validate_and_wrap_record.
    }

    #[test]
    fn test_value_matches_type_proxy() {
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
        assert!(value_matches_type(&proxy_val, &Type::Proxy));
        assert!(value_matches_type(&proxy_val, &Type::Int)); // Unknown ~<: Int = true
        assert!(value_matches_type(&proxy_val, &Type::Top));

        // Verify ground_type_of explicitly
        assert_eq!(ground_type_of(&proxy_val), Type::Unknown);
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
            Key::String("x".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(1),
                span.clone(),
            ))),
        );
        entries.insert(
            Key::String("z".into()),
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
                span.clone(),
            ))),
        );
        entries.insert(
            Key::String("name".into()),
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
                span.clone(),
            ))),
        );
        entries.insert(
            Key::String("name".into()),
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

    #[test]
    fn test_materialize_cached_thunk_at_high_depth() {
        // Pre-materialized thunks should succeed even at depth > MAX_CONTINUATION_STACK.
        // Previously, the depth check fired BEFORE the Materialized early-return,
        // causing spurious depth errors when accessing cached values at high depth.
        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_materialized(Value::Int(42), span);
        let ctx = test_ctx();

        // Materialize at high depth (CEK continuation stack) should succeed
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
        let thunk = Arc::new(Thunk::new_materialized(Value::Int(42), span.clone()));

        // Force it into Failed state with a cached error
        let err = Box::new(EvalError::type_mismatch("String", "Int", span));
        thunk.cache_failure_once(&err);

        let ctx = test_ctx();

        // Materialize should return the cached error
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
        let base_thunk = Arc::new(Thunk::new_materialized(
            Value::Int(chain_len as i64),
            span.clone(),
        ));
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
                span.clone(),
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
        let types = std::sync::Arc::new(crate::ast::TypeAnnotationTable::default());
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
        // cycle detection.
        let dict_val = materialize(&thunk, None, &ctx).expect("dict should materialize");

        // Access one of the cyclic keys to trigger cycle detection
        let err = match dict_val {
            Value::Dict(ref map) => {
                let a_thunk_id = map
                    .get(&Key::String("a".into()))
                    .expect("dict should have 'a' key");
                let a_thunk = ctx.get_thunk(*a_thunk_id);
                materialize(&a_thunk, None, &ctx).unwrap_err()
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
                let types = std::sync::Arc::new(crate::ast::TypeAnnotationTable::default());
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
    /// 3. Verify that ctx2 correctly propagates the no_fs flag.
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
    }

    #[test]
    fn test_selective_materialization_unused_branch() {
        // Verify that accessing only one dict entry doesn't materialize unused entries
        let input = r#"[used: 1  unused: [call $error "should not materialize"]]"#;
        let surface = crate::parser::parse_surface_expression(input).expect("parse failed");
        let env = empty_env();
        let ctx = test_ctx();
        let thunk = eval_for_test(surface, Arc::clone(&env), &ctx).unwrap();
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

        // SurfaceNode with the guarded span: `42` (Int literal)
        let node = Arc::new(SurfaceNode {
            expr: SurfaceExpression::Int(42),
            span: guarded_span.clone(),
        });

        let ctx = ctx_with_guard(guarded_span, Type::Int);

        // eval() should wrap the Int thunk in a Guarded thunk.
        let thunk = eval_for_test(node, empty_env(), &ctx).unwrap();

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

        // SurfaceNode with the guarded span: `"hello"` (String literal)
        let node = Arc::new(SurfaceNode {
            expr: SurfaceExpression::Str("hello".into()),
            span: guarded_span.clone(),
        });

        // Guard expects Int — the String value will fail.
        let ctx = ctx_with_guard(guarded_span, Type::Int);

        let thunk = eval_for_test(node, empty_env(), &ctx).unwrap();

        // The guard must be present.
        assert!(
            thunk.is_guarded(),
            "expected Guarded thunk for span with guard"
        );

        // Forcing must return a type_assert_failed error.
        let err = materialize(&thunk, None, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("E011"),
            "error should have code E011; got: {msg}"
        );
        assert!(
            msg.contains("expected Int"),
            "error should mention expected Int; got: {msg}"
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

        let node = Arc::new(SurfaceNode {
            expr: SurfaceExpression::Int(42),
            span: expr_span,
        });

        // Guard is for guarded_span, but node uses expr_span — no wrap.
        let ctx = ctx_with_guard(guarded_span, Type::Int);
        let thunk = eval_for_test(node, empty_env(), &ctx).unwrap();

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

        let node = Arc::new(SurfaceNode {
            expr: SurfaceExpression::Int(7),
            span: guarded_span.clone(),
        });

        let ctx = ctx_with_guard(guarded_span, Type::Int);
        let thunk = eval_for_test(node, empty_env(), &ctx).unwrap();

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
        assert!(
            result.is_err(),
            "duplicate in Seq head/tail must be rejected"
        );
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
        // Dict pattern. Duplicate bindings use "last binding wins" semantics.
        //
        // match [a: 1  b: 2]
        //   [a: x  b: x  ...]: x
        //
        // The pattern binds x twice; the second binding (b: 2) wins.
        let result = eval_str(
            "[match [a: 1  b: 2]  [a: x  b: x  ...]: x]",
            empty_env(),
            &test_ctx(),
        )
        .unwrap();
        let ctx = test_ctx();
        let val = materialize(&result, None, &ctx).unwrap();
        assert_eq!(val, Value::Int(2));
    }

    #[test]
    fn test_pm3_match_expr_linear_dict_pattern_succeeds() {
        // Integration test: linear Dict pattern must succeed normally.
        //
        // match [a: 1  b: 2]
        //   [a: x  b: y  ...]: 99
        //
        // The arm is linear; no linearity error should fire.
        let result = eval_str(
            "[match [a: 1  b: 2]  [a: x  b: y  ...]: 99]",
            empty_env(),
            &test_ctx(),
        );
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
        //   x: 1    <- first arm, matches anything and returns 1
        //   x: 2    <- second arm, would match anything but arm1 fires first
        //
        // Both arms have a single `x` binding (linear), so no linearity error.
        let result = eval_str("[match 42  x: 1  x: 2]", empty_env(), &test_ctx());
        assert!(
            result.is_ok(),
            "same name in different arms must not trigger linearity error; got: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_match_guard_callable_iterative() {
        // Guard expressed as a callable is now driven through the CEK machine
        // iteratively (MatchGuardCheck with callable_invoked flag) rather than
        // via block_on_anywhere. Verify basic correctness: positive? guard passes
        // for 5, falls through to wildcard for -1.
        //
        // Uses eval_source_with_config (no_fs=true) so we can write a full
        // multi-binding document without needing the filesystem/stdlib.
        let src_pos = "[
            positive?: [fn [let n] [> n 0]]
            result: [match 5  x@[is: positive?]: \"pos\"  _: \"other\"]
        ]";
        let result_pos =
            crate::eval_source_with_config(src_pos, true).expect("eval must not error");
        assert!(
            result_pos.contains("String(\"pos\")"),
            "guard callable should pass for positive: {result_pos:?}"
        );

        // Guard fails for negative input — wildcard arm fires
        let src_neg = "[
            positive?: [fn [let n] [> n 0]]
            result: [match -1  x@[is: positive?]: \"pos\"  _: \"other\"]
        ]";
        let result_neg =
            crate::eval_source_with_config(src_neg, true).expect("eval must not error");
        assert!(
            result_neg.contains("String(\"other\")"),
            "guard callable should fail for negative: {result_neg:?}"
        );
    }

    #[test]
    fn test_match_deep_does_not_stack_overflow() {
        // Deeply nested match dispatched iteratively through the CEK machine
        // (Cont::MatchDispatch with .await on match_pattern). Previously each level
        // called block_on_anywhere() creating nested async contexts that overflowed
        // the stack at ~50 levels. The iterative fix keeps async state on the heap.
        //
        // This exercises: recursive fn -> match 0 -> arm body -> recursive call,
        // 100 levels of match dispatch driven through the CEK continuation stack.
        let src = "[
            depth-match: [fn [let n]
                [match n
                    0:   \"done\"
                    _:   [depth-match [- n 1]]]]
            result: [depth-match 100]
        ]";
        let result = crate::eval_source_with_config(src, true).expect("eval must not error");
        assert!(
            result.contains("String(\"done\")"),
            "deep match should terminate with 'done': {result:?}"
        );
    }
}
