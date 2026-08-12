//! Bootstrap type environment for the type checker.
//!
//! Provides `build_builtin_core_envs()` which parses and type-checks
//! `stdlib/builtin_core.llt` to build the initial type environment seeded
//! with core type definitions (`Handle`, `DirCap`, and other prelude-defined types).
//!
//! Include resolution for user programs is handled by the prelude's `include`
//! function, which runs the full pipeline (parse → desugar → resolve →
//! typecheck → eval) via `builtin-typecheck-doc`.

use std::sync::Arc;

const KIND: &str = "internal";

/// Parse and type-check `stdlib/builtin_core.llt`, returning the resulting
/// `(TyConEnv, type_name_to_id)` pair.
///
/// Uses `include_str!` so the file is embedded at compile time — no runtime libdir access
/// needed.
pub async fn build_builtin_core_envs() -> Result<
    (
        crate::type_def::TyConEnv,
        std::collections::HashMap<String, u64>,
    ),
    Box<crate::error::Diagnostic>,
> {
    let (tycon_env, _, type_name_to_id) = build_builtin_core_envs_inner().await?;
    Ok((tycon_env, type_name_to_id))
}

/// Evaluate the type-stage section of `stdlib/builtin_core.llt` and return the resulting
/// `type_stage_scope` for use by type-checker entry points that do not have an evaluated
/// type-stage scope available (e.g., corpus tests, LSP, formatter).
///
/// This is the authoritative source of the type-stage scope; no Rust code should manually
/// construct this mapping by hardcoding tinct-side type names.
pub async fn get_builtin_core_type_stage_scope(
) -> Result<crate::type_infer::TypeStageData, Box<crate::error::Diagnostic>> {
    let (_, data, _) = build_builtin_core_envs_inner().await?;
    Ok(data)
}

/// Type-check `stdlib/builtin_core.llt` and return the resulting `TyConEnv` containing
/// opaque type definitions (e.g. `BuilderHandle`, `DirCap`).
pub async fn get_builtin_core_tycon_env(
) -> Result<crate::type_def::TyConEnv, Box<crate::error::Diagnostic>> {
    let (tycon_env, _, _) = build_builtin_core_envs_inner().await?;
    Ok(tycon_env)
}

/// Classify a single materialized type-stage value.
///
/// Three cases are handled in precedence order:
/// 1. Variant value → `typenode_value_to_type` attempts conversion to a TypeValue.
///    Returns `Some((tv, None, None))` on success (Resolved entry). Structural types like Span
///    and Diagnostic produce precise record types with named fields.
/// 2. TypeVar kind sentinel (Operator, Label) → `typenode_typevar_kind`. Returns
///    `Some((make_typevalue_nominal(name), None, Some(kind)))` where the third element is the kind.
/// 3. `Value::Function` → parameterized type constructor. Returns
///    `Some((make_typevalue_nominal(name), Some(thunk), None))` where the second element is the thunk.
///
/// Returns:
/// - `Ok(Some((tv, opt_thunk, opt_kind)))`:
///   - `tv`: TypeValue to store in the type-stage scope (the resolved type, or Op placeholder)
///   - `opt_thunk`: `Some(thunk)` if this is a Function (parameterized type constructor)
///   - `opt_kind`: `Some(kind_str)` if this is a TypeVar kind sentinel
/// - `Ok(None)`: Variant not representable at current scope depth, or unrecognised value kind
/// - `Err(_)`: propagated eval error from `typenode_value_to_type`
pub(crate) async fn classify_type_stage_entry(
    name: &str,
    thunk: &std::sync::Arc<crate::value::Thunk>,
    val: &crate::value::Value,
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
    scope_so_far: &[std::collections::HashMap<String, crate::type_infer::TypeValue>],
    type_name_to_id: &std::collections::HashMap<String, u64>,
) -> Result<
    Option<(
        crate::type_infer::TypeValue,
        Option<std::sync::Arc<crate::value::Thunk>>,
        Option<String>,
    )>,
    Box<crate::error::Diagnostic>,
> {
    // For Variant values: call typenode_value_to_type (full recursive converter that reads
    // payload fields for structural types like Span, Diagnostic, Dict, Union, etc.).
    // typenode_value_to_type returns Ok(None) for non-TypeNode variants, so calling it
    // unconditionally is correct — no Rust code here needs to know the TypeNode tycon name.
    if matches!(val, crate::value::Value::Variant { .. }) {
        if let Some(ty) = crate::typecheck::typecheck_annot::typenode_value_to_type(
            val,
            ctx,
            scope_so_far,
            type_name_to_id,
        )
        .await?
        {
            return Ok(Some((ty, None, None)));
        }
        // typenode_value_to_type returned None — this Variant is not representable as a static
        // TypeValue at this scope depth (e.g., not a TypeNode variant, or references type-stage
        // functions that haven't been called yet, or opaque types that need parameterization).
        return Ok(None);
    }
    if let Some(kind) =
        crate::type_normalize::typenode_typevar_kind(val).map_err(|e| Box::new((*e).clone()))?
    {
        let tv = crate::type_infer::make_typevalue_nominal(
            type_name_to_id.get(name).copied().unwrap_or(0),
        );
        return Ok(Some((tv, None, Some(kind))));
    }
    if matches!(val, crate::value::Value::Function { .. }) {
        let tv = crate::type_infer::make_typevalue_nominal(
            type_name_to_id.get(name).copied().unwrap_or(0),
        );
        return Ok(Some((tv, Some(std::sync::Arc::clone(thunk)), None)));
    }
    // Not a recognized Variant, TypeVar sentinel, or Function — not a type-stage entry.
    Ok(None)
}

/// Inner implementation of `build_builtin_core_envs`.
///
/// Parses `stdlib/builtin_core.llt` (embedded at compile time via `include_str!`),
/// runs the full pipeline (desugar → resolve → typecheck), and returns the
/// resulting `TyConEnv` and the evaluated type-stage scope
/// (used to seed `InferState::type_stage_scope`).
async fn build_builtin_core_envs_inner() -> Result<
    (
        crate::type_def::TyConEnv,
        crate::type_infer::TypeStageData,
        std::collections::HashMap<String, u64>,
    ),
    Box<crate::error::Diagnostic>,
> {
    let source = include_str!("../stdlib/builtin_core.llt");
    let sf: Arc<str> = Arc::from("stdlib/builtin_core.llt");

    let program = crate::desugar::desugar_program_full(
        &crate::parser::parse(source, sf)
            .map_err(|e| {
                Box::new(crate::error::Diagnostic::error(
                    KIND,
                    format!("builtin_core.llt parse failed: {e}"),
                    crate::rust_span!(),
                ))
            })?
            .program,
    );
    let program = crate::expand::expand_program(&program);

    let ctx = crate::eval::EvalContext::new_empty();
    let mut state = crate::pipeline::PipelineState::from_builtins(&ctx);

    for doc in &program.documents {
        crate::pipeline::process_pipeline_document(doc, &mut state, &ctx).await?;
    }

    Ok((
        state.infer_state.tycon_env,
        state.infer_state.type_stage_data,
        state.infer_state.type_name_to_id,
    ))
}
