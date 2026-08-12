//! Per-document bootstrap pipeline: resolve → typecheck → eval.
//!
//! Two entry points process a single document against the accumulated `PipelineState`:
//! - [`process_pipeline_document`] — full pipeline (resolve → typecheck → eval)
//! - [`typecheck_pipeline_document`] — typecheck-only (resolve → typecheck, no eval)
//!
//! The bootstrap is a simple loop over documents.

use std::collections::HashMap;
use std::sync::Arc;

use crate::ast::{Spanned, SurfaceDocument, SurfaceItem};
use crate::eval::EvalContext;
use crate::symbol_table::SymbolTable;
use crate::type_infer::{InferState, TypeValue};

/// Accumulated state threaded through the per-document pipeline.
pub struct PipelineState {
    /// Type checker state — single source of truth for type-stage data,
    /// tycon_env, type_name_to_id, type_display_names, etc.
    pub(crate) infer_state: InferState,
    /// Current scope — grows as documents are processed.
    pub current_scope: Arc<SymbolTable>,
}

impl PipelineState {
    /// Create the initial pipeline state from a scope (builtins + capabilities).
    pub fn from_scope(scope: Arc<SymbolTable>, ctx: &Arc<EvalContext>) -> Self {
        let mut infer_state = InferState::new();
        infer_state.eval_ctx = Some(Arc::clone(ctx));
        Self {
            infer_state,
            current_scope: scope,
        }
    }

    /// Create the initial pipeline state from the Rust-native builtins.
    pub fn from_builtins(ctx: &Arc<EvalContext>) -> Self {
        Self::from_scope(SymbolTable::empty(), ctx)
    }

    /// Access type checker diagnostics collected during pipeline processing.
    pub fn diagnostics(&self) -> &[crate::error::Diagnostic] {
        &self.infer_state.diagnostics
    }
}

/// Typecheck a single document: resolve → typecheck (no eval).
///
/// Returns diagnostics from the type checker. Used by typecheck-only paths
/// (lint pipeline, corpus tests) that need diagnostic inspection without evaluation.
pub async fn typecheck_pipeline_document(
    doc_spanned: &Spanned<Arc<SurfaceDocument>>,
    state: &mut PipelineState,
) -> Vec<crate::error::Diagnostic> {
    let doc = doc_spanned.node.as_ref();

    // Step 1: Resolve this document against the current scope.
    let _resolve_diags = crate::resolve::resolve_document(doc, &state.current_scope);

    // Sync type-stage data into InferState before typecheck.
    // Step 2: Typecheck this document.
    // InferState is the single source of truth — no sync needed.
    let (_result_ty, tc_errors) = crate::typecheck::process_document(
        doc,
        &mut state.infer_state,
        &state.current_scope,
        &mut None,
    )
    .await;

    state
        .infer_state
        .diagnostics
        .extend(tc_errors.iter().cloned());

    tc_errors
}

