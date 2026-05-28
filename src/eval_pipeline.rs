//! Document and pipeline evaluation: `eval_document_exprs`, `eval_surface_document`, `eval_surface_file`, `eval_surface_file_with_input`.
//!
//! Documents are scope chains (each intermediate dict extends the environment for the next
//! expression). Files are sequences of documents separated by `---`, with `%` threading
//! the previous document's output into the next.
//!
//! The canonical scope-chaining loop is [`eval_document_exprs`]. Both
//! [`eval_surface_document`] and `builtin_eval` (in `builtins_meta.rs`) delegate to it.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::ast::{
    ResolutionTable, Span, Spanned, SurfaceNode, SurfaceProgram, TypeAnnotationTable,
};
use crate::error::{EvalError, EvalResult};
use crate::value::{Environment, Key, Thunk, Value};

use super::{materialize, EvalContext};

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
fn wrap_with_nominal_validation(
    inner: Arc<Thunk>,
    annotation: &crate::ast::Spanned<crate::ast::Annotation>,
    resolved_type: Option<crate::types::Type>,
    validation_span: Span,
    ctx: &Arc<EvalContext>,
) -> Arc<Thunk> {
    use crate::ast::CoreExpr;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Generate a unique variable name to avoid collisions with user code
    static GENSYM_COUNTER: AtomicU64 = AtomicU64::new(0);
    let gensym_id = GENSYM_COUNTER.fetch_add(1, Ordering::Relaxed);
    let gensym_name = format!("__nominal_input_{}", gensym_id);

    // Create a synthetic TypeAssert expression: [@Annotation __nominal_input_N]
    // If resolved_type is None (untyped contract), use Type::Unknown which accepts all values.
    let type_check_expr = Arc::new(crate::ast::Spanned::new(
        CoreExpr::TypeAssert {
            annotation: annotation.clone(),
            expr: Arc::new(crate::ast::Spanned::new(
                CoreExpr::FreeVar(gensym_name.clone()),
                validation_span.clone(),
            )),
            resolved_type: resolved_type.unwrap_or(crate::types::Type::Unknown),
        },
        validation_span.clone(),
    ));

    // Create an environment with __nominal_input_N bound to the inner thunk
    let validation_env = Arc::new(RwLock::new(Environment::new()));
    validation_env.write().unwrap().insert(gensym_name, inner);

    // Return an Unevaluated thunk wrapping the TypeAssert expression
    Arc::new(Thunk::new_unevaluated_core(
        type_check_expr,
        validation_env,
        Arc::clone(ctx),
        validation_span,
    ))
}

// ============================================================================
// Surface AST evaluation — runtime-v2 pipeline
// ============================================================================
//
// These functions evaluate SurfaceProgram directly. `eval_surface_document` lowers
// each SurfaceNode to CoreExpr via lower.rs, then calls eval_core_expr_pub.
//
// Callers must provide:
// - ResolutionTable: from resolve::resolve_surface_program (variable de Bruijn coords)
// - TypeAnnotationTable: from typecheck::typecheck_surface_program (TypeAssert resolution)
//   An empty table is valid — TypeAssert nodes use Type::Unknown (accepts all values).

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
/// - **Last expression**: lower → eval (lazy). The resulting thunk is returned
///   without forcing — callers decide when (and whether) to materialize it.
/// - **Empty slice**: returns a materialized empty-dict thunk (same as an empty doc).
///
/// # Slot alignment note
///
/// Dict literals with only static string keys produce a `Value::Dict` whose keys are
/// exactly the static keys, so promoting all string keys is equivalent to filtering.
/// Dicts with computed keys (e.g., `[$k: v]`) may produce runtime string keys unknown
/// to the resolver; inserting them adds extra names to the child env. The slot-based
/// `get_by_slot` fast path detects the mismatch via name verification and falls back to
/// name-based lookup (correct, slightly slower). No wrong-value bugs can result.
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
            return super::eval_core_expr_pub(&core_spanned, &current_env, ctx).await;
        }

        // Intermediate expression: eval and materialize to extract potential bindings.
        let thunk =
            super::eval_core_expr_pub(&core_spanned, &Arc::clone(&current_env), ctx).await?;
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
                    let val_thunk = ctx.get_thunk(*val_thunk_id);
                    // Strictly materialize each value before binding (shallow let* semantics).
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
/// Evaluates `SurfaceDocument`s directly via `eval_surface_document`. Documents are
/// isolated — data flows between them only via `%` and named sections (`%name`).
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
    eval_surface_file_with_input(program, env, ctx, res, types, &HashMap::new(), None).await
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
    expects_resolved: &HashMap<crate::ast::Span, crate::types::Type>,
    initial_input: Option<Arc<Thunk>>,
) -> EvalResult<Arc<Thunk>> {
    let mut prev_output = initial_input.unwrap_or_else(|| EMPTY_DICT_THUNK.with(Arc::clone));
    let mut named: IndexMap<String, Arc<Thunk>> = IndexMap::new();

    for surface_doc in &program.documents {
        // Skip type-stage documents
        if surface_doc.node.stage == Some(crate::ast::Stage::Type) {
            continue;
        }

        // Each document gets a fresh scope with % and %name bindings
        let doc_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(&env))));

        // Bind % (pipeline variable), wrapping with validation if expects: is declared
        let percent_thunk = if let Some(ref expects_ann) = surface_doc.node.expects {
            let resolved_type = expects_resolved.get(&expects_ann.span).cloned();
            wrap_with_nominal_validation(
                Arc::clone(&prev_output),
                expects_ann,
                resolved_type,
                surface_doc.span.clone(),
                ctx,
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

        prev_output = result; // lazy: no materialization at boundary
    }

    Ok(prev_output)
}
