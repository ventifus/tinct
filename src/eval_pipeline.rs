//! Document and pipeline evaluation: `eval_document`, `eval_file`, `eval_file_with_input`.
//!
//! Documents are scope chains (each intermediate dict extends the environment for the next
//! expression). Files are sequences of documents separated by `---`, with `%` threading
//! the previous document's output into the next.

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::ast::{
    Document, File, ResolutionTable, Span, Spanned, SurfaceExpression, SurfaceProgram,
    TypeAnnotationTable,
};
use crate::error::{EvalError, EvalResult};
use crate::value::{Environment, Key, Thunk, Value};

use super::{eval, materialize, EvalContext};

thread_local! {
    /// Cached empty dict thunk used as the default `%` when no stdin is provided.
    /// Avoids allocating a fresh `Arc<Thunk>` on every `eval_file_with_input` call.
    static EMPTY_DICT_THUNK: Arc<Thunk> = Arc::new(Thunk::new_materialized(
        Value::Dict(IndexMap::new()),
        Span::origin(),
    ));
}

/// Evaluate a document: a sequence of expressions forming a scope chain.
///
/// Each intermediate expression is materialized and must produce a `Value::Dict`.
/// The dict's string-keyed entries become bindings in a new child environment that
/// serves as the scope for the next expression. The last expression is returned
/// as-is (lazy, any type). An empty document returns an empty dict.
pub async fn eval_document(
    doc: &Spanned<Document>,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let exprs = &doc.node.expressions;

    if exprs.is_empty() {
        return Ok(Arc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            doc.span,
        )));
    }

    // Validate capabilities if caps: is declared
    if let Some(ref caps_ann) = doc.node.caps {
        for (cap_name, annotation) in &caps_ann.node {
            let full_cap_name = format!("%{}", cap_name);

            // Check if capability is present in environment
            let cap_present = {
                let env_ref = env.read().unwrap();
                env_ref.get(&full_cap_name).is_some()
            };

            if !cap_present {
                // Determine the CLI flag suggestion based on the annotation type
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
                    _ => {
                        // Generic fallback for other capability types
                        ("--cap", format!("{}=VALUE", cap_name))
                    }
                };

                // Check if this is an auto-injected capability
                let auto_injected_caps = ["pwd", "libdir", "stdin"];
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

                return Err(EvalError::internal(message, caps_ann.span).into());
            }
        }
    }

    let mut current_env = env;

    for (i, expr) in exprs.iter().enumerate() {
        let is_last = i == exprs.len() - 1;

        if is_last {
            // Last expression: return its thunk as-is (lazy, any type)
            return eval(Rc::clone(expr), current_env, ctx).await;
        }

        // Intermediate expression: materialize and extract dict bindings
        let thunk = eval(Rc::clone(expr), Arc::clone(&current_env), ctx).await?;
        let value = materialize(&thunk, Some(&expr.span), ctx).await?;

        // Flatten Overlay to Dict for scope chain binding.
        let map = match value {
            Value::Dict(map) => map,
            Value::Overlay(l, r) => {
                crate::builtins::flatten_overlay(&l, &r, "document pipeline", ctx, expr.span)?
            }
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "document pipeline".to_string(),
                    "Dict",
                    value.type_name(),
                    expr.span,
                )
                .into());
            }
        };
        {
            let child_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(
                &current_env,
            ))));
            for (key, val_thunk_id) in map {
                // Only string keys become scope bindings; int keys are positional, not named.
                // Owned iteration (into_iter): Key::String(name) moves the String rather than
                // cloning it — saves one String clone per string-keyed entry.
                // Named bindings are forced to WHNF at binding time — strict let* semantics.
                // This means dead-but-erroring bindings fail eagerly.
                if let Key::String(name) = key {
                    let val_thunk = ctx.get_thunk(val_thunk_id);
                    let forced_value = materialize(&val_thunk, Some(&expr.span), ctx).await?;
                    let strict_thunk = Arc::new(Thunk::new_materialized(forced_value, expr.span));
                    child_env.write().unwrap().insert(name, strict_thunk);
                }
            }
            current_env = child_env;
        }
    }

    // INVARIANT: This is unreachable because the loop above always returns when
    // processing the last expression (when i == exprs.len() - 1). The loop only
    // terminates naturally if exprs is empty, but we return early for empty docs.
    unreachable!(
        "eval_document: loop did not return — exprs was non-empty but is_last never triggered"
    )
}