/// Process a single document through the full pipeline: resolve → typecheck → eval.
///
/// After evaluation, routes results based on the document's stage header:
/// - `stage: "type"` → classify entries and update `state.infer_state.type_stage_data`
pub async fn process_pipeline_document(
    doc_spanned: &Spanned<Arc<SurfaceDocument>>,
    state: &mut PipelineState,
    ctx: &Arc<EvalContext>,
) -> Result<(), Box<crate::error::Diagnostic>> {
    let doc = doc_spanned.node.as_ref();

    // Step 1: Resolve this document against the current scope.
    let _resolve_diags = crate::resolve::resolve_document(doc, &state.current_scope);

    // Step 1b: Wire Rust builtins — for each scope entry matching a registered
    // builtin name, pre-write Value::Builtin. OnceLock is write-once, so eval's
    // Placeholder write becomes a no-op.
    let builtin_defs = crate::builtins_core::core_builtins();
    for doc_item in &doc.items {
        if let crate::ast::SurfaceItem::Expr(node) = doc_item {
            if let crate::ast::SurfaceExpression::Dict { scope, .. } = &node.expr {
                if let Some(dict_scope) = scope.get() {
                    for def in &builtin_defs {
                        if let Some(entry) = dict_scope.entries().get(def.name) {
                            let thunk = std::sync::Arc::new(crate::value::Thunk::value(
                                crate::value::Value::Builtin {
                                    def: def.clone(),
                                    type_val: None,
                                },
                                crate::rust_span!(),
                            ));
                            entry
                                .value
                                .set(thunk)
                                .expect("builtin wiring: scope entry must be empty before eval");
                        }
                    }
                }
            }
        }
    }

    // Step 2: Evaluate this document.
    let expr_nodes: Vec<Arc<crate::ast::SurfaceNode>> = doc
        .items
        .iter()
        .filter_map(|item| match item {
            SurfaceItem::Expr(node) => Some(Arc::clone(node)),
            SurfaceItem::Decl(_) => None,
        })
        .collect();

    // Lower expressions to CoreExpr for eval_document.
    let display_names = state.infer_state.type_display_names.clone();
    let entries: Vec<Arc<crate::ast::Spanned<crate::ast::CoreExpr>>> = expr_nodes
        .iter()
        .map(|node| {
            Arc::new(
                crate::lower::lower(node, &display_names)
                    .take(&ctx.runtime_diagnostics)
                    .expect("lowering always succeeds"),
            )
        })
        .collect();

    let (_eval_scope, eval_diags) =
        crate::eval::eval_document(&entries, ctx, Arc::clone(&state.current_scope)).await;

    // Propagate eval diagnostics.
    for diag in &eval_diags {
        if diag.level == crate::error::DiagnosticLevel::Err {
            return Err(Box::new(diag.clone()));
        }
    }

    // Read the scope from the document's outermost Dict AST node.
    // The resolver created this scope; eval populated its values.
    for node in expr_nodes.iter().rev() {
        if let crate::ast::SurfaceExpression::Dict { scope, .. } = &node.expr {
            if let Some(dict_scope) = scope.get() {
                state.current_scope = Arc::clone(dict_scope);
                break;
            }
        }
    }

    // Step 4: Route results based on stage header.
    let is_type_stage = doc.header.get("stage").is_some_and(|stage_node| {
        matches!(
            &stage_node.expr,
            crate::ast::SurfaceExpression::StringLiteral { content, .. }
            if content == "type"
        )
    });

    if is_type_stage {
        let scope_entries = state.current_scope.entries();
        let mut scope_map: HashMap<String, TypeValue> = HashMap::new();
        for (name, entry) in scope_entries {
            if let Some(thunk) = entry.value.get() {
                let val = crate::eval::materialize(thunk, None, ctx).await?;
                if let Some((tv, opt_fn_thunk, opt_kind)) =
                    crate::imports::classify_type_stage_entry(
                        name,
                        thunk,
                        &val,
                        ctx,
                        &state.infer_state.type_stage_data.scope,
                        &state.infer_state.type_name_to_id,
                    )
                    .await?
                {
                    scope_map.insert(name.to_string(), tv);
                    if let Some(fn_thunk) = opt_fn_thunk {
                        state
                            .infer_state
                            .type_stage_data
                            .fns
                            .insert(name.to_string(), fn_thunk);
                    }
                    if let Some(kind) = opt_kind {
                        state
                            .infer_state
                            .type_stage_data
                            .type_vars
                            .insert(name.to_string(), kind);
                    }
                    if let Some(id) = crate::type_infer::typevalue_nominal_id(
                        scope_map.get(name.as_str()).unwrap(),
                    ) {
                        if id > 0 {
                            state
                                .infer_state
                                .type_name_to_id
                                .insert(name.to_string(), id);
                            state
                                .infer_state
                                .type_display_names
                                .entry(id)
                                .or_insert_with(|| name.to_string());
                        }
                    }
                }
            }
        }
        if !scope_map.is_empty() {
            state.infer_state.type_stage_data.scope.push(scope_map);
        }
    }

    // Step 4: Typecheck. Runs AFTER eval so that the scope has values
    // and type-stage entries are classified. The type checker resolves
    // annotations by looking up values in the scope.
    let (_result_ty, tc_errors) = crate::typecheck::process_document(
        doc,
        &mut state.infer_state,
        &state.current_scope,
        &mut None,
    )
    .await;

    for err in &tc_errors {
        if err.level == crate::error::DiagnosticLevel::Err {
            return Err(Box::new(err.clone()));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_ctx() -> Arc<EvalContext> {
        EvalContext::new_empty()
    }

    #[tokio::test]
    async fn typecheck_pipeline_document_simple_dict() {
        // Parse a minimal tinct program.
        let file: Arc<str> = Arc::from("<test>");
        let parsed = crate::parser::parse("[x: 42]", file).expect("parse should succeed");
        let program = crate::desugar::desugar_program_full(&parsed.program);
        let program = crate::expand::expand_program(&program);

        let ctx = test_ctx();
        let mut state = PipelineState::from_builtins(&ctx);

        for doc in &program.documents {
            let diags = super::typecheck_pipeline_document(doc, &mut state).await;
            let errors: Vec<_> = diags
                .iter()
                .filter(|d| d.level == crate::error::DiagnosticLevel::Err)
                .collect();
            assert!(
                errors.is_empty(),
                "unexpected type errors: {:?}",
                errors.iter().map(|d| &d.message).collect::<Vec<_>>()
            );
        }

        // State-level diagnostics should also be error-free.
        let state_errors: Vec<_> = state
            .diagnostics()
            .iter()
            .filter(|d| d.level == crate::error::DiagnosticLevel::Err)
            .collect();
        assert!(
            state_errors.is_empty(),
            "unexpected state-level type errors: {:?}",
            state_errors.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }
}
