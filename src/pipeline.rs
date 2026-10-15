//! Per-document bootstrap pipeline: resolve → typecheck → eval.
//!
//! Two entry points process a single document against the accumulated `PipelineState`:
//! - [`process_pipeline_document`] — full pipeline (resolve → typecheck → eval)
//! - [`typecheck_pipeline_document`] — typecheck-only (resolve → typecheck, no eval)
//!
//! The bootstrap is a simple loop over documents.

use std::sync::Arc;

use crate::ast::{Spanned, SurfaceDocument};
use crate::eval::EvalContext;
use crate::symbol_table::{ScopeChain, SymbolTable};
use crate::type_infer::InferState;

/// Accumulated state threaded through the per-document pipeline.
pub struct PipelineState {
    /// Type checker state — single source of truth for type-stage data,
    /// tycon_env, type_name_to_id, type_display_names, etc.
    pub(crate) infer_state: InferState,
    /// Current scope — grows as documents are processed.
    pub current_scope: ScopeChain,
}

impl PipelineState {
    /// Create the initial pipeline state from a scope (builtins + capabilities).
    pub fn from_scope(scope: ScopeChain, ctx: &Arc<EvalContext>) -> Self {
        let mut infer_state = InferState::new();
        infer_state.eval_ctx = Some(Arc::clone(ctx));
        Self {
            infer_state,
            current_scope: scope,
        }
    }

    /// Create the initial pipeline state from the Rust-native builtins.
    pub fn from_builtins(ctx: &Arc<EvalContext>) -> Self {
        Self::from_scope(SymbolTable::empty_chain(), ctx)
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

    let is_type_stage = doc.header.get("stage").is_some_and(|stage_node| {
        matches!(
            &stage_node.expr,
            crate::ast::SurfaceExpression::StringLiteral { content, .. }
            if content == "type"
        )
    });
    let stage = if is_type_stage {
        crate::symbol_table::Stage::Type
    } else {
        crate::symbol_table::Stage::Runtime
    };

    // Step 1: Resolve this document against the current scope.
    let resolve_result = crate::resolve::resolve_document(doc, &state.current_scope, stage);
    state
        .infer_state
        .diagnostics
        .extend(resolve_result.diagnostics.iter().cloned());

    // Step 2: Typecheck this document.
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
) -> crate::error::DiagnosticResult<()> {
    let doc = doc_spanned.node.as_ref();
    let mut diagnostics: Vec<crate::error::Diagnostic> = Vec::new();

    let is_type_stage = doc.header.get("stage").is_some_and(|stage_node| {
        matches!(
            &stage_node.expr,
            crate::ast::SurfaceExpression::StringLiteral { content, .. }
            if content == "type"
        )
    });
    let stage = if is_type_stage {
        crate::symbol_table::Stage::Type
    } else {
        crate::symbol_table::Stage::Runtime
    };

    // Step 1: Resolve this document against the current scope.
    let resolve_result = crate::resolve::resolve_document(doc, &state.current_scope, stage);
    diagnostics.extend(resolve_result.diagnostics);

    // Step 1b: Wire Rust builtins into the resolver's scope frames.
    let builtin_defs = crate::builtins_core::core_builtins();
    for frame in &resolve_result.scope_frames {
        for def in &builtin_defs {
            if let Some(entry) = frame.entries().get(def.name) {
                let thunk = std::sync::Arc::new(crate::value::Thunk::value(
                    crate::value::Value::Builtin {
                        def: def.clone(),
                        type_val: None,
                    },
                    crate::rust_span!(),
                ));
                let _ = entry.value.set(thunk);
            }
        }
    }

    // Step 2: Lower and evaluate the document body.

    let display_names = state.infer_state.type_display_names.clone();
    let lowered_body = Arc::new(
        crate::lower::lower(&doc.body, &display_names)
            .take(&ctx.runtime_diagnostics)
            .expect("lowering always succeeds"),
    );

    let (eval_result, eval_diags) =
        crate::eval_core::eval_core_expr(&lowered_body, &state.current_scope, ctx)
            .await
            .into_parts();
    diagnostics.extend(eval_diags);
    match eval_result {
        Ok(thunk) => {
            let (mat_result, mat_diags) =
                crate::eval::materialize(&thunk, None, ctx).await.into_parts();
            diagnostics.extend(mat_diags);
            if let Err(e) = mat_result {
                diagnostics.push(e);
            }
        }
        Err(e) => diagnostics.push(e),
    }

    // Step 3: Typecheck.
    let (_result_ty, tc_errors) = crate::typecheck::process_document(
        doc,
        &mut state.infer_state,
        &state.current_scope,
        &mut None,
    )
    .await;
    diagnostics.extend(tc_errors);

    // Step 4: Accumulate this document's scope frames for subsequent documents.
    // Frames are already tagged with their stage by the resolver (walk_surface_document
    // calls mark_type_stage() on type-stage frames), so resolution later filters by
    // stage over this single chain — no branching needed here.
    for frame in &resolve_result.scope_frames {
        state.current_scope.push(Arc::clone(frame));
    }

    let first_error = diagnostics
        .iter()
        .find(|d| d.level == crate::error::DiagnosticLevel::Err)
        .cloned();
    match first_error {
        Some(err) => crate::error::DiagnosticResult::err(err).with_diagnostics(diagnostics),
        None => crate::error::DiagnosticResult::ok(()).with_diagnostics(diagnostics),
    }
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
