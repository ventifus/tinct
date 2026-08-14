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

const KIND: &str = "import";

/// Parse and type-check `stdlib/builtin_core.llt`, returning the resulting
/// `(TyConEnv, type_name_to_id)` pair.
///
/// Uses `include_str!` so the file is embedded at compile time — no runtime libdir access
/// needed.
pub async fn build_builtin_core_envs() -> crate::error::DiagnosticResult<(
    crate::type_def::TyConEnv,
    std::collections::HashMap<String, u64>,
)> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
    let (result, diags) = build_builtin_core_envs_inner().await.into_parts();
    _dr_diags.extend(diags);
    let (tycon_env, _, type_name_to_id) = match result {
        Ok(v) => v,
        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
    };
    crate::error::DiagnosticResult::ok((tycon_env, type_name_to_id)).with_diagnostics(_dr_diags)
}

/// Evaluate the type-stage section of `stdlib/builtin_core.llt` and return the resulting
/// `type_stage_scope` for use by type-checker entry points that do not have an evaluated
/// type-stage scope available (e.g., corpus tests, LSP, formatter).
///
/// This is the authoritative source of the type-stage scope; no Rust code should manually
/// construct this mapping by hardcoding tinct-side type names.
pub async fn get_builtin_core_type_stage_scope(
) -> crate::error::DiagnosticResult<crate::type_infer::TypeStageData> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
    let (result, diags) = build_builtin_core_envs_inner().await.into_parts();
    _dr_diags.extend(diags);
    let (_, data, _) = match result {
        Ok(v) => v,
        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
    };
    crate::error::DiagnosticResult::ok(data).with_diagnostics(_dr_diags)
}

/// Type-check `stdlib/builtin_core.llt` and return the resulting `TyConEnv` containing
/// opaque type definitions (e.g. `BuilderHandle`, `DirCap`).
pub async fn get_builtin_core_tycon_env(
) -> crate::error::DiagnosticResult<crate::type_def::TyConEnv> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
    let (result, diags) = build_builtin_core_envs_inner().await.into_parts();
    _dr_diags.extend(diags);
    let (tycon_env, _, _) = match result {
        Ok(v) => v,
        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
    };
    crate::error::DiagnosticResult::ok(tycon_env).with_diagnostics(_dr_diags)
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
) -> crate::error::DiagnosticResult<
    Option<(
        crate::type_infer::TypeValue,
        Option<std::sync::Arc<crate::value::Thunk>>,
        Option<String>,
    )>,
> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
    // For Variant values: call typenode_value_to_type (full recursive converter that reads
    // payload fields for structural types like Span, Diagnostic, Dict, Union, etc.).
    // typenode_value_to_type returns Ok(None) for non-TypeNode variants, so calling it
    // unconditionally is correct — no Rust code here needs to know the TypeNode tycon name.
    if matches!(val, crate::value::Value::Variant { .. }) {
        let (tv_result, tv_diags) = crate::typecheck::typecheck_annot::typenode_value_to_type(
            val,
            ctx,
            scope_so_far,
            type_name_to_id,
        )
        .await
        .into_parts();
        _dr_diags.extend(tv_diags);
        let maybe_ty = match tv_result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };
        if let Some(ty) = maybe_ty {
            return crate::error::DiagnosticResult::ok(Some((ty, None, None)))
                .with_diagnostics(_dr_diags);
        }
        // typenode_value_to_type returned None — this Variant is not representable as a static
        // TypeValue at this scope depth (e.g., not a TypeNode variant, or references type-stage
        // functions that haven't been called yet, or opaque types that need parameterization).
        return crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags);
    }
    let maybe_kind = match crate::type_normalize::typenode_typevar_kind(val) {
        Ok(v) => v,
        Err(e) => {
            return crate::error::DiagnosticResult::err((*e).clone())
                .with_diagnostics(_dr_diags)
        }
    };
    if let Some(kind) = maybe_kind {
        let tv = crate::type_infer::make_typevalue_nominal(
            type_name_to_id.get(name).copied().unwrap_or(0),
        );
        return crate::error::DiagnosticResult::ok(Some((tv, None, Some(kind))))
            .with_diagnostics(_dr_diags);
    }
    if matches!(val, crate::value::Value::Function { .. }) {
        let tv = crate::type_infer::make_typevalue_nominal(
            type_name_to_id.get(name).copied().unwrap_or(0),
        );
        return crate::error::DiagnosticResult::ok(Some((tv, Some(std::sync::Arc::clone(thunk)), None)))
            .with_diagnostics(_dr_diags);
    }
    // Not a recognized Variant, TypeVar sentinel, or Function — not a type-stage entry.
    crate::error::DiagnosticResult::ok(None).with_diagnostics(_dr_diags)
}

/// Inner implementation of `build_builtin_core_envs`.
///
/// Parses `stdlib/builtin_core.llt` (embedded at compile time via `include_str!`),
/// runs the full pipeline (desugar → resolve → typecheck), and returns the
/// resulting `TyConEnv` and the evaluated type-stage scope
/// (used to seed `InferState::type_stage_scope`).
async fn build_builtin_core_envs_inner() -> crate::error::DiagnosticResult<(
    crate::type_def::TyConEnv,
    crate::type_infer::TypeStageData,
    std::collections::HashMap<String, u64>,
)> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
    let source = include_str!("../stdlib/builtin_core.llt");
    let sf: Arc<str> = Arc::from("stdlib/builtin_core.llt");

    let (parse_result, parse_diags) = crate::parser::parse(source, sf).into_parts();
    let program = crate::desugar::desugar_program_full(
        &match parse_result {
            Ok(v) => v,
            Err(e) => {
                _dr_diags.extend(parse_diags);
                return crate::error::DiagnosticResult::err(crate::error::Diagnostic::error(
                    KIND,
                    format!("builtin_core.llt parse failed: {e}"),
                    crate::rust_span!(),
                ))
                .with_diagnostics(_dr_diags)
            }
        }
        .program,
    );
    _dr_diags.extend(parse_diags);
    let program = crate::expand::expand_program(&program);

    let ctx = crate::eval::EvalContext::new_empty();
    let mut state = crate::pipeline::PipelineState::from_builtins(&ctx);

    for doc in &program.documents {
        let (result, diags) = crate::pipeline::process_pipeline_document(doc, &mut state, &ctx)
            .await
            .into_parts();
        _dr_diags.extend(diags);
        match result {
            Ok(()) => {}
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        }
    }

    crate::error::DiagnosticResult::ok((
        state.infer_state.tycon_env,
        state.infer_state.type_stage_data,
        state.infer_state.type_name_to_id,
    ))
    .with_diagnostics(_dr_diags)
}