/// Wrap a thunk with nominal type validation for pipeline input contracts.
///
/// Creates a synthetic `CoreExpr::RuntimeTypeCheck` wrapping a gensym'd `FreeVar` reference.
/// When evaluated, it performs the same validation as a regular `[@Type expr]` assertion.
/// `RuntimeTypeCheck` is used (rather than `CoreExpr::TypeAssert`) because the annotation
/// is not statically resolved — `TypeAssert` requires a pre-resolved `Type`.
fn wrap_with_nominal_validation(
    inner: Arc<Thunk>,
    annotation: &crate::ast::Spanned<crate::ast::Annotation>,
    validation_span: Span,
    ctx: &Arc<EvalContext>,
) -> Arc<Thunk> {
    use crate::ast::CoreExpr;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Generate a unique variable name to avoid collisions with user code
    static GENSYM_COUNTER: AtomicU64 = AtomicU64::new(0);
    let gensym_id = GENSYM_COUNTER.fetch_add(1, Ordering::Relaxed);
    let gensym_name = format!("__nominal_input_{}", gensym_id);

    // Create a synthetic RuntimeTypeCheck expression: [@Annotation __nominal_input_N]
    // RuntimeTypeCheck is correct here because the type is not statically resolved —
    // it uses Annotation directly, which RuntimeTypeCheck supports. CoreExpr::TypeAssert
    // requires a pre-resolved Type which we don't have at pipeline input time.
    let type_check_expr = Arc::new(crate::ast::Spanned::new(
        CoreExpr::RuntimeTypeCheck {
            annotation: annotation.clone(),
            expr: Arc::new(crate::ast::Spanned::new(
                CoreExpr::FreeVar(gensym_name.clone()),
                validation_span,
            )),
            default: None,
        },
        validation_span,
    ));

    // Create an environment with __nominal_input_N bound to the inner thunk
    let validation_env = Arc::new(RwLock::new(Environment::new()));
    validation_env.write().unwrap().insert(gensym_name, inner);

    // Return an Unevaluated thunk wrapping the RuntimeTypeCheck expression
    Arc::new(Thunk::new_unevaluated_core(
        type_check_expr,
        validation_env,
        Arc::clone(ctx),
        validation_span,
    ))
}

/// Evaluate a file: one or more documents separated by `---`.
///
/// Documents are totally isolated -- they share no scope. Data flows between
/// documents via `%` (and named sections `%name`), which are injected into each
/// document's root scope from the previous document's output.
///
/// - For the first document, `%` is an empty dict.
/// - For subsequent documents, `%` is the previous document's result thunk
///   (lazy -- no materialization at the `---` boundary).
/// - The last document's result is the file's output.
/// - An empty file (zero documents) returns an empty dict.
///
/// # Precondition
///
/// **Pipeline invariant:** `expand_surface_program` → `desugar_surface_program` →
/// `resolve_surface_program` → `surface_program_to_file` must be called before passing the [`File`] here.
/// The evaluator has no `$_` handling; callers that skip the desugar pass will see
/// `UndefinedVariable("_")` errors for any `$_` expression. Macros must be expanded
/// before desugaring so that macro-introduced `$_` patterns are also desugared.
///
/// **Note:** Provide an `EvalContext` via `EvalContext::new()` to configure `$include`;
/// no separate setup call required.
pub async fn eval_file(
    file: &File,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    eval_file_with_input(file, env, ctx, None).await
}

