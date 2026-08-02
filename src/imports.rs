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
use crate::type_tags::*;
use crate::typecheck::typecheck_program_bootstrap;

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
pub async fn get_builtin_core_type_stage_scope() -> crate::type_infer::TypeStageData {
    let (_, _, data) = build_builtin_core_envs_inner()
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

/// Classify a single materialized type-stage value.
///
/// Three cases are handled in precedence order:
/// 1. Variant value → `typenode_value_to_type` attempts conversion to a TypeValue.
///    Returns `Some((tv, None, None))` on success (Resolved entry). Structural types like Span
///    and Diagnostic produce precise record types with named fields.
/// 2. TypeVar kind sentinel (Operator, Label) → `typenode_typevar_kind`. Returns
///    `Some((make_typevalue_op(name), None, Some(kind)))` where the third element is the kind.
/// 3. `Value::Function` → parameterized type constructor. Returns
///    `Some((make_typevalue_op(name), Some(thunk), None))` where the second element is the thunk.
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
        if let Some(ty) =
            crate::typecheck::typecheck_annot::typenode_value_to_type(val, ctx, scope_so_far)
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
        let tv = crate::type_infer::make_typevalue_op(name);
        return Ok(Some((tv, None, Some(kind))));
    }
    if matches!(val, crate::value::Value::Function { .. }) {
        let tv = crate::type_infer::make_typevalue_op(name);
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
        let (_table, new_frames_with_kind) =
            crate::resolve::resolve_surface_program(&program, std::slice::from_ref(&root_frame));
        // Extract just the frames, discarding FrameKind metadata
        let new_frames: Vec<indexmap::IndexMap<String, u32>> = new_frames_with_kind
            .into_iter()
            .map(|(frame, _kind)| frame)
            .collect();
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

    // Build type_stage_data by evaluating the type-stage documents with the correct context.
    // eval_ctx_with_frames has the scope frames from the resolve pass, so VarRefs at any
    // nesting level resolve correctly — including unit constructors (TypeNode.Int) which
    // require outer-group frame lookup, not just builtins.
    let type_stage_data: crate::type_infer::TypeStageData = if ts_docs.is_empty() {
        crate::type_infer::TypeStageData::new()
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
            crate::value::Value::Dict { entries, .. } => {
                // scope_map accumulates the resolved TypeValues for already-processed entries.
                // Used by typenode_value_to_type to resolve recursive TypeNode references.
                let mut scope_map: std::collections::HashMap<String, crate::type_infer::TypeValue> =
                    std::collections::HashMap::new();
                let mut fns_map: std::collections::HashMap<
                    String,
                    std::sync::Arc<crate::value::Thunk>,
                > = std::collections::HashMap::new();
                let mut type_vars_map: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();
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
                        let scope_so_far = vec![scope_map.clone()];
                        let classified = classify_type_stage_entry(
                            name,
                            thunk,
                            &val,
                            &eval_ctx_with_frames,
                            &scope_so_far,
                        )
                        .await?;
                        if let Some((tv, opt_thunk, opt_kind)) = classified {
                            scope_map.insert(name.to_string(), Arc::clone(&tv));
                            if let Some(thunk_arc) = opt_thunk {
                                fns_map.insert(name.to_string(), thunk_arc);
                            }
                            if let Some(kind_str) = opt_kind {
                                type_vars_map.insert(name.to_string(), kind_str);
                            }
                        }
                    }
                }
                crate::type_infer::TypeStageData {
                    scope: vec![scope_map],
                    fns: fns_map,
                    type_vars: type_vars_map,
                }
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

    // Insert type-stage bindings into parent_env by name.
    // This enables name-based lookup (get_scheme, resolve_type_head, etc.) from any
    // child env frame. Slot-based insertion (for get_scheme_at) happens in
    // typecheck_program_bootstrap after the resolver assigns the correct runtime slots.
    // Type-stage names have no pre-assigned slots in parent_env at this point — append.
    {
        let mut env_write = parent_env.write().unwrap();
        // Resolved entries: insert their TypeValue directly.
        for (name, tv) in type_stage_data.scope.iter().flat_map(|m| m.iter()) {
            let slot = env_write.slots.len();
            env_write.insert_at_slot(slot, name.clone(), Arc::clone(tv), None);
        }
        // Function entries: insert make_typevalue_op placeholder.
        for (name, _thunk) in &type_stage_data.fns {
            let slot = env_write.slots.len();
            env_write.insert_at_slot(
                slot,
                name.clone(),
                crate::type_infer::make_typevalue_op(name),
                None,
            );
        }
        // TypeVar entries: insert make_typevalue_op placeholder.
        for (name, _kind) in &type_stage_data.type_vars {
            let slot = env_write.slots.len();
            env_write.insert_at_slot(
                slot,
                name.clone(),
                crate::type_infer::make_typevalue_op(name),
                None,
            );
        }
    }

    // Collect opaque TyCon names from the resolved type-stage scope before passing to typecheck.
    // Each TypeValue.Op in the resolved scope represents an opaque builtin type that needs a
    // TyConDef so value_matches_type can dispatch on it. We scan here (before the move into
    // typecheck_program_bootstrap) and register the TyConDefs after typecheck returns.
    let opaque_tycon_names: Vec<String> = type_stage_data
        .scope
        .iter()
        .flat_map(|scope_map| scope_map.iter())
        .filter_map(|(_tname, tv)| {
            // After S-1003: TypeValue.Op { name: String } replaces Type::TyCon(name).
            if let crate::value::Value::Variant {
                ctor,
                payload: Some(payload_thunk),
                ..
            } = tv.as_ref()
            {
                if ctor.as_ref() == TV_OP {
                    if let Some(Ok(crate::value::Value::Dict { entries, .. })) =
                        payload_thunk.peek_result()
                    {
                        let key =
                            crate::value::HashableValue::Str(std::sync::Arc::from(FIELD_NAME));
                        if let Some(Ok(crate::value::Value::String {
                            source, start, end, ..
                        })) = entries.get(&key).map(|t| t.peek_result()).flatten()
                        {
                            return Some(source[*start..*end].to_string());
                        }
                    }
                }
            }
            None
        })
        .collect();

    // Typecheck with builtins env as parent.
    // Clone type_stage_data so it can be returned to callers that need it
    // (e.g. get_builtin_core_type_stage_scope, which seeds type-checker entry points
    // that do not have their own type-stage scope available).
    let type_stage_data_for_return = type_stage_data.clone();
    let (_tc_errors, final_env, mut tycon_env) = typecheck_program_bootstrap(
        &program,
        parent_env,
        None,                             // eval_ctx: no EvalContext at bootstrap
        std::collections::HashMap::new(), // seed_tycon_env: empty at bootstrap
        type_stage_data,
    )
    .await;

    // Auto-register TyConDefs for opaque builtin types derived from the type-stage scope.
    // These types are declared as TypeNode leaf constructors in the type-stage (not as
    // [type X] in the runtime dict), so the typecheck pass does not create their TyConDefs
    // automatically. builtin_type: Some(discriminant) enables value_matches_type dispatch.
    // or_insert_with: if a [type X] declaration already registered an entry, keep it.
    use crate::type_def::TyConDef;
    for discriminant in opaque_tycon_names {
        tycon_env.entry(discriminant.clone()).or_insert_with(|| {
            std::sync::Arc::new(TyConDef {
                params: vec![],
                body: crate::value::unknown_type_val(),
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
    Ok((final_env, tycon_env, type_stage_data_for_return))
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
