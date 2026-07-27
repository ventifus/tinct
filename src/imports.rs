//! Bootstrap type environment for the type checker.
//!
//! Provides `build_builtin_core_envs()` which parses and type-checks
//! `stdlib/builtin_core.llt` to build the initial type environment seeded
//! with core type definitions (`Handle`, `DirCap`, and other prelude-defined types).
//!
//! Include resolution for user programs is handled by the prelude's `include`
//! function, which runs the full pipeline (parse → desugar → resolve →
//! typecheck → eval) via `builtin-typecheck-doc`.

use std::sync::{Arc, RwLock};

use crate::env::Env;
use crate::typecheck::typecheck_surface_program_with_env;

/// Parse and type-check `stdlib/builtin_core.llt`, returning the resulting
/// `(Arc<RwLock<Env>>, TyConEnv)` pair so that callers receive both the type
/// environment and the tycon definitions in one call.
///
/// Uses `include_str!` so the file is embedded at compile time — no runtime libdir access
/// needed.
pub async fn build_builtin_core_envs() -> (Arc<RwLock<Env>>, crate::type_def::TyConEnv) {
    let (env, tycon_env, _) = build_builtin_core_envs_inner()
        .await
        .expect("builtin_core.llt is embedded at compile time — failure is a programmer error");
    (env, tycon_env)
}

/// Evaluate the type-stage section of `stdlib/builtin_core.llt` and return the resulting
/// `type_stage_scope` for use by type-checker entry points that do not have an evaluated
/// type-stage scope available (e.g., corpus tests, LSP, formatter).
///
/// This is the authoritative source of the type-stage scope; no Rust code should manually
/// construct this mapping by hardcoding tinct-side type names.
pub async fn get_builtin_core_type_stage_scope(
) -> Vec<std::collections::HashMap<String, crate::type_infer::TypeStageEntry>> {
    let (_, _, scope) = build_builtin_core_envs_inner()
        .await
        .expect("builtin_core.llt is embedded at compile time — failure is a programmer error");
    scope
}

/// Type-check `stdlib/builtin_core.llt` and return the resulting `TypeEnv` so that
/// prelude-defined types, `Handle`, `builtin-raise`, etc. are visible to the prelude
/// type-checker by their bare names.
///
/// Uses `include_str!` so the file is embedded at compile time — no runtime libdir access
/// needed.
pub async fn get_builtin_core_type_env() -> Arc<RwLock<Env>> {
    let (env, _, _) = build_builtin_core_envs_inner()
        .await
        .expect("builtin_core.llt is embedded at compile time — failure is a programmer error");
    env
}

/// Type-check `stdlib/builtin_core.llt` and return the resulting `TyConEnv` containing
/// opaque type definitions (e.g. `BuilderHandle`, `DirCap`).
pub async fn get_builtin_core_tycon_env() -> crate::type_def::TyConEnv {
    let (_, tycon_env, _) = build_builtin_core_envs_inner()
        .await
        .expect("builtin_core.llt is embedded at compile time — failure is a programmer error");
    tycon_env
}