/// Evaluate a parsed [`File`], optionally injecting an initial `%` value for the first document.
///
/// When `initial_input` is `Some(thunk)`, that thunk becomes `%` for the first
/// document instead of the default empty dict. This supports the CLI's stdin
/// JSON injection: `cat data.json | llt eval file.llt`.
///
/// # Precondition
///
/// **`desugar::desugar_surface_program` must be called on the [`SurfaceProgram`] before
/// converting to [`File`] and passing it here.** See [`eval_file`] for details.
///
/// **Note:** Provide an `EvalContext` via `EvalContext::new()` to configure `$include`;
/// no separate setup call required.
pub async fn eval_file_with_input(
    file: &File,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
    initial_input: Option<Arc<Thunk>>,
) -> EvalResult<Arc<Thunk>> {
    // % starts as the provided input, or empty dict if none given
    let mut prev_output = initial_input.unwrap_or_else(|| EMPTY_DICT_THUNK.with(Arc::clone));
    // Named section accumulator: maps section name → result thunk
    let mut named: IndexMap<String, Arc<Thunk>> = IndexMap::new();

    for doc in &file.documents {
        // Skip type-stage documents — they are handled separately by create_type_stage_env()
        if doc.node.stage == Some(crate::ast::Stage::Type) {
            continue;
        }

        // Each document gets a fresh scope with % and %name bindings
        let doc_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(&env))));

        // Bind % (pipeline variable)
        // If the document has an expects: annotation, wrap % in a validation thunk
        let percent_thunk = if let Some(ref expects_ann) = doc.node.expects {
            // Wrap prev_output in a thunk that validates on materialization.
            // We use nominal type checking (like --no-typecheck mode) because we don't
            // have type elaboration here (expects annotations are advisory in typecheck).
            wrap_with_nominal_validation(Arc::clone(&prev_output), expects_ann, doc.span, ctx)
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

        let result = eval_document(doc, doc_env, ctx).await?;

        // If this document is named, accumulate it in the named map
        if let Some(ref name) = doc.node.name {
            named.insert(name.clone(), Arc::clone(&result));
        }

        prev_output = result; // lazy: no materialization at boundary
    }

    Ok(prev_output)
}

// ============================================================================
// Surface AST evaluation — runtime-v2 pipeline
// ============================================================================
//
// These functions bypass the Expr/File/Document bridge and evaluate SurfaceProgram
// directly. `eval_surface_document` lowers each SurfaceNode to CoreExpr via lower.rs,
// then calls eval_core_expr_pub. This eliminates the surface_program_to_file +
// expr_to_core_expr round-trip from the hot evaluation path.
//
// Callers must provide:
// - ResolutionTable: from resolve::resolve_surface_program (variable de Bruijn coords)
// - TypeAnnotationTable: from typecheck::typecheck_surface_program (TypeAssert resolution)
//   An empty table is valid — TypeAssert nodes fall back to RuntimeTypeCheck.

