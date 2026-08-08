//! Per-document bootstrap pipeline: resolve → typecheck → eval.
//!
//! One function (`process_pipeline_document`) processes a single document against the
//! accumulated `PipelineState`. The bootstrap is a simple loop over documents.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::ast::{Spanned, SurfaceDocument, SurfaceItem};
use crate::env::Env;
use crate::error::{EvalError, EvalResult};
use crate::eval::EvalContext;
use crate::type_infer::{InferState, TypeStageData, TypeValue};

/// Accumulated state threaded through the per-document pipeline.
pub struct PipelineState {
    /// Runtime environment — bindings from all processed documents.
    pub env: Arc<RwLock<Env>>,
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
    pub infer_state: InferState,
}

impl PipelineState {
    /// Create the initial pipeline state from the Rust-native builtins.
    pub fn from_builtins(ctx: &Arc<EvalContext>) -> Self {
        let env = crate::builtins::build_core_env();
        let root_frame = ctx.root_group_resolver_map();
        let mut infer_state = InferState::with_env(Arc::clone(&env));
        infer_state.eval_ctx = Some(Arc::clone(ctx));

        // Seed the type checker's env with Unknown type schemes for all builtins.
        // The type checker needs to resolve VarRefs to builtins during type-checking.
        // Builtins don't have declared types yet (those come from builtin_core.llt),
        // so they start as Unknown (gradual typing).
        {
            let mut env_guard = env.write().unwrap();
            for (name, &slot) in &root_frame {
                let idx = slot as usize;
                env_guard.insert_at_slot(
                    idx,
                    name.clone(),
                    crate::type_infer::make_typevalue_unknown(),
                    None,
                );
            }
        }

        Self {
            env,
            type_stage_data: TypeStageData::new(),
            tycon_env: HashMap::new(),
            type_name_to_id: HashMap::new(),
            type_display_names: HashMap::new(),
            resolver_frames: vec![root_frame],
            infer_state,
        }
    }
}

/// Process a single document through the full pipeline: resolve → typecheck → eval.
///
/// After evaluation, routes results based on the document's stage header:
/// - `stage: "type"` → classify entries and update `state.type_stage_data`
/// - Otherwise → update `state.env` with the document's bindings
pub async fn process_pipeline_document(
    doc_spanned: &Spanned<Arc<SurfaceDocument>>,
    state: &mut PipelineState,
    ctx: &Arc<EvalContext>,
) -> EvalResult<()> {
    let doc = doc_spanned.node.as_ref();

    // Step 1: Resolve this document against the current environment.
    // env_names are exports from PREVIOUS documents (not builtins — those are in resolver_frames).
    // For the first document, this is empty.
    let env_names: Vec<String> = Vec::new(); // TODO: populate from previous document exports
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
    state.infer_state.resolution_table = Arc::clone(&resolve_table);
    state.infer_state.resolver_frames = resolve_frames;
    state.infer_state.type_stage_scope = state.type_stage_data.scope.clone();
    state.infer_state.type_stage_fns = state.type_stage_data.fns.clone();
    state.infer_state.type_stage_type_vars = state.type_stage_data.type_vars.clone();
    state.infer_state.tycon_env = state.tycon_env.clone();
    state.infer_state.type_name_to_id = state.type_name_to_id.clone();
    state.infer_state.type_display_names = state.type_display_names.clone();
    // Sync into the embedded InferenceContext.
    state.infer_state.ctx.type_name_to_id = state.type_name_to_id.clone();
    state.infer_state.ctx.type_display_names = state.type_display_names.clone();
    for (id, def) in &state.tycon_env {
        state.infer_state.ctx.tycon_env.insert(*id, Arc::clone(def));
    }

    // Create a DocSequential env frame between state.env and the dict env.
    // This matches the resolver's scope structure: seed_frame → DocSequential → dict.
    // The DocSequential frame is empty for the first document (no previous exports).
    let doc_sequential_env = Arc::new(RwLock::new(crate::env::Env::with_parent(Arc::clone(
        &state.env,
    ))));

    let (new_env, _result_ty, tc_errors) = crate::typecheck::process_document(
        doc,
        &doc_sequential_env,
        &mut state.infer_state,
        &mut None,
    )
    .await;

    // Log ALL type checker diagnostics.
    for err in &tc_errors {
        eprintln!("[pipeline typecheck] {:?}: {}", err.level, err.message);
    }
    // Propagate type checker errors.
    for err in &tc_errors {
        if err.level == crate::error::DiagnosticLevel::Err {
            return Err(Box::new(EvalError::internal(
                format!("typecheck error: {}", err.message),
                err.spans
                    .first()
                    .map(|s| s.0.clone())
                    .unwrap_or_else(|| crate::rust_span!()),
            )));
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

    let result_thunk = crate::eval::eval_document_exprs_with_env(
        &expr_nodes,
        ctx,
        None,
    )
    .await?;

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
                            state.type_stage_data.type_vars.insert(name.to_string(), kind);
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

    // Update environment for the next document.
    state.env = new_env;

    // Extend resolver frames with this document's scope entries.
    // The new env has bindings from this document — extract them for the next resolve.
    // (The resolver_frames were already updated by the resolve step's unified_frames output.)

    Ok(())
}
