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

/// Parse and type-check `stdlib/builtin_core.llt`, returning the resulting
/// `(Arc<RwLock<Env>>, TyConEnv)` pair so that callers receive both the type
/// environment and the tycon definitions in one call.
///
/// Uses `include_str!` so the file is embedded at compile time — no runtime libdir access
/// needed.
pub async fn build_builtin_core_envs() -> (
    Arc<RwLock<Env>>,
    crate::type_def::TyConEnv,
    std::collections::HashMap<String, u64>,
) {
    let (env, tycon_env, _, type_name_to_id) = build_builtin_core_envs_inner()
        .await
        .expect("builtin_core.llt is embedded at compile time — failure is a programmer error");
    (env, tycon_env, type_name_to_id)
}

/// Evaluate the type-stage section of `stdlib/builtin_core.llt` and return the resulting
/// `type_stage_scope` for use by type-checker entry points that do not have an evaluated
/// type-stage scope available (e.g., corpus tests, LSP, formatter).
///
/// This is the authoritative source of the type-stage scope; no Rust code should manually
/// construct this mapping by hardcoding tinct-side type names.
pub async fn get_builtin_core_type_stage_scope() -> crate::type_infer::TypeStageData {
    let (_, _, data, _) = build_builtin_core_envs_inner()
        .await
        .expect("builtin_core.llt is embedded at compile time — failure is a programmer error");
    data
}

/// Type-check `stdlib/builtin_core.llt` and return the resulting `TypeEnv` so that
/// prelude-defined types, `Handle`, `builtin-raise`, etc. are visible to the prelude
/// type-checker by their bare names.
///
/// Uses `include_str!` so the file is embedded at compile time — no runtime libdir access
/// needed.
pub async fn get_builtin_core_type_env() -> Arc<RwLock<Env>> {
    let (env, _, _, _) = build_builtin_core_envs_inner()
        .await
        .expect("builtin_core.llt is embedded at compile time — failure is a programmer error");
    env
}