/// Classify a single materialized type-stage value into a `TypeStageEntry`.
///
/// Three cases are handled in precedence order:
/// 1. Variant value → `typenode_value_to_type` attempts conversion to a `Type`.
///    Returns `TypeStageEntry::Resolved` on success. Structural types like Span and
///    Diagnostic produce precise record types with named fields because constructor
///    payload entries use `VarAddr::Parameter` (resolved at force time in the call
///    frame that holds the constructor arguments).
/// 2. TypeVar kind sentinel (Operator, Label) → `TypeStageEntry::TypeVar` via `typenode_typevar_kind`
/// 3. `Value::Function` → `TypeStageEntry::Function` holding the thunk for later parameterized-type calls
///
/// Returns `Ok(Some(entry))` when the value maps to a known TypeStageEntry kind,
/// `Ok(None)` when the value is a Variant that cannot be converted at the current scope depth,
/// and `Err(_)` when `typenode_value_to_type` returns a propagated eval error (caller decides
/// whether to panic or propagate).
pub(crate) async fn classify_type_stage_entry(
    thunk: &std::sync::Arc<crate::value::Thunk>,
    val: &crate::value::Value,
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
    scope_so_far: &[std::collections::HashMap<String, crate::type_infer::TypeStageEntry>],
) -> crate::error::EvalResult<Option<crate::type_infer::TypeStageEntry>> {
    // For Variant values: call typenode_value_to_type (full recursive converter that reads
    // payload fields for structural types like Span, Diagnostic, Dict, Union, etc.).
    // typenode_value_to_type returns Ok(None) for non-TypeNode variants, so calling it
    // unconditionally is correct — no Rust code here needs to know the TypeNode tycon name.
    if matches!(val, crate::value::Value::Variant { .. }) {
        if let Some(ty) =
            crate::typecheck::typecheck_annot::typenode_value_to_type(val, ctx, scope_so_far)
                .await?
        {
            return Ok(Some(crate::type_infer::TypeStageEntry::Resolved(ty)));
        }
        // typenode_value_to_type returned None — this Variant is not representable as a static
        // Type at this scope depth (e.g., not a TypeNode variant, or references type-stage
        // functions that haven't been called yet, or opaque types that need parameterization).
        return Ok(None);
    }
    if let Some(kind) = crate::type_normalize::typenode_typevar_kind(val) {
        return Ok(Some(crate::type_infer::TypeStageEntry::TypeVar(kind)));
    }
    if matches!(val, crate::value::Value::Function { .. }) {
        return Ok(Some(crate::type_infer::TypeStageEntry::Function(
            std::sync::Arc::clone(thunk),
        )));
    }
    // Not a recognized Variant, TypeVar sentinel, or Function — not a type-stage entry.
    Ok(None)
}

