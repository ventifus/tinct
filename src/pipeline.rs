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
use crate::type_infer::{InferState, TypeStageData, TypeValue};

/// Accumulated state threaded through the per-document pipeline.
pub struct PipelineState {
    /// Type-stage scope — TypeValues from type-stage documents.
    pub type_stage_data: TypeStageData,
    /// Type constructor definitions accumulated across documents.
    pub tycon_env: crate::type_def::TyConEnv,
    /// Type name → type_decl_id mapping.
    pub type_name_to_id: HashMap<String, u64>,
    /// type_decl_id → display name mapping.
    pub type_display_names: HashMap<u64, String>,
    /// Resolver seed frames — one per scope level (builtins + previous documents).
    pub resolver_frames: Vec<indexmap::IndexMap<String, u32>>,
    /// Type checker state — carries substitution, constraints, levels, etc.
    pub(crate) infer_state: InferState,
}

impl PipelineState {
    /// Create the initial pipeline state from the Rust-native builtins.
    pub fn from_builtins(ctx: &Arc<EvalContext>) -> Self {
        let root_frame = ctx.root_group_resolver_map();
        let mut infer_state = InferState::new();
        infer_state.eval_ctx = Some(Arc::clone(ctx));

        Self {
            type_stage_data: TypeStageData::new(),
            tycon_env: HashMap::new(),
            type_name_to_id: HashMap::new(),
            type_display_names: HashMap::new(),
            resolver_frames: vec![root_frame],
            infer_state,
        }
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

    // Step 1: Resolve this document against the current environment.
    let env_names: Vec<String> = Vec::new();
    let root_group_len = state
        .resolver_frames
        .iter()
        .flat_map(|f| f.values())
        .copied()
        .max()
        .map(|m| m + 1)
        .unwrap_or(0);

    let (resolve_table, _resolve_diags, _unreferenced, resolve_frames) =
        crate::resolve::resolve_surface_document_with_seed_frames(
            doc,
            &state.resolver_frames,
            &env_names,
            root_group_len,
        );
    let resolve_table = Arc::new(resolve_table);

    // Step 2: Typecheck this document.
    crate::typecheck::build_infer_state(
        &mut state.infer_state,
        Arc::clone(&resolve_table),
        resolve_frames,
        state.type_stage_data.scope.clone(),
        state.type_stage_data.fns.clone(),
        state.type_stage_data.type_vars.clone(),
        state.tycon_env.clone(),
        state.type_name_to_id.clone(),
        state.type_display_names.clone(),
    );

    let (_result_ty, tc_errors) =
        crate::typecheck::process_document(doc, &mut state.infer_state, &mut None).await;

    // Append tc_errors to state diagnostics so state.diagnostics() is the
    // single source of truth — consistent with process_pipeline_document.
    state
        .infer_state
        .diagnostics
        .extend(tc_errors.iter().cloned());

    // Update state for next document.
    state.tycon_env = state.infer_state.tycon_env.clone();
    state.type_name_to_id = state.infer_state.type_name_to_id.clone();
    state.type_display_names = state.infer_state.type_display_names.clone();

    tc_errors
}

/// Process a single document through the full pipeline: resolve → typecheck → eval.
///
/// After evaluation, routes results based on the document's stage header:
/// - `stage: "type"` → classify entries and update `state.type_stage_data`
/// - Otherwise → no-op (bindings propagate via resolver_frames)
pub async fn process_pipeline_document(
    doc_spanned: &Spanned<Arc<SurfaceDocument>>,
    state: &mut PipelineState,
    ctx: &Arc<EvalContext>,
) -> Result<(), Box<crate::error::Diagnostic>> {
    let doc = doc_spanned.node.as_ref();

    // Step 1: Resolve this document against the current environment.
    let env_names: Vec<String> = Vec::new();
    let root_group_len = state
        .resolver_frames
        .iter()
        .flat_map(|f| f.values())
        .copied()
        .max()
        .map(|m| m + 1)
        .unwrap_or(0);

    let (resolve_table, _resolve_diags, _unreferenced, resolve_frames) =
        crate::resolve::resolve_surface_document_with_seed_frames(
            doc,
            &state.resolver_frames,
            &env_names,
            root_group_len,
        );
    let resolve_table = Arc::new(resolve_table);

    // Step 2: Typecheck this document.
    // Set the resolution table and type-stage data on InferState before type-checking.
    crate::typecheck::build_infer_state(
        &mut state.infer_state,
        Arc::clone(&resolve_table),
        resolve_frames,
        state.type_stage_data.scope.clone(),
        state.type_stage_data.fns.clone(),
        state.type_stage_data.type_vars.clone(),
        state.tycon_env.clone(),
        state.type_name_to_id.clone(),
        state.type_display_names.clone(),
    );

    let (_result_ty, tc_errors) =
        crate::typecheck::process_document(doc, &mut state.infer_state, &mut None).await;

    // Propagate type checker errors — they are already Diagnostics.
    for err in &tc_errors {
        if err.level == crate::error::DiagnosticLevel::Err {
            return Err(Box::new(err.clone()));
        }
    }

    // Collect tycon_env updates from the type checker.
    state.tycon_env = state.infer_state.tycon_env.clone();
    state.type_name_to_id = state.infer_state.type_name_to_id.clone();
    state.type_display_names = state.infer_state.type_display_names.clone();

    // Step 3: Evaluate this document.
    let expr_nodes: Vec<Arc<crate::ast::SurfaceNode>> = doc
        .items
        .iter()
        .filter_map(|item| match item {
            SurfaceItem::Expr(node) => Some(Arc::clone(node)),
            SurfaceItem::Decl(_) => None,
        })
        .collect();

    let result_thunk = crate::eval::eval_document_exprs_with_env(&expr_nodes, ctx, None).await?;

    // Step 4: Route results based on stage header.
    let is_type_stage = doc.header.get("stage").is_some_and(|stage_node| {
        matches!(
            &stage_node.expr,
            crate::ast::SurfaceExpression::StringLiteral { content, .. }
            if content == "type"
        )
    });

    if is_type_stage {
        // Materialize the result and classify each entry.
        let result_val = crate::eval::materialize(&result_thunk, None, ctx).await?;
        if let crate::value::Value::Dict { entries, .. } = &result_val {
            let mut scope_map: HashMap<String, TypeValue> = HashMap::new();
            for (key, thunk) in entries {
                if let crate::value::HashableValue::Str(name) = key {
                    let val = crate::eval::materialize(thunk, None, ctx).await?;
                    if let Some((tv, opt_fn_thunk, opt_kind)) =
                        crate::imports::classify_type_stage_entry(
                            name,
                            thunk,
                            &val,
                            ctx,
                            &state.type_stage_data.scope,
                            &state.type_name_to_id,
                        )
                        .await?
                    {
                        scope_map.insert(name.to_string(), tv);
                        if let Some(fn_thunk) = opt_fn_thunk {
                            state.type_stage_data.fns.insert(name.to_string(), fn_thunk);
                        }
                        if let Some(kind) = opt_kind {
                            state
                                .type_stage_data
                                .type_vars
                                .insert(name.to_string(), kind);
                        }
                        // Register type_name_to_id from the TypeValue if nominal.
                        if let Some(id) = crate::type_infer::typevalue_nominal_id(
                            scope_map.get(name.as_ref()).unwrap(),
                        ) {
                            if id > 0 {
                                state.type_name_to_id.insert(name.to_string(), id);
                                state
                                    .type_display_names
                                    .entry(id)
                                    .or_insert_with(|| name.to_string());
                            }
                        }
                    }
                }
            }
            if !scope_map.is_empty() {
                state.type_stage_data.scope.push(scope_map);
            }
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