/// Type-check `stdlib/builtin_core.llt` and return the resulting `TyConEnv` containing
/// opaque type definitions (e.g. `BuilderHandle`, `DirCap`).
pub async fn get_builtin_core_tycon_env() -> crate::type_def::TyConEnv {
    let (_, tycon_env, _, _) = build_builtin_core_envs_inner()
        .await
        .expect("builtin_core.llt is embedded at compile time — failure is a programmer error");
    tycon_env
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
) -> crate::error::EvalResult<
    Option<(
        crate::type_infer::TypeValue,
        Option<std::sync::Arc<crate::value::Thunk>>,
        Option<String>,
    )>,
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
/// resulting `Arc<RwLock<Env>>` with the new type declarations merged on top of
/// `build_builtins_type_env_arc()` as the parent, along with the `TyConEnv` and the
/// evaluated type-stage scope (used to seed `InferState::type_stage_scope`).
async fn build_builtin_core_envs_inner() -> crate::error::EvalResult<(
    Arc<RwLock<Env>>,
    crate::type_def::TyConEnv,
    crate::type_infer::TypeStageData,
    std::collections::HashMap<String, u64>,
)> {
    let source = include_str!("../stdlib/builtin_core.llt");
    let sf: Arc<str> = Arc::from("stdlib/builtin_core.llt");

    let program = crate::desugar::desugar_program_full(
        &crate::parser::parse(source, sf)
            .map_err(|e| {
                Box::new(crate::error::EvalError::internal(
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
        state.env,
        state.tycon_env,
        state.type_stage_data,
        state.type_name_to_id,
    ))
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
        let data = get_builtin_core_type_stage_scope().await;
        let scope = &data.scope;

        let all_keys: Vec<&str> = scope.iter().flatten().map(|(k, _)| k.as_str()).collect();

        // Helper to extract field names from a TypeValue.Record.
        fn extract_record_field_names(tv: &std::sync::Arc<crate::value::Value>) -> Vec<String> {
            use crate::value::{HashableValue, Value};
            match tv.as_ref() {
                Value::Variant {
                    ctor,
                    payload: Some(p),
                    ..
                } if ctor.as_ref() == TV_RECORD => match p.peek_result() {
                    Some(Ok(Value::Dict { entries, .. })) => {
                        let fk = HashableValue::Str(std::sync::Arc::from(FIELD_FIELDS));
                        match entries.get(&fk).and_then(|t| t.peek_result()) {
                            Some(Ok(Value::Dict { entries: fe, .. })) => fe
                                .keys()
                                .filter_map(|k| {
                                    if let HashableValue::Str(s) = k {
                                        Some(s.as_ref().to_string())
                                    } else {
                                        None
                                    }
                                })
                                .collect(),
                            _ => vec![],
                        }
                    }
                    _ => vec![],
                },
                _ => vec![],
            }
        }

        // ── Span ───────────────────────────────────────────────────────────────
        // Span is declared as TypeNode.Dict with fields: file, start-line, start-col, end-line, end-col.
        let span_tv = scope
            .iter()
            .flatten()
            .find(|(k, _)| *k == "Span")
            .map(|(_, v)| v);
        assert!(
            span_tv.is_some(),
            "Span must appear in the type-stage scope; got keys: {:?}",
            all_keys
        );
        {
            let tv = span_tv.unwrap();
            // Verify that Span resolves to a TypeValue.Record with the expected fields.
            let field_names = extract_record_field_names(tv);
            for expected in &["file", "start-line", "start-col", "end-line", "end-col"] {
                assert!(
                    field_names.contains(&expected.to_string()),
                    "Span must have '{}' field; got fields: {:?}",
                    expected,
                    field_names
                );
            }
        }

        // ── Diagnostic ─────────────────────────────────────────────────────────
        let diagnostic_tv = scope
            .iter()
            .flatten()
            .find(|(k, _)| *k == "Diagnostic")
            .map(|(_, v)| v);
        assert!(
            diagnostic_tv.is_some(),
            "Diagnostic must appear in the type-stage scope; got keys: {:?}",
            all_keys
        );
        {
            let tv = diagnostic_tv.unwrap();
            let field_names = extract_record_field_names(tv);
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
                    field_names.contains(&field.to_string()),
                    "Diagnostic must have '{}' field; got fields: {:?}",
                    field,
                    field_names
                );
            }
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

        // ── Dict (B-663 regression test) ───────────────────────────────────────
        // Dict must resolve to an open-record TypeValue (TypeValue.Record with empty
        // named-field set and Uniform(Top) tail), NOT TypeValue.Op("Dict").
        //
        // Before B-663 fix: builtin-make-type-ctx seeded type_stage_scope with Vec::new().
        // After uses-scope ["core"] wired TyConDefs, Dict → Op("Dict") was inserted.
        // This caused @Dict annotations to resolve to Op("Dict") instead of the
        // open-record type, producing "missing field 'Proxy': record subtype constraint"
        // errors when @Dict was used with record subtype checking.
        //
        // The fix: builtin-make-type-ctx now seeds with get_builtin_core_type_stage_scope(),
        // which has Dict → Resolved(open-record). The .or_insert() in TyConDef wiring
        // then preserves the correct entry.
        let dict_tv = scope
            .iter()
            .flatten()
            .find(|(k, _)| *k == "Dict")
            .map(|(_, v)| v);
        assert!(
            dict_tv.is_some(),
            "Dict must appear in the type-stage scope (B-663); got keys: {:?}",
            all_keys
        );
        // Dict must NOT be a Function entry.
        assert!(
            !data.fns.contains_key("Dict"),
            "Dict must NOT be a Function entry in type_stage_scope (B-663)"
        );
        {
            let tv = dict_tv.unwrap();
            // Must be TypeValue.Record (open-record), NOT TypeValue.Op("Dict").
            let ctor = match tv.as_ref() {
                crate::value::Value::Variant { ctor, .. } => Some(ctor.as_ref()),
                _ => None,
            };
            assert_eq!(
                ctor,
                Some(crate::type_tags::TV_RECORD),
                "Dict must resolve to TypeValue.Record (open-record), not {:?} (B-663)",
                ctor
            );
            // The record must have empty named fields and a Uniform tail.
            let field_names = extract_record_field_names(tv);
            assert!(
                field_names.is_empty(),
                "Dict record must have no named fields, got: {:?}",
                field_names
            );
        }
    }
}