/// Inner implementation of `build_builtin_core_envs`.
///
/// Parses `stdlib/builtin_core.llt` (embedded at compile time via `include_str!`),
/// runs the full pipeline (desugar → resolve → typecheck), and returns the
/// resulting `Arc<RwLock<Env>>` with the new type declarations merged on top of
/// `build_builtins_type_env_arc()` as the parent, along with the `TyConEnv` and the
/// evaluated type-stage scope (used to seed `InferState::type_stage_scope`).
async fn build_builtin_core_envs_inner() -> crate::error::EvalResult<(
    Arc<RwLock<Env>>,
    crate::type_def::TyConEnv,
    Vec<std::collections::HashMap<String, crate::type_infer::TypeStageEntry>>,
)> {
    // Embedded source — no libdir access needed at runtime.
    let source = include_str!("../stdlib/builtin_core.llt");
    let sf: Arc<str> = Arc::from("stdlib/builtin_core.llt");

    // Parse — extract .program from ParseOutput.
    // builtin_core.llt is embedded at compile time; parse failure is a programmer error.
    let program = crate::desugar::desugar_program_full(
        &crate::parser::parse(source, sf)
            .map_err(|e| {
                Box::new(crate::error::EvalError::internal(
                    format!("builtin_core.llt failed to parse — file is embedded at compile time and must be valid: {e}"),
                    crate::rust_span!(),
                ))
            })?
            .program,
    );

    // Empty parent — builtin_core.llt is the source of truth. Primitives are hardcoded
    // in resolve_type_name; types declared within the file resolve via state.tycon_env.
    let parent_env = Arc::new(RwLock::new(crate::env::Env::new()));

    // Create EvalContext for type-stage evaluation. No filesystem access needed —
    // type-stage eval only constructs TypeNode values in memory (no IO builtins).
    let type_stage_eval_ctx = crate::eval::EvalContext::new_empty();

    // ONE resolve pass on the FULL program with the eval root frame as the outer scope.
    // This sets OnceLock coordinates on ALL nodes (including type-stage ones) relative to
    // the EvalContext's root group, exactly as run_loader_pipeline does. The resulting
    // scope frames are threaded into eval_ctx_with_frames so the evaluator can resolve
    // VarRefs at any level — including unit constructor accesses like TypeNode.Int which
    // require a working outer-frame lookup chain.
    let eval_ctx_with_frames: std::sync::Arc<crate::eval::EvalContext> = {
        let root_frame = type_stage_eval_ctx.root_group_resolver_map();
        let (_table, new_frames) =
            crate::resolve::resolve_surface_program(&program, std::slice::from_ref(&root_frame));
        let all_frames: Vec<indexmap::IndexMap<String, u32>> =
            std::iter::once(root_frame).chain(new_frames).collect();
        type_stage_eval_ctx.with_scope_frames(std::sync::Arc::new(all_frames))
    };

    // Filter type-stage documents from the already-resolved full program.
    // The OnceLocks are already set by the resolve above — no separate resolve needed.
    let ts_docs: Vec<_> = program
        .documents
        .iter()
        .filter(|d| {
            d.node.header.get("stage").is_some_and(|stage_node| {
                matches!(
                    &stage_node.expr,
                    crate::ast::SurfaceExpression::StringLiteral { content, .. }
                    if content == "type"
                )
            })
        })
        .cloned()
        .collect();

    // Build type_stage_scope by evaluating the type-stage documents with the correct context.
    // eval_ctx_with_frames has the scope frames from the resolve pass, so VarRefs at any
    // nesting level resolve correctly — including unit constructors (TypeNode.Int) which
    // require outer-group frame lookup, not just builtins.
    let type_stage_scope: Vec<
        std::collections::HashMap<String, crate::type_infer::TypeStageEntry>,
    > = if ts_docs.is_empty() {
        Vec::new()
    } else {
        let ts_program = crate::ast::SurfaceProgram { documents: ts_docs };
        let ts_thunk = crate::eval::eval_surface_file(&ts_program, &eval_ctx_with_frames)
            .await
            .map_err(|e| {
                Box::new(crate::error::EvalError::internal(
                    format!("builtin_core.llt type-stage eval failed — file is embedded at compile time and must be valid: {e}"),
                    crate::rust_span!(),
                ))
            })?;
        let ts_val = crate::eval::materialize(&ts_thunk, None, &eval_ctx_with_frames)
            .await
            .map_err(|e| {
                Box::new(crate::error::EvalError::internal(
                    format!("builtin_core.llt type-stage materialization failed — file is embedded at compile time and must be valid: {e}"),
                    crate::rust_span!(),
                ))
            })?;
        match ts_val {
            crate::value::Value::Dict(entries) => {
                let mut map = std::collections::HashMap::new();
                for (key, thunk) in &entries {
                    if let crate::value::HashableValue::Str(name) = key {
                        let val = crate::eval::materialize(thunk, None, &eval_ctx_with_frames)
                            .await
                            .map_err(|e| {
                                Box::new(crate::error::EvalError::internal(
                                    format!("builtin_core.llt type-stage entry materialization failed — file is embedded at compile time and must be valid: {e}"),
                                    crate::rust_span!(),
                                ))
                            })?;

                        // scope_so_far is built incrementally: only entries already
                        // processed are visible when classify_type_stage_entry runs.
                        // This means type-stage entries that reference other type-stage
                        // entries (e.g., ElementType referencing FieldType) must be
                        // declared AFTER their dependencies in builtin_core.llt and
                        // prelude.llt.  A future reordering of those declarations could
                        // silently drop entries that can't resolve their references.
                        let scope_so_far = vec![map.clone()];
                        let entry = classify_type_stage_entry(
                            thunk,
                            &val,
                            &eval_ctx_with_frames,
                            &scope_so_far,
                        )
                        .await?;
                        if let Some(e) = entry {
                            map.insert(name.to_string(), e);
                        }
                    }
                }
                vec![map]
            }
            _ => {
                return Err(Box::new(crate::error::EvalError::internal(
                    format!(
                        "builtin_core.llt type-stage produced non-Dict value: {:?}",
                        ts_val
                    ),
                    crate::rust_span!(),
                )));
            }
        }
    };

    // Collect opaque TyCon names from the type-stage scope before passing it to typecheck.
    // Each TypeStageEntry::Resolved(Type::TyCon(name)) in the type-stage scope represents
    // an opaque builtin type that needs a TyConDef so value_matches_type can dispatch on it.
    // We scan here (before the move into typecheck_surface_program_with_env) and register
    // the TyConDefs after typecheck returns with the mutably-accessible `state`.
    let opaque_tycon_names: Vec<String> = type_stage_scope
        .iter()
        .flat_map(|scope_map| scope_map.iter())
        .filter_map(|(_tname, entry)| {
            if let crate::type_infer::TypeStageEntry::Resolved(crate::types::Type::TyCon(
                ref discriminant,
            )) = *entry
            {
                Some(discriminant.clone())
            } else {
                None
            }
        })
        .collect();

    // Typecheck with builtins env as parent.
    // enable_hover_map=false (no LSP hover needed for bootstrap).
    // Clone type_stage_scope so it can be returned to callers that need it
    // (e.g. get_builtin_core_type_stage_scope, which seeds type-checker entry points
    // that do not have their own type-stage scope available).
    let type_stage_scope_for_return = type_stage_scope.clone();
    let tc_result = typecheck_surface_program_with_env(
        &program,
        parent_env,
        false,                            // enable_hover_map
        std::collections::HashMap::new(), // seed_tycon_env: empty at bootstrap
        None,                             // eval_ctx: no EvalContext at bootstrap
        Some(type_stage_scope),           // type_stage_scope from evaluating type-stage docs
    )
    .await;
    let final_env = tc_result.env;
    let mut state = tc_result.state;

    // Auto-register TyConDefs for opaque builtin types derived from the type-stage scope.
    // These types are declared as TypeNode leaf constructors in the type-stage (not as
    // [type X] in the runtime dict), so the typecheck pass does not create their TyConDefs
    // automatically. builtin_type: Some(discriminant) enables value_matches_type dispatch.
    // or_insert_with: if a [type X] declaration already registered an entry, keep it.
    use crate::type_def::TyConDef;
    for discriminant in opaque_tycon_names {
        state
            .tycon_env
            .entry(discriminant.clone())
            .or_insert_with(|| {
                std::sync::Arc::new(TyConDef {
                    params: vec![],
                    body: crate::types::Type::Unknown,
                    constraints: vec![],
                    variance: vec![],
                    constructors: vec![],
                    builtin_type: Some(discriminant.clone()),
                    annotation: None,
                    field_annotations: indexmap::IndexMap::new(),
                    constructor_constants: indexmap::IndexMap::new(),
                    definition_span: None,
                })
            });
    }

    // `final_env` is the child Env containing parent bindings plus new type declarations.
    Ok((final_env, state.tycon_env, type_stage_scope_for_return))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that `Span` and `Diagnostic` appear in the type-stage scope returned by
    /// `get_builtin_core_type_stage_scope` with precise record types (named fields), not
    /// generic open dicts.
    ///
    /// These types are declared as complex TypeNode.Dict values in `builtin_core.llt`.
    /// Constructor payload entries use `VarAddr::Parameter` (not `ClosureCapture`), so
    /// `variant_payload_dict` can materialize payload fields in any context.
    /// `typenode_value_to_type` produces precise record types with the correct named
    /// fields from builtin_core.llt:
    /// - Span: file, start-line, start-col, end-line, end-col
    /// - Diagnostic: level, kind, message, span, help, notes, secondary-spans, call-stack, macro-expand, blame
    #[tokio::test]
    async fn test_span_and_diagnostic_in_type_stage_scope() {
        use crate::type_def::Type;
        use crate::type_infer::TypeStageEntry;

        let scope = get_builtin_core_type_stage_scope().await;

        let all_keys: Vec<&str> = scope.iter().flatten().map(|(k, _)| k.as_str()).collect();

        // ── Span ───────────────────────────────────────────────────────────────
        // Span is declared as TypeNode.Dict with fields: file, start-line, start-col, end-line, end-col.
        let span_entry = scope
            .iter()
            .flatten()
            .find(|(k, _)| *k == "Span")
            .map(|(_, v)| v);
        assert!(
            span_entry.is_some(),
            "Span must appear in the type-stage scope; got keys: {:?}",
            all_keys
        );
        match span_entry.unwrap() {
            TypeStageEntry::Resolved(Type::Dict(row)) => {
                // Verify that Span has the expected named fields, not a generic open dict.
                assert!(
                    row.fields.contains_key("file"),
                    "Span must have 'file' field; got fields: {:?}",
                    row.fields.keys().collect::<Vec<_>>()
                );
                assert!(
                    row.fields.contains_key("start-line"),
                    "Span must have 'start-line' field; got fields: {:?}",
                    row.fields.keys().collect::<Vec<_>>()
                );
                assert!(
                    row.fields.contains_key("start-col"),
                    "Span must have 'start-col' field; got fields: {:?}",
                    row.fields.keys().collect::<Vec<_>>()
                );
                assert!(
                    row.fields.contains_key("end-line"),
                    "Span must have 'end-line' field; got fields: {:?}",
                    row.fields.keys().collect::<Vec<_>>()
                );
                assert!(
                    row.fields.contains_key("end-col"),
                    "Span must have 'end-col' field; got fields: {:?}",
                    row.fields.keys().collect::<Vec<_>>()
                );
            }
            other => panic!(
                "Span must resolve to Type::Dict with named fields; got: {:?}",
                other
            ),
        }

        // ── Diagnostic ─────────────────────────────────────────────────────────
        let diagnostic_entry = scope
            .iter()
            .flatten()
            .find(|(k, _)| *k == "Diagnostic")
            .map(|(_, v)| v);
        assert!(
            diagnostic_entry.is_some(),
            "Diagnostic must appear in the type-stage scope; got keys: {:?}",
            all_keys
        );
        match diagnostic_entry.unwrap() {
            TypeStageEntry::Resolved(Type::Dict(row)) => {
                // Verify that Diagnostic has the expected named fields, not a generic open dict.
                let expected_fields = [
                    "level",
                    "kind",
                    "message",
                    "span",
                    "help",
                    "notes",
                    "secondary-spans",
                    "call-stack",
                    "macro-expand",
                    "blame",
                ];
                for field in &expected_fields {
                    assert!(
                        row.fields.contains_key(*field),
                        "Diagnostic must have '{}' field; got fields: {:?}",
                        field,
                        row.fields.keys().collect::<Vec<_>>()
                    );
                }
            }
            other => panic!(
                "Diagnostic must resolve to Type::Dict with named fields; got: {:?}",
                other
            ),
        }

        // ── CallFrame ──────────────────────────────────────────────────────────
        let has_callframe = scope.iter().flatten().any(|(k, _)| k == "CallFrame");
        assert!(
            has_callframe,
            "CallFrame must appear in the type-stage scope; got keys: {:?}",
            all_keys
        );

        // ── SecondarySpan ──────────────────────────────────────────────────────
        let has_secondary_span = scope.iter().flatten().any(|(k, _)| k == "SecondarySpan");
        assert!(
            has_secondary_span,
            "SecondarySpan must appear in the type-stage scope; got keys: {:?}",
            all_keys
        );

        // ── Diagnostics ────────────────────────────────────────────────────────
        let has_diagnostics = scope.iter().flatten().any(|(k, _)| k == "Diagnostics");
        assert!(
            has_diagnostics,
            "Diagnostics must appear in the type-stage scope; got keys: {:?}",
            all_keys
        );
    }
}