/// Evaluate a SurfaceDocument: a sequence of expression items forming a scope chain.
///
/// This is the runtime-v2 replacement for `eval_document`. Each `SurfaceItem::Expr`
/// is lowered to `CoreExpr` via `lower.rs` and evaluated via `eval_core_expr_pub`.
/// `SurfaceItem::Decl` items are skipped (they were processed at expand time).
///
/// The scope-chain semantics are identical to `eval_document`:
/// - Intermediate expressions are materialized and must produce `Value::Dict`.
/// - Dict entries become bindings in a child environment for subsequent expressions.
/// - The last expression is returned as-is (lazy, any type).
/// - An empty document returns an empty dict.
pub async fn eval_surface_document(
    doc: &Spanned<crate::ast::SurfaceDocument>,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
    res: &Arc<ResolutionTable>,
    types: &Arc<TypeAnnotationTable>,
) -> EvalResult<Arc<Thunk>> {
    // Collect expression nodes (skip Decl items — processed by expander)
    let expr_nodes: Vec<&Arc<crate::ast::SurfaceNode>> = doc.node.expressions().collect();

    if expr_nodes.is_empty() {
        return Ok(Arc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            doc.span,
        )));
    }

    // Validate capabilities (same logic as eval_document)
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

                let auto_injected_caps = ["pwd", "libdir", "stdin"];
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

                return Err(EvalError::internal(message, caps_ann.span).into());
            }
        }
    }

    let mut current_env = env;

    for (i, node) in expr_nodes.iter().enumerate() {
        let is_last = i == expr_nodes.len() - 1;

        // Extract static keys from the expression BEFORE lowering.
        // Only SurfaceExpression::Dict with static keys creates a new scope (mirrors resolve.rs).
        let static_keys: Option<HashSet<String>> = match &node.expr {
            SurfaceExpression::Dict(entries) => {
                let keys: Vec<String> = entries
                    .iter()
                    .filter_map(|entry| {
                        entry.node.key.as_ref().and_then(|k| match &k.expr {
                            SurfaceExpression::Str(s) => Some(s.clone()),
                            SurfaceExpression::Annotated { name, .. } => Some(name.clone()),
                            _ => None,
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

        // Lower the SurfaceNode to CoreExpr
        let core_spanned = crate::lower::lower(node, res, types);
        let node_span = node.span;

        if is_last {
            // Last expression: return its thunk as-is (lazy, any type)
            return super::eval_core_expr_pub(&core_spanned, &current_env, ctx).await;
        }

        // Intermediate expression: materialize and extract dict bindings
        let thunk =
            super::eval_core_expr_pub(&core_spanned, &Arc::clone(&current_env), ctx).await?;
        let value = materialize(&thunk, Some(&node_span), ctx).await?;

        // Create child environment with bindings from intermediate expression.
        // CRITICAL: Only insert static-key entries to preserve slot alignment with the resolver.
        // If static_keys is None (non-Dict expression or Dict with no static keys), no scope is created.
        if let Some(ref static_key_set) = static_keys {
            // Flatten Overlay to Dict for scope chain binding.
            // Only computed when static_keys is Some — avoids wasted work when no scope is created.
            let map = match value {
                Value::Dict(map) => map,
                Value::Overlay(l, r) => {
                    crate::builtins::flatten_overlay(&l, &r, "document pipeline", ctx, node_span)?
                }
                _ => {
                    return Err(EvalError::type_mismatch_ctx(
                        "document pipeline".to_string(),
                        "Dict",
                        value.type_name(),
                        node_span,
                    )
                    .into());
                }
            };

            let child_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(
                &current_env,
            ))));
            for (key, val_thunk_id) in map {
                if let Key::String(name) = key {
                    if static_key_set.contains(&name) {
                        let val_thunk = ctx.get_thunk(val_thunk_id);
                        let forced_value = materialize(&val_thunk, Some(&node_span), ctx).await?;
                        let strict_thunk =
                            Arc::new(Thunk::new_materialized(forced_value, node_span));
                        child_env.write().unwrap().insert(name, strict_thunk);
                    }
                }
            }
            current_env = child_env;
        }
    }

    unreachable!(
        "eval_surface_document: loop did not return — expr_nodes was non-empty but is_last never triggered"
    )
}

/// Evaluate a SurfaceProgram: one or more documents separated by `---`.
///
/// Runtime-v2 replacement for `eval_file`. Evaluates `SurfaceDocument`s directly
/// without the `surface_program_to_file` + `eval()` bridge conversion.
///
/// # Precondition
///
/// **Pipeline invariant:** `expand_surface_program` → `desugar_surface_program` →
/// `resolve_surface_program` must be called before passing the program here.
/// The `res` table must be the one returned by `resolve_surface_program`.
/// The `types` table may be empty (from `TypeAnnotationTable::new()`) if type checking
/// was skipped; `TypeAssert` nodes will fall back to `RuntimeTypeCheck` in that case.
pub async fn eval_surface_file(
    program: &SurfaceProgram,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
    res: &Arc<ResolutionTable>,
    types: &Arc<TypeAnnotationTable>,
) -> EvalResult<Arc<Thunk>> {
    eval_surface_file_with_input(program, env, ctx, res, types, None).await
}

/// Evaluate a SurfaceProgram with an optional initial `%` value.
///
/// Runtime-v2 replacement for `eval_file_with_input`. See `eval_surface_file` for
/// preconditions.
pub async fn eval_surface_file_with_input(
    program: &SurfaceProgram,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
    res: &Arc<ResolutionTable>,
    types: &Arc<TypeAnnotationTable>,
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
            wrap_with_nominal_validation(
                Arc::clone(&prev_output),
                expects_ann,
                surface_doc.span,
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
