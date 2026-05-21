//! Document and pipeline evaluation: `eval_document`, `eval_file`, `eval_file_with_input`.
//!
//! Documents are scope chains (each intermediate dict extends the environment for the next
//! expression). Files are sequences of documents separated by `---`, with `%` threading
//! the previous document's output into the next.

use std::rc::Rc;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::ast::{Document, File, Span, Spanned};
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
pub fn eval_document(
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
            return eval(Rc::clone(expr), current_env, ctx);
        }

        // Intermediate expression: materialize and extract dict bindings
        let thunk = eval(Rc::clone(expr), Arc::clone(&current_env), ctx)?;
        let value = materialize(&thunk, Some(&expr.span), ctx)?;

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
                    let forced_value = materialize(&val_thunk, Some(&expr.span), ctx)?;
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
/// This creates a synthetic TypeAssert expression that wraps a gensym'd variable reference.
/// When evaluated, it will perform the same validation as a regular `[@Type expr]` assertion.
fn wrap_with_nominal_validation(
    inner: Arc<Thunk>,
    annotation: &crate::ast::Spanned<crate::ast::Annotation>,
    validation_span: Span,
    ctx: &Arc<EvalContext>,
) -> Arc<Thunk> {
    use crate::ast::Expr;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Generate a unique variable name to avoid collisions with user code
    static GENSYM_COUNTER: AtomicU64 = AtomicU64::new(0);
    let gensym_id = GENSYM_COUNTER.fetch_add(1, Ordering::Relaxed);
    let gensym_name = format!("__nominal_input_{}", gensym_id);

    // Create a synthetic TypeAssert expression: [@Annotation __nominal_input_N]
    let varref_expr = Box::new(crate::ast::Spanned::new(
        Expr::VarRef {
            name: gensym_name.clone(),
            escaped: false,
            resolved: RefCell::new(None),
        },
        validation_span,
    ));

    let type_assert_expr = Rc::new(crate::ast::Spanned::new(
        Expr::TypeAssert {
            annotation: annotation.clone(),
            expr: varref_expr,
            resolved_type: RefCell::new(None),
        },
        validation_span,
    ));

    // Create an environment with __nominal_input_N bound to the inner thunk
    let validation_env = Arc::new(RwLock::new(Environment::new()));
    validation_env.write().unwrap().insert(gensym_name, inner);

    // Return an Unevaluated thunk wrapping the TypeAssert expression
    Arc::new(Thunk::new_unevaluated(
        type_assert_expr,
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
/// **`desugar::desugar_file` must be called on the [`File`] before passing it here.**
/// The evaluator has no `$_` handling; callers that skip the desugar pass will see
/// `UndefinedVariable("_")` errors for any `$_` expression. All pipeline entry points
/// (`eval_source_with_config`, `main.rs::run_eval`, `repl.rs::eval_input`,
/// `builtins.rs` `$include` handler) already call `desugar_file` first.
///
/// **Note:** Provide an `EvalContext` via `EvalContext::new()` to configure `$include`;
/// no separate setup call required.
pub fn eval_file(
    file: &File,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    eval_file_with_input(file, env, ctx, None)
}

/// Evaluate a parsed [`File`], optionally injecting an initial `%` value for the first document.
///
/// When `initial_input` is `Some(thunk)`, that thunk becomes `%` for the first
/// document instead of the default empty dict. This supports the CLI's stdin
/// JSON injection: `cat data.json | llt eval file.llt`.
///
/// # Precondition
///
/// **`desugar::desugar_file` must be called on the [`File`] before passing it here.**
/// See [`eval_file`] for details.
///
/// **Note:** Provide an `EvalContext` via `EvalContext::new()` to configure `$include`;
/// no separate setup call required.
pub fn eval_file_with_input(
    file: &File,
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
    initial_input: Option<Arc<Thunk>>,
) -> EvalResult<Arc<Thunk>> {
    // % starts as the provided input, or empty dict if none given
    let mut prev_output = initial_input.unwrap_or_else(|| EMPTY_DICT_THUNK.with(|t| Arc::clone(t)));
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

        doc_env.write().unwrap().insert("%".to_string(), percent_thunk);

        // Bind all previously named sections as %name
        for (section_name, section_thunk) in &named {
            doc_env
                .write().unwrap()
                .insert(format!("%{}", section_name), Arc::clone(section_thunk));
        }

        let result = eval_document(doc, doc_env, ctx)?;

        // If this document is named, accumulate it in the named map
        if let Some(ref name) = doc.node.name {
            named.insert(name.clone(), Arc::clone(&result));
        }

        prev_output = result; // lazy: no materialization at boundary
    }

    Ok(prev_output)
}
