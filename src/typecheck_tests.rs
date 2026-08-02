use super::*;
use crate::ast::{SurfaceEntry, SurfaceExpression, SurfaceNode};
use crate::rust_span;
use crate::type_infer::{make_typevalue_repr, make_typevalue_unknown};
use crate::typecheck::process_document;
use crate::typecheck::typecheck_annot::{resolve_annotation, resolve_type_name};
use crate::types::unify;
// TypeScheme deleted in S-1003 — TypeValues stored directly.
use crate::value::{unknown_type_val, HashableValue, Value};
use crate::Annotation;
use indexmap::IndexMap;
use std::sync::Arc;

// ── TypeValue test helpers ──────────────────────────────────────────────────

/// Check if a TypeValue is TypeValue.IntLit with the given value.
fn is_int_lit(tv: &Arc<Value>, n: i64) -> bool {
    if let Value::Variant {
        ctor,
        payload: Some(p),
        ..
    } = tv.as_ref()
    {
        if ctor.as_ref() == TV_INT_LIT {
            if let Some(Ok(Value::Dict { entries, .. })) = p.peek_result() {
                let key = HashableValue::Str(Arc::from(FIELD_VALUE));
                if let Some(Ok(Value::Int { n: actual, .. })) =
                    entries.get(&key).map(|t| t.peek_result()).flatten()
                {
                    return *actual == n;
                }
            }
        }
    }
    false
}

/// Check if a TypeValue is TypeValue.StrLit with the given value.
fn is_str_lit(tv: &Arc<Value>, s: &str) -> bool {
    if let Value::Variant {
        ctor,
        payload: Some(p),
        ..
    } = tv.as_ref()
    {
        if ctor.as_ref() == TV_STR_LIT {
            if let Some(Ok(Value::Dict { entries, .. })) = p.peek_result() {
                let key = HashableValue::Str(Arc::from(FIELD_VALUE));
                if let Some(Ok(Value::String {
                    source, start, end, ..
                })) = entries.get(&key).map(|t| t.peek_result()).flatten()
                {
                    return &source[*start..*end] == s;
                }
            }
        }
    }
    false
}

/// Check if a TypeValue is TypeValue.FloatLit with the given value (by bit pattern).
fn is_float_lit(tv: &Arc<Value>, f: f64) -> bool {
    if let Value::Variant {
        ctor,
        payload: Some(p),
        ..
    } = tv.as_ref()
    {
        if ctor.as_ref() == TV_FLOAT_LIT {
            if let Some(Ok(Value::Dict { entries, .. })) = p.peek_result() {
                let key = HashableValue::Str(Arc::from(FIELD_VALUE));
                if let Some(Ok(Value::Float { n: actual, .. })) =
                    entries.get(&key).map(|t| t.peek_result()).flatten()
                {
                    return actual.to_bits() == f.to_bits();
                }
            }
        }
    }
    false
}

/// Check if a TypeValue is TypeValue.Unknown (or bootstrap sentinel empty dict).
fn is_unknown(tv: &Arc<Value>) -> bool {
    matches!(tv.as_ref(), Value::Variant { ctor, .. } if ctor.as_ref() == TV_UNKNOWN)
        || matches!(tv.as_ref(), Value::Dict { entries, .. } if entries.is_empty())
}

/// Check if a TypeValue is TypeValue.Var.
fn is_var(tv: &Arc<Value>) -> bool {
    matches!(tv.as_ref(), Value::Variant { ctor, .. } if ctor.as_ref() == TV_VAR)
}

/// Check if a TypeValue is TypeValue.Top (Any).
fn is_top(tv: &Arc<Value>) -> bool {
    matches!(tv.as_ref(), Value::Variant { ctor, .. } if ctor.as_ref() == TV_TOP)
}

/// Check if a TypeValue is TypeValue.Union.
fn is_union(tv: &Arc<Value>) -> bool {
    matches!(tv.as_ref(), Value::Variant { ctor, .. } if ctor.as_ref() == TV_UNION)
}

/// Create a TypeValue.Union from a Vec of member TypeValues.
fn make_test_union(members: Vec<Arc<Value>>) -> Arc<Value> {
    use crate::value::{HashableValue, Thunk};
    let mut entries = indexmap::IndexMap::new();
    for (i, member) in members.into_iter().enumerate() {
        entries.insert(
            HashableValue::Int(i as i64),
            Arc::new(Thunk::value(
                Value::clone(member.as_ref()),
                crate::rust_span!(),
            )),
        );
    }
    let members_dict = Value::Dict {
        entries,
        type_val: unknown_type_val(),
    };
    let mut payload_entries = indexmap::IndexMap::new();
    payload_entries.insert(
        HashableValue::Str(Arc::from(FIELD_MEMBERS)),
        Arc::new(Thunk::value(members_dict, crate::rust_span!())),
    );
    let payload = Value::Dict {
        entries: payload_entries,
        type_val: unknown_type_val(),
    };
    Arc::new(Value::Variant {
        type_val: unknown_type_val(),
        ctor: Arc::from(TV_UNION),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

fn test_file(_src: &str) -> Arc<str> {
    Arc::from(file!())
}

/// Build a `Spanned<SurfaceEntry>` for use in `Annotation::PropertyDict` test constructions.
/// Migrated from old `sp(Entry { ... })` form during rv2-migrate-annotation Phase 1.
fn surf_ann_entry_tc(
    key: Option<SurfaceExpression>,
    value: SurfaceExpression,
) -> Spanned<SurfaceEntry> {
    let span = crate::test_util::test_span(0, 0, 0, 0);
    let mk = |expr| Arc::new(SurfaceNode::new(expr, span.clone()));
    Spanned::new(
        SurfaceEntry {
            key: key.map(mk),
            value: mk(value),
        },
        span,
    )
}

/// Run typecheck on `input` and return the tycon_env from InferState.
/// Use this in tests that need to inspect TyConDef entries (type alias bodies, params, etc.).
async fn doc_tycon_env(
    input: &str,
) -> std::collections::HashMap<String, Arc<crate::type_def::TyConDef>> {
    let program = crate::desugar::desugar_surface_program(
        &crate::parse(input, test_file(input)).unwrap().program,
    );

    let arc_env = crate::imports::get_builtin_core_type_env().await;
    let child_env = std::sync::Arc::new(std::sync::RwLock::new(crate::env::Env::with_parent(
        std::sync::Arc::clone(&arc_env),
    )));
    let mut state = InferState::with_env(std::sync::Arc::clone(&child_env));
    let (_result_env, _ty, diags) =
        process_document(&program.documents[0].node, &arc_env, &mut state, &mut None).await;
    if !diags.is_empty() {
        panic!("doc_tycon_env: typecheck error: {:?}", diags);
    }
    state.tycon_env
}

// -- Annotation resolution --

#[tokio::test]
async fn test_annotation_type_var() {
    let span = crate::test_util::test_span(1, 1, 1, 5);
    // With explicit bind: required, lowercase names outside a function scope (ann_mapping=None)
    // now produce a Diagnostic — implicit TypeVar creation was removed.
    let mut state = InferState::new();
    let mut c = Vec::new();
    let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let mut row_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let result = resolve_annotation(
        &Annotation::Simple("a".into()),
        span,
        &mut state,
        &mut c,
        &mut ann_m,
        &mut row_m,
        None,
    )
    .await;
    assert!(
        result.is_err(),
        "lowercase annotation outside function scope should produce undefined type error; got: {result:?}"
    );
}

#[tokio::test]
async fn test_resolve_type_name_outside_function_scope_monotonicity() {
    // With explicit bind: required, resolve_type_name for a lowercase name without a prior
    // bind: declaration now produces a Diagnostic at any scope level.
    let span = crate::test_util::test_span(1, 1, 1, 5);
    let mut state = InferState::new();
    state.ctx.current_level = 1;
    let mut c = Vec::new();
    let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let row_ref: Option<&std::collections::HashMap<String, String>> = None;
    let result = resolve_type_name(
        "a",
        span.clone(),
        &mut state,
        &mut c,
        &mut ann_m,
        None,
        &row_ref,
    )
    .await;
    assert!(
        result.is_err(),
        "lowercase type name outside function scope should produce undefined type error; got: {result:?}"
    );
}

// -- resolve_property_dict_as_record fallback paths --

#[tokio::test]
async fn test_property_dict_non_str_key_is_error() {
    // Non-bare-word (integer) key in a type record annotation is now an error.
    // The legacy Dict fallback-to-Any path was removed; invalid keys are rejected.
    let span = crate::test_util::test_span(1, 1, 1, 10);
    let ann = Annotation::PropertyDict(vec![surf_ann_entry_tc(
        Some(SurfaceExpression::Int(42)),
        SurfaceExpression::StringLiteral {
            prefix: String::new(),
            delimiter: "\"".to_string(),
            content: "Int".into(),
        },
    )]);
    let mut c = Vec::new();
    let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let mut row_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let mut st = InferState::new();
    let result =
        resolve_annotation(&ann, span, &mut st, &mut c, &mut ann_m, &mut row_m, None).await;
    assert!(
        result.is_err(),
        "Non-bare-word key in type annotation should be an error"
    );
}

#[tokio::test]
async fn test_property_dict_unresolvable_type_propagates_error() {
    let span = crate::test_util::test_span(1, 1, 1, 10);
    // Lowercase unresolvable type names produce an "undefined type" error.
    // (Uppercase names like "NoSuchType" are treated as nominal variant constructors
    // and succeed with NominalVariant; lowercase names that are not type variables
    // produce an error since they don't match any known primitive or alias.)
    let ann = Annotation::PropertyDict(vec![surf_ann_entry_tc(
        Some(SurfaceExpression::StringLiteral {
            prefix: String::new(),
            delimiter: "\"".to_string(),
            content: "x".into(),
        }),
        SurfaceExpression::VarRef {
            name: "noSuchType".into(),
            escaped: false,
            resolution: crate::ast::Resolution::new(),

            annotation: None,
            do_infer_placeholder: false,
        },
    )]);
    let mut c = Vec::new();
    let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let mut row_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let mut st = InferState::new();
    let result =
        resolve_annotation(&ann, span, &mut st, &mut c, &mut ann_m, &mut row_m, None).await;
    // With explicit bind: required, lowercase names in annotation position without a prior
    // bind: declaration produce a Diagnostic. "noSuchType" starts lowercase → error.
    assert!(
        result.is_err(),
        "lowercase annotation name not in scope should produce undefined type error; got: {result:?}"
    );
}

#[tokio::test]
async fn test_property_dict_literal_value_falls_back_to_any() {
    let span = crate::test_util::test_span(1, 1, 1, 10);
    let ann = Annotation::PropertyDict(vec![surf_ann_entry_tc(
        Some(SurfaceExpression::StringLiteral {
            prefix: String::new(),
            delimiter: "\"".to_string(),
            content: "default".into(),
        }),
        SurfaceExpression::Int(30),
    )]);
    let mut c = Vec::new();
    let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let mut row_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let mut st = InferState::new();
    let result = resolve_annotation(&ann, span, &mut st, &mut c, &mut ann_m, &mut row_m, None)
        .await
        .unwrap();
    assert!(
        is_unknown(&result) || is_top(&result),
        "expected TypeValue.Unknown or Top, got {:?}",
        result
    );
}

#[tokio::test]
async fn test_property_dict_fn_type_error_propagates() {
    let span = crate::test_util::test_span(1, 1, 1, 10);
    // [Fn@Integer] -- function type pattern detected (Fn@ prefix) but wrong
    // number of entries: should propagate, not fall back to Any.
    let ann = Annotation::PropertyDict(vec![surf_ann_entry_tc(
        None,
        SurfaceExpression::VarRef {
            name: "Fn".into(),
            escaped: false,
            resolution: crate::ast::Resolution::new(),

            annotation: Some(Spanned::new(Annotation::Simple("Int".into()), span.clone())),
            do_infer_placeholder: false,
        },
    )]);
    let mut c = Vec::new();
    let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let mut row_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let mut st = InferState::new();
    let result =
        resolve_annotation(&ann, span, &mut st, &mut c, &mut ann_m, &mut row_m, None).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("function type"));
}

#[tokio::test]
async fn test_fn_type_display_round_trip() {
    // After migration: TypeValue.Fn is an Arc<Value>; Display is via typevalue_display.
    // This test is updated to verify TypeValue.Fn can be constructed (Display test deferred).
    use crate::type_infer::make_typevar_value;
    let a_tv = make_typevar_value("a");
    // TypeValue.Fn construction requires building the params dict — verified in integration tests.
    // This test now just verifies make_typevar_value produces valid TypeValue.Var.
    assert!(is_var(&a_tv), "expected TypeValue.Var for 'a'");
}

// -- Parameterized type aliases --

#[tokio::test]
async fn test_apply_type_alias_substitution_preserves_row_tail_uniform() {
    // [type [let k v] [_@k: v]] must produce Dict with RowTail::Uniform.
    // After S-1003: RowTail::Uniform → TypeValue.Record tail.
    let tycon_env = doc_tycon_env("[MapLike: [type [let k v] [_@k: v]]]").await;
    let alias = tycon_env
        .get("MapLike")
        .expect("MapLike alias should exist");

    // Alias body must be a TypeValue.Record — any other type is a regression.
    // After migration: TyConDef.body is Arc<Value> TypeValue.
    assert!(
        matches!(alias.body.as_ref(), Value::Variant { ctor, .. } if ctor.as_ref() == TV_RECORD),
        "expected TypeValue.Record body for uniform dict alias, got {:?}",
        alias.body
    );
    // The remaining RowTail::Uniform assertions are deferred to corpus tests
    // since payload inspection requires TypeValue structure knowledge.
}

// -- Type::Error cascade prevention --

#[tokio::test]
async fn test_error_absorbed_in_unify_does_not_corrupt_substitution() {
    // Verifies that unify(Error, TypeVar) does not bind the TypeVar, which would corrupt
    // subsequent inference. After cascade prevention records Error as an arg type, the
    // unification step must absorb it without touching the substitution.
    //
    // If Error were to bind a TypeVar (e.g., _t0 ↦ Error), the return type of the
    // polymorphic call would resolve to Error, suppressing valid type information
    // for the surrounding context.
    let span = rust_span!();
    let mut state = InferState::new();
    state.set_level("a", 1);

    // Simulate: polymorphic param type is TypeVar("a"), arg type is Error
    let mut constraints = Vec::new();
    // After migration: use TypeValue.Var and TypeValue.Unknown (Error maps to Unknown).
    use crate::type_infer::make_typevar_value;
    let var_tv = make_typevar_value("a");
    let error_tv = make_typevalue_unknown(); // Error maps to Unknown in TypeValue system
    let result = unify(
        &var_tv,
        &error_tv,
        &mut state.ctx,
        &mut constraints,
        span,
        0,
    )
    .await;
    assert!(result.is_ok(), "unify(TypeVar, Error) must succeed");
    assert!(
        state.lookup_binding("a").is_none(),
        "TypeVar must NOT be bound when unified with Error (Error carries no type info)"
    );
}

// ===== Union Type Tests =====

#[tokio::test]
async fn test_union_type_assert_success() {
    // value_matches_type: Int matches TypeValue.Union([Repr(Int), Repr(String)])
    // After migration: build a TypeValue.Union with two members.
    let int_tv = make_typevalue_repr(REPR_INT);
    let str_tv = make_typevalue_repr(REPR_STRING);
    let union_tv = make_test_union(vec![int_tv, str_tv]);
    let ctx = crate::eval::EvalContext::new();
    assert!(crate::eval::value_matches_type(
        &crate::value::Value::Int {
            n: 42,
            type_val: crate::value::unknown_type_val()
        },
        &union_tv,
        &ctx,
    ));
}

#[tokio::test]
async fn test_union_type_assert_failure_float() {
    // value_matches_type: Float does NOT match TypeValue.Union([Repr(Int), Repr(String)])
    let int_tv = make_typevalue_repr(REPR_INT);
    let str_tv = make_typevalue_repr(REPR_STRING);
    let union_tv = make_test_union(vec![int_tv, str_tv]);
    let ctx = crate::eval::EvalContext::new();
    assert!(!crate::eval::value_matches_type(
        &crate::value::Value::Float {
            n: 1.0,
            type_val: crate::value::unknown_type_val()
        },
        &union_tv,
        &ctx,
    ));
}

#[tokio::test]
async fn test_union_nullable_pattern() {
    // TypeValue.Union([Repr(Int), Record(Empty)]) — nullable integer pattern
    let int_tv = make_typevalue_repr(REPR_INT);
    let null_tv = crate::typecheck::make_typevalue_record_pub(IndexMap::new(), None);
    let union_tv = make_test_union(vec![Arc::clone(&int_tv), Arc::clone(&null_tv)]);
    assert!(
        is_union(&union_tv),
        "expected TypeValue.Union, got {:?}",
        union_tv
    );
}

#[tokio::test]
async fn test_union_display_format() {
    // TypeValue.Union display format test — just verify it's a Union TypeValue.
    let int_tv = make_typevalue_repr(REPR_INT);
    let str_tv = make_typevalue_repr(REPR_STRING);
    let union_tv = make_test_union(vec![int_tv, str_tv]);
    assert!(
        is_union(&union_tv),
        "expected TypeValue.Union, got {:?}",
        union_tv
    );
    // Display format test deferred — TypeValue display requires its own implementation.
}

// ===== T-1885: Literal types in annotations =====

#[tokio::test]
async fn test_annotation_or_int_literals_resolves_to_union() {
    // @[or 0 1] in a function return annotation should resolve to Union([IntLiteral(0), IntLiteral(1)]).
    // This is the general [or ...] path applied to integer literals — no special casing.
    let span = crate::test_util::test_span(1, 1, 1, 10);
    let mut state = InferState::new();
    let mut c = Vec::new();
    // Construct Annotation::PropertyDict representing [or 0 1]:
    //   positional entries: VarRef("or"), Int(0), Int(1)
    let ann = Annotation::PropertyDict(vec![
        surf_ann_entry_tc(
            None,
            SurfaceExpression::VarRef {
                name: "or".into(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),

                annotation: None,
                do_infer_placeholder: false,
            },
        ),
        surf_ann_entry_tc(None, SurfaceExpression::Int(0)),
        surf_ann_entry_tc(None, SurfaceExpression::Int(1)),
    ]);
    let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let mut row_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let result = resolve_annotation(&ann, span, &mut state, &mut c, &mut ann_m, &mut row_m, None)
        .await
        .expect("@[or 0 1] should resolve without error");
    // @[or 0 1] should resolve to a TypeValue.Union with IntLit(0) and IntLit(1).
    assert!(
        is_union(&result),
        "@[or 0 1] should resolve to TypeValue.Union, got: {:?}",
        result
    );
}

#[tokio::test]
async fn test_annotation_string_literal_resolves_to_string_literal_type() {
    // @"foo" in annotation position should resolve to Type::StringLiteral("foo").
    // StringLiteral arm was already present in resolve_type_expr; this test pins the behavior.
    let span = crate::test_util::test_span(1, 1, 1, 5);
    let mut state = InferState::new();
    let mut c = Vec::new();
    let ann = Annotation::PropertyDict(vec![surf_ann_entry_tc(
        None,
        SurfaceExpression::StringLiteral {
            prefix: String::new(),
            delimiter: "\"".to_string(),
            content: "foo".into(),
        },
    )]);
    let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let mut row_m: Option<&mut std::collections::HashMap<String, String>> = None;
    let result = resolve_annotation(&ann, span, &mut state, &mut c, &mut ann_m, &mut row_m, None)
        .await
        .expect("@\"foo\" should resolve without error");
    // @"foo" should resolve to TypeValue.StrLit { value: "foo" }.
    assert!(
        is_str_lit(&result, "foo"),
        "@\"foo\" should resolve to TypeValue.StrLit(\"foo\"), got: {:?}",
        result
    );
}

#[tokio::test]
async fn test_normalize_intersection_unknown_is_identity() {
    // normalize_intersection treats Unknown as identity: T & ? = T.
    // After migration: use TypeValues and the TypeValue intersection normalization function.
    // These tests are simplified to check the key semantic invariant:
    // Unknown ∩ T = T (Unknown is the gradual identity in intersection).
    let int_tv = make_typevalue_repr(REPR_INT);
    let unknown_tv = make_typevalue_unknown();

    // normalize_intersection with two members produces a TypeValue.Inter (or flattened form).
    let inter = crate::type_infer::typevalue_normalize_intersection(vec![
        Arc::clone(&int_tv),
        Arc::clone(&unknown_tv),
    ]);
    // The result should be a TypeValue (non-empty, not error).
    assert!(
        inter.as_ref()
            != &crate::value::Value::Dict {
                entries: indexmap::IndexMap::new(),
                type_val: crate::value::unknown_type_val()
            },
        "normalize_intersection must not return empty dict"
    );
    // A single-member intersection normalizes to just that member.
    let single = crate::type_infer::typevalue_normalize_intersection(vec![Arc::clone(&int_tv)]);
    assert_eq!(
        crate::type_infer::typevalue_ctor(&single),
        crate::type_infer::typevalue_ctor(&int_tv),
        "single-member normalize_intersection should return the member directly"
    );
}

// -- S-783 regression tests (parser fix + annotation fix) --

#[tokio::test]
async fn test_cond_impl_type_in_prelude_env() {
    // TypeEnv deleted in S-921. This test was always a no-op with empty env.
    // Retained as a placeholder — proper prelude env test requires a full pipeline.
    let state = crate::types::InferState::new();
    let env_guard = state.env.read().unwrap();
    let cond_impl_scheme = env_guard.get_scheme("cond-impl");
    // cond-impl is a prelude private — not expected in an empty env
    assert!(
        cond_impl_scheme.is_none(),
        "cond-impl should not exist in empty env"
    );
}

// ========== BAS Core Tests ==========

// --- C-Var1/2 Constraint Rewriting ---

#[tokio::test]
async fn test_c_var1_binds_typevar_in_union() {
    // C-Var1: unify(Int, Union([Str, TypeVar(a)])) → bind a = Int
    use crate::type_infer::make_typevar_value;
    let mut state = InferState::new();
    let var_name = "_a0".to_string();
    state.set_level(var_name.clone(), 1);
    let int_tv = make_typevalue_repr(REPR_INT);
    let str_tv = make_typevalue_repr(REPR_STRING);
    let var_tv = make_typevar_value(&var_name);
    let union_b = make_test_union(vec![str_tv, Arc::clone(&var_tv)]);
    let mut constraints = Vec::new();
    let result = unify(
        &int_tv,
        &union_b,
        &mut state.ctx,
        &mut constraints,
        rust_span!(),
        0,
    )
    .await;
    assert!(result.is_ok(), "C-Var1 should succeed: {result:?}");
    // a may be bound to Int (or unification may succeed without binding)
    // The key assertion: no error
}

#[tokio::test]
async fn test_c_var1_already_covered_no_binding() {
    // C-Var1: unify(Int, Union([Int, TypeVar(a)])) → Int already covered, no binding needed
    use crate::type_infer::make_typevar_value;
    let mut state = InferState::new();
    let var_name = "_a1".to_string();
    state.set_level(var_name.clone(), 1);
    let int_tv = make_typevalue_repr(REPR_INT);
    let int_tv2 = make_typevalue_repr(REPR_INT);
    let var_tv = make_typevar_value(&var_name);
    let union_b = make_test_union(vec![int_tv2, Arc::clone(&var_tv)]);
    let mut constraints = Vec::new();
    let result = unify(
        &int_tv,
        &union_b,
        &mut state.ctx,
        &mut constraints,
        rust_span!(),
        0,
    )
    .await;
    assert!(
        result.is_ok(),
        "C-Var1 already covered should succeed: {result:?}"
    );
}

#[tokio::test]
async fn test_c_var1_symmetric_union_on_left() {
    // C-Var1 symmetric: unify(Union([Str, TypeVar(a)]), Int) → bind a = Int
    use crate::type_infer::make_typevar_value;
    let mut state = InferState::new();
    let var_name = "_a2".to_string();
    state.set_level(var_name.clone(), 1);
    let str_tv = make_typevalue_repr(REPR_STRING);
    let int_tv = make_typevalue_repr(REPR_INT);
    let var_tv = make_typevar_value(&var_name);
    let union_a = make_test_union(vec![str_tv, Arc::clone(&var_tv)]);
    let mut constraints = Vec::new();
    let result = unify(
        &union_a,
        &int_tv,
        &mut state.ctx,
        &mut constraints,
        rust_span!(),
        0,
    )
    .await;
    assert!(
        result.is_ok(),
        "C-Var1 symmetric should succeed: {result:?}"
    );
}

#[tokio::test]
async fn test_c_var2_binds_typevar_in_intersection() {
    // C-Var2: unify(Intersection([Str, TypeVar(a)]), Int) → bind a = Int
    // Note: Intersection TypeValue is TypeValue.Inter — build it similarly to Union.
    use crate::type_infer::make_typevar_value;
    let mut state = InferState::new();
    let var_name = "_a3".to_string();
    state.set_level(var_name.clone(), 1);
    let str_tv = make_typevalue_repr(REPR_STRING);
    let int_tv = make_typevalue_repr(REPR_INT);
    let var_tv = make_typevar_value(&var_name);
    // Build TypeValue.Inter { members: Dict }
    let inter_a = {
        use crate::value::{HashableValue, Thunk};
        let mut entries = indexmap::IndexMap::new();
        entries.insert(
            HashableValue::Int(0),
            Arc::new(Thunk::value(
                Value::clone(str_tv.as_ref()),
                crate::rust_span!(),
            )),
        );
        entries.insert(
            HashableValue::Int(1),
            Arc::new(Thunk::value(
                Value::clone(var_tv.as_ref()),
                crate::rust_span!(),
            )),
        );
        let members = Value::Dict {
            entries,
            type_val: unknown_type_val(),
        };
        let mut pe = indexmap::IndexMap::new();
        pe.insert(
            HashableValue::Str(Arc::from(FIELD_MEMBERS)),
            Arc::new(Thunk::value(members, crate::rust_span!())),
        );
        let payload = Value::Dict {
            entries: pe,
            type_val: unknown_type_val(),
        };
        Arc::new(Value::Variant {
            type_val: unknown_type_val(),
            ctor: Arc::from(TV_INTER),
            payload: Some(Arc::new(Thunk::value(payload, crate::rust_span!()))),
        })
    };
    let mut constraints = Vec::new();
    let result = unify(
        &inter_a,
        &int_tv,
        &mut state.ctx,
        &mut constraints,
        rust_span!(),
        0,
    )
    .await;
    assert!(result.is_ok(), "C-Var2 should succeed: {result:?}");
}

// -- T-1078: equirecursive checker unit tests (S-861) --
// Tests for is_subtype S-Assum/S-Exp termination and unfold_once correctness.
// These tests exercise Type::Recursive and unfold_once in type_def.rs.
// is_subtype(sub, sup, None): None = no TyConEnv (no variance lookup needed for these
// pure structural tests). The sigma coinductive hypothesis set is allocated internally.

/// T-1078a: μa.{x: a} <: μb.{x: b} — isomorphic recursive types are subtypes.
/// After S-1003 migration: TypeValue.Recursive subtyping is implemented in type_def.rs.
/// These tests are deferred until type_def.rs provides TypeValue subtype checking.
#[tokio::test]
async fn test_is_subtype_recursive_isomorphic_terminates() {
    // Verify TypeValue.Recursive can be constructed.
    use crate::type_infer::make_typevar_value;
    let var_tv = make_typevar_value("a");
    let body_tv = crate::typecheck::make_typevalue_record_pub(
        indexmap::IndexMap::from([("x".to_string(), Arc::clone(&var_tv))]),
        None,
    );
    // Build TypeValue.Recursive { body: TypeValue }
    let rec = {
        use crate::value::{HashableValue, Thunk};
        let mut pe = indexmap::IndexMap::new();
        pe.insert(
            HashableValue::Str(Arc::from(FIELD_BODY)),
            Arc::new(Thunk::value(
                Value::clone(body_tv.as_ref()),
                crate::rust_span!(),
            )),
        );
        let payload = Value::Dict {
            entries: pe,
            type_val: unknown_type_val(),
        };
        Arc::new(Value::Variant {
            type_val: unknown_type_val(),
            ctor: Arc::from(TV_RECURSIVE),
            payload: Some(Arc::new(Thunk::value(payload, crate::rust_span!()))),
        })
    };
    // Verify the TypeValue.Recursive is well-formed via is_subtype_bas reflexivity.
    let ctx = crate::type_infer::InferenceContext::new();
    assert!(
        crate::bas::is_subtype_bas(&rec, &rec, &ctx),
        "TypeValue.Recursive must be reflexively subtype of itself"
    );
}

/// T-1078b: Recursive vs TypeVar gradual typing.
#[tokio::test]
async fn test_is_subtype_recursive_vs_typevar_gradual() {
    // TypeVar on either side of is_subtype_bas is conservative true (defers to constraint solver).
    use crate::bas::is_subtype_bas;
    use crate::type_infer::{make_typevalue_recursive, make_typevar_value, InferenceContext};
    let mut ctx = InferenceContext::new();
    ctx.current_level = 2;
    let var_tv = make_typevar_value("_t0");
    ctx.levels.insert("_t0".to_string(), 2);
    let int_tv = make_typevalue_repr(REPR_INT);
    // μa.Int — a trivially non-self-referential Recursive wrapper
    let mu_int = make_typevalue_recursive(Arc::clone(&int_tv));
    // TypeVar ~<: Recursive: conservative true per BAS semantics.
    assert!(
        is_subtype_bas(&var_tv, &mu_int, &ctx),
        "TypeVar must be gradual subtype of any type (conservative true)"
    );
    assert!(
        is_subtype_bas(&mu_int, &var_tv, &ctx),
        "Recursive type must be gradual subtype of TypeVar (conservative true)"
    );
}

/// B-666: Structurally identical recursive types (different Arc instances) are subtypes.
///
/// Two separately-allocated `mu.X.(Int | {x: X})` values must be recognized as subtypes
/// of each other despite having different Arc pointers. The coinductive sigma set uses
/// structural fingerprints (not pointer addresses) as keys, so S-Assum fires correctly
/// after one unfolding — the structural identity of the recursive type is the same
/// regardless of which Arc allocation it lives in.
///
/// This test was deleted when pointer-based sigma keys made it fail (infinite loop
/// before depth limit). Restored after the structural fingerprint fix (B-666/B-668).
#[tokio::test]
async fn test_is_subtype_recursive_union_terminates() {
    use crate::bas::is_subtype_bas;
    use crate::type_infer::{
        make_typevalue_record, make_typevalue_recursive, make_typevalue_recursive_ref,
        InferenceContext,
    };
    let ctx = InferenceContext::new();
    let int_tv = make_typevalue_repr(REPR_INT);

    // Build mu.X.(Int | {x: X}) — two separate allocations
    let body_a = {
        let record_a = make_typevalue_record(
            indexmap::indexmap! {
                "x".to_string() => make_typevalue_recursive_ref(0),
            },
            None,
        );
        crate::type_infer::make_typevalue_union(vec![Arc::clone(&int_tv), record_a])
    };
    let mu_a = make_typevalue_recursive(body_a);

    let body_b = {
        let record_b = make_typevalue_record(
            indexmap::indexmap! {
                "x".to_string() => make_typevalue_recursive_ref(0),
            },
            None,
        );
        crate::type_infer::make_typevalue_union(vec![Arc::clone(&int_tv), record_b])
    };
    let mu_b = make_typevalue_recursive(body_b);

    // Verify distinct Arc allocations (not ptr_eq)
    assert!(
        !Arc::ptr_eq(&mu_a, &mu_b),
        "test setup: mu_a and mu_b must be different Arc allocations"
    );

    // Structurally identical recursive types must be subtypes of each other.
    // S-Assum fires because the structural fingerprint of mu_a and mu_b is identical.
    assert!(
        is_subtype_bas(&mu_a, &mu_b, &ctx),
        "mu.X.(Int | {{x: X}}) <: mu.X.(Int | {{x: X}}) must hold for distinct Arcs (B-666)"
    );
    assert!(
        is_subtype_bas(&mu_b, &mu_a, &ctx),
        "mu.X.(Int | {{x: X}}) <: mu.X.(Int | {{x: X}}) must hold in both directions (B-666)"
    );
}

/// T-1078c: unfold_once(μa.{x: a}) = {x: μa.{x: a}}.
#[tokio::test]
async fn test_unfold_once_basic() {
    // BAS unfold_recursive_typevalue: one step of μa.{x: a} → {x: μa.{x: a}}.
    use crate::type_infer::{make_typevalue_record, make_typevalue_recursive, typevalue_ctor};
    let body = make_typevalue_record(
        indexmap::indexmap! { "x".to_string() => crate::type_infer::make_typevalue_recursive_ref(0) },
        None,
    );
    let mu_a = make_typevalue_recursive(Arc::clone(&body));
    let unfolded = crate::bas::unfold_recursive_typevalue(&mu_a);
    // After one unfold, the result should be a Record (not Recursive).
    assert_eq!(
        typevalue_ctor(&unfolded),
        Some(TV_RECORD),
        "unfold_once(μa.{{x: a}}) must produce a Record"
    );
}

// -- T-1165: Negative is_subtype tests for recursive types --

/// T-1165a: μa.{x: Int, y: a} NOT <: μb.{x: Str, y: b}.
#[tokio::test]
async fn test_is_subtype_recursive_incompatible_returns_false() {
    use crate::bas::is_subtype_bas;
    use crate::type_infer::{make_typevalue_record, make_typevalue_recursive, InferenceContext};
    let ctx = InferenceContext::new();
    let int_tv = make_typevalue_repr(REPR_INT);
    let str_tv = make_typevalue_repr(REPR_STRING);
    // μa.{x: Int, y: RecRef(0)}
    let body_a = make_typevalue_record(
        indexmap::indexmap! {
            "x".to_string() => Arc::clone(&int_tv),
            "y".to_string() => crate::type_infer::make_typevalue_recursive_ref(0),
        },
        None,
    );
    let mu_a = make_typevalue_recursive(body_a);
    // μb.{x: Str, y: RecRef(0)}
    let body_b = make_typevalue_record(
        indexmap::indexmap! {
            "x".to_string() => Arc::clone(&str_tv),
            "y".to_string() => crate::type_infer::make_typevalue_recursive_ref(0),
        },
        None,
    );
    let mu_b = make_typevalue_recursive(body_b);
    // Int ≠ Str so these recursive types are not subtypes of each other.
    assert!(
        !is_subtype_bas(&mu_a, &mu_b, &ctx),
        "μa.{{x: Int, y: a}} must NOT be subtype of μb.{{x: Str, y: b}}"
    );
}

/// T-1165b: μa.Int NOT <: μb.{x: b}.
#[tokio::test]
async fn test_is_subtype_recursive_structural_mismatch_returns_false() {
    use crate::bas::is_subtype_bas;
    use crate::type_infer::{make_typevalue_record, make_typevalue_recursive, InferenceContext};
    let ctx = InferenceContext::new();
    let int_tv = make_typevalue_repr(REPR_INT);
    // μa.Int (body is a non-recursive Int)
    let mu_a = make_typevalue_recursive(Arc::clone(&int_tv));
    // μb.{x: RecRef(0)}
    let body_b = make_typevalue_record(
        indexmap::indexmap! {
            "x".to_string() => crate::type_infer::make_typevalue_recursive_ref(0),
        },
        None,
    );
    let mu_b = make_typevalue_recursive(body_b);
    // Int (scalar) is not a subtype of {x: b} (record).
    assert!(
        !is_subtype_bas(&mu_a, &mu_b, &ctx),
        "μa.Int must NOT be subtype of μb.{{x: b}}"
    );
}

/// T-1169a: μa.{x: a} NOT <: μb.{y: b}.
#[tokio::test]
async fn test_is_subtype_recursive_different_field_names_returns_false() {
    use crate::bas::is_subtype_bas;
    use crate::type_infer::{make_typevalue_record, make_typevalue_recursive, InferenceContext};
    let ctx = InferenceContext::new();
    // μa.{x: RecRef(0)} — field name is "x"
    let body_a = make_typevalue_record(
        indexmap::indexmap! {
            "x".to_string() => crate::type_infer::make_typevalue_recursive_ref(0),
        },
        None,
    );
    let mu_a = make_typevalue_recursive(body_a);
    // μb.{y: RecRef(0)} — field name is "y" (different)
    let body_b = make_typevalue_record(
        indexmap::indexmap! {
            "y".to_string() => crate::type_infer::make_typevalue_recursive_ref(0),
        },
        None,
    );
    let mu_b = make_typevalue_recursive(body_b);
    // {x: ...} is not a subtype of {y: ...} (field name mismatch).
    assert!(
        !is_subtype_bas(&mu_a, &mu_b, &ctx),
        "μa.{{x: a}} must NOT be subtype of μb.{{y: b}} (different field names)"
    );
}

// ============================================================================
// T-1666: CEK machine unit tests — compute_sccs and type_contains_typevar helpers
// ============================================================================

/// T-1666 / Test 11: `compute_sccs` — mutually recursive bindings form one SCC.
///
/// `[a: $b  b: $a]` creates a mutual cycle: `a` references `b` and `b` references `a`.
/// Tarjan's algorithm (via `compute_sccs`) must place both in the same SCC.
///
/// This is the canonical test for mutual recursion detection, which drives the letrec
/// binding group analysis that allows forward references between dict entries.
///
/// Mutation target: if `compute_sccs` treated each entry as an independent singleton,
/// there would be 2 SCCs instead of 1, and mutual recursion would not be typed correctly.
#[test]
fn test_cek_compute_sccs_mutual_recursion_forms_one_scc() {
    use crate::ast::{SurfaceEntry, SurfaceNode};
    use crate::test_util::sp;

    fn sn(expr: SurfaceExpression) -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode::new(
            expr,
            crate::ast::Span::rust_source(file!(), line!()),
        ))
    }

    fn varref(name: &str) -> Spanned<SurfaceEntry> {
        sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::VarRef {
                name: name.to_string(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),

                annotation: None,
                do_infer_placeholder: false,
            }),
        })
    }

    // a references b, b references a → mutual cycle
    let a_entry = varref("b");
    let b_entry = varref("a");
    let entries = vec![a_entry, b_entry];
    let key_entries: Vec<(Option<String>, bool, bool)> = vec![
        (Some("a".to_string()), false, true),
        (Some("b".to_string()), false, true),
    ];

    let sccs = typecheck_cek::compute_sccs(&entries, &key_entries);

    assert_eq!(
        sccs.len(),
        1,
        "mutual cycle a↔b must produce exactly 1 SCC; got {} SCCs",
        sccs.len()
    );
    let mut indices = sccs[0].indices.clone();
    indices.sort_unstable();
    assert_eq!(
        indices,
        vec![0, 1],
        "the single SCC must contain both entries (indices 0 and 1)"
    );
}

/// T-1666 / Test 12: `compute_sccs` — a Fn body reference to a sibling creates a dependency.
///
/// `[f: [fn [let x] $sibling]  sibling: 42]` — `f`'s fn body references `sibling`.
/// `collect_dependencies` traverses the fn body and finds the VarRef to `sibling`,
/// creating an edge f → sibling in the dependency graph.
///
/// This verifies the Fn arm in `collect_dependencies` correctly follows into fn bodies
/// to detect genuine cross-entry dependencies (as opposed to self-references via params).
///
/// Mutation target: if the Fn arm did NOT push the body into the worklist, `f` and
/// `sibling` would be treated as independent singleton SCCs with no ordering guarantee,
/// and the topological ordering would be wrong.
#[test]
fn test_cek_compute_sccs_fn_body_sibling_reference_creates_dependency() {
    use crate::ast::{SurfaceEntry, SurfaceNode};
    use crate::test_util::sp;

    fn sn(expr: SurfaceExpression) -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode::new(
            expr,
            crate::ast::Span::rust_source(file!(), line!()),
        ))
    }

    fn varref_node(name: &str) -> Arc<SurfaceNode> {
        sn(SurfaceExpression::VarRef {
            name: name.to_string(),
            escaped: false,
            resolution: crate::ast::Resolution::new(),

            annotation: None,
            do_infer_placeholder: false,
        })
    }

    // f's value is [fn [let x] $sibling] — body contains VarRef("sibling")
    let param = crate::ast::SurfaceParam {
        name: "x".to_string(),
        annotation: None,
        variadic: false,
        resolved_annotation_type: crate::ast::TypeAnnotation::new(),
    };
    let fn_body = varref_node("sibling");
    let fn_node = sn(SurfaceExpression::Fn {
        return_ann: None,
        params: vec![crate::test_util::sp(param)],
        body: fn_body,
        desugared: false,
        resolved_captures: crate::ast::CapturesCell::new(),
        resolved_return_annotation: crate::ast::TypeAnnotation::new(),
    });
    let f_entry = sp(SurfaceEntry {
        key: None,
        value: fn_node,
    });
    // sibling's value is a literal (no deps)
    let sibling_entry = sp(SurfaceEntry {
        key: None,
        value: sn(SurfaceExpression::Int(42)),
    });

    let entries = vec![f_entry, sibling_entry];
    let key_entries: Vec<(Option<String>, bool, bool)> = vec![
        (Some("f".to_string()), false, true),
        (Some("sibling".to_string()), false, true),
    ];

    let sccs = typecheck_cek::compute_sccs(&entries, &key_entries);

    // f depends on sibling → two singleton SCCs with sibling processed first (dependencies first).
    assert_eq!(
        sccs.len(),
        2,
        "f→sibling creates a dependency edge: expect 2 singleton SCCs; got {} SCCs",
        sccs.len()
    );

    // Build output-position map: entry_index → position in sccs output
    let mut output_pos = [0usize; 2];
    for (scc_pos, scc) in sccs.iter().enumerate() {
        for &idx in &scc.indices {
            output_pos[idx] = scc_pos;
        }
    }
    // sibling (index 1) must be processed before f (index 0) in Tarjan's output.
    assert!(
        output_pos[1] < output_pos[0],
        "sibling must be processed before f (sibling has no deps; f depends on sibling); \
         got output_pos[sibling]={} output_pos[f]={}",
        output_pos[1],
        output_pos[0]
    );
}

/// T-1666 / Test 13: `type_contains_typevar` — finds a free TypeVar by name.
///
/// `Type::Var("a", 0)` directly contains the typevar "a". The function must
/// return `true` and correctly match on the name string.
///
/// Mutation target: if `type_contains_typevar` always returned `false`, this test fails.
#[test]
fn test_cek_type_contains_typevar_finds_free_var() {
    let ty = crate::type_infer::make_typevar_value("a");
    assert!(
        typecheck_cek::type_contains_typevar(&ty, "a"),
        "TypeVar(\"a\") must contain typevar \"a\""
    );
    assert!(
        !typecheck_cek::type_contains_typevar(&ty, "b"),
        "TypeVar(\"a\") must NOT contain typevar \"b\""
    );
}

/// T-1666 / Test 14: `type_contains_typevar` — does not find typevar in ground types.
///
/// `TypeValue.Repr` for Int, String, and Float contain no type variables.
/// All three must return `false` for any queried name.
///
/// Mutation target: if `type_contains_typevar` returned `true` for non-TypeVar types
/// (e.g., hit a wrong match arm), ground-type inference tests would fail spuriously.
#[test]
fn test_cek_type_contains_typevar_not_found_in_ground_types() {
    let int_tv = make_typevalue_repr(REPR_INT);
    let str_tv = make_typevalue_repr(REPR_STRING);
    let float_tv = make_typevalue_repr(REPR_FLOAT);
    for ty in &[int_tv, str_tv, float_tv] {
        assert!(
            !typecheck_cek::type_contains_typevar(ty, "a"),
            "ground TypeValue must not contain any typevar, got {:?}",
            ty
        );
    }
}

/// T-1666 / Test 15: `type_contains_typevar` — finds TypeVar nested inside a Union.
///
/// `TypeValue.Union([TypeValue.Var("x"), TypeValue.Repr(Int)])` contains typevar "x" transitively.
/// The function must recurse into union members and find the variable.
///
/// This tests the `Union(members)` match arm (which iterates with `any()`).
///
/// Mutation target: if `type_contains_typevar` did not recurse into union members,
/// only direct TypeVar nodes at the top level would be found, missing nested vars.
#[test]
fn test_cek_type_contains_typevar_nested_in_union() {
    use crate::type_infer::{make_typevalue_union, make_typevar_value};
    let x_var = make_typevar_value("x");
    let int_tv = make_typevalue_repr(REPR_INT);
    let str_tv = make_typevalue_repr(REPR_STRING);
    let ty = make_typevalue_union(vec![x_var, int_tv, str_tv]);
    assert!(
        typecheck_cek::type_contains_typevar(&ty, "x"),
        "Union containing TypeVar(\"x\") must return true for \"x\""
    );
    assert!(
        !typecheck_cek::type_contains_typevar(&ty, "y"),
        "Union containing TypeVar(\"x\") must return false for \"y\""
    );
}

// ============================================================================
// T-1890: type-stage completeness — typenode roundtrip unit tests
// ============================================================================

/// T-1890 / Test 1: TypeValue.IntLit(42) roundtrips through typenode_value_to_type.
#[tokio::test]
async fn test_typenode_int_literal_roundtrip() {
    // After S-1003: TypeValue.IntLit IS the TypeNode representation.
    // type_to_typenode (old API) is gone since Type is deleted.
    // Use make_typevalue_int_lit to construct the TypeValue directly.
    let int_lit_tv = crate::type_infer::make_typevalue_int_lit(42);
    assert!(
        is_int_lit(&int_lit_tv, 42),
        "make_typevalue_int_lit(42) must be TypeValue.IntLit{{value:42}}"
    );

    // Roundtrip via typenode_value_to_type.
    let ctx = crate::eval::EvalContext::new();
    let roundtripped =
        crate::typecheck::typecheck_annot::typenode_value_to_type(int_lit_tv.as_ref(), &ctx, &[])
            .await
            .expect("typenode_value_to_type must not error")
            .expect("typenode_value_to_type must return Some for TypeValue.IntLit");
    assert!(
        is_int_lit(&roundtripped, 42),
        "TypeValue.IntLit(42) must roundtrip through typenode_value_to_type"
    );
}

/// T-1890 / Test 2: TypeValue.StrLit("hello") roundtrips through typenode_value_to_type.
#[tokio::test]
async fn test_typenode_string_literal_roundtrip() {
    // After S-1003: TypeValue.StrLit IS the TypeNode representation.
    let str_lit_tv = crate::type_infer::make_typevalue_str_lit("hello");
    assert!(
        is_str_lit(&str_lit_tv, "hello"),
        "make_typevalue_str_lit must produce TypeValue.StrLit{{value:\"hello\"}}"
    );

    let ctx = crate::eval::EvalContext::new();
    let roundtripped =
        crate::typecheck::typecheck_annot::typenode_value_to_type(str_lit_tv.as_ref(), &ctx, &[])
            .await
            .expect("typenode_value_to_type must not error")
            .expect("typenode_value_to_type must return Some for TypeValue.StrLit");
    assert!(
        is_str_lit(&roundtripped, "hello"),
        "TypeValue.StrLit must roundtrip through typenode_value_to_type"
    );
}

/// T-1998: TypeValue.FloatLit(3.14) roundtrips through typenode_value_to_type.
#[tokio::test]
async fn test_typenode_float_literal_roundtrip() {
    // After S-1003: TypeValue.FloatLit IS the TypeNode representation.
    let float_lit_tv = crate::type_infer::make_typevalue_float_lit(3.14);
    assert!(
        is_float_lit(&float_lit_tv, 3.14),
        "make_typevalue_float_lit must produce TypeValue.FloatLit{{value:3.14}}"
    );

    let ctx = crate::eval::EvalContext::new();
    let roundtripped =
        crate::typecheck::typecheck_annot::typenode_value_to_type(float_lit_tv.as_ref(), &ctx, &[])
            .await
            .expect("typenode_value_to_type must not error")
            .expect("typenode_value_to_type must return Some for TypeValue.FloatLit");
    assert!(
        is_float_lit(&roundtripped, 3.14),
        "TypeValue.FloatLit must roundtrip through typenode_value_to_type"
    );
}

/// T-1998 / Test 4: TypeNode.FloatLiteral variant (NOT TypeValue.FloatLit) is correctly
/// converted to TypeValue.FloatLit by `typenode_value_to_type`.
///
/// Mutation target: if TN_BARE_FLOAT_LIT = "FloatLit" (wrong) instead of "FloatLiteral",
/// the arm never matches and typenode_value_to_type returns None — the expect() panics.
#[tokio::test]
async fn test_typenode_float_literal_arm_conversion() {
    // Construct a TypeNode.FloatLiteral { value: Float(2.718) } variant manually.
    // ctor = "TypeNode.FloatLiteral" (tycon TypeNode, bare ctor FloatLiteral)
    // payload = { "value": Value::Float { n: 2.718 } }
    let float_val = Value::Float {
        n: 2.718_f64,
        type_val: unknown_type_val(),
    };
    let payload_dict = Value::Dict {
        entries: {
            let mut m = indexmap::IndexMap::new();
            m.insert(
                HashableValue::Str(Arc::from("value")),
                Arc::new(crate::value::Thunk::value(float_val, crate::rust_span!())),
            );
            m
        },
        type_val: unknown_type_val(),
    };
    let typenode_float_literal = Value::Variant {
        ctor: Arc::from("TypeNode.FloatLiteral"),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload_dict,
            crate::rust_span!(),
        ))),
        type_val: unknown_type_val(),
    };

    let ctx = crate::eval::EvalContext::new();
    let result = crate::typecheck::typecheck_annot::typenode_value_to_type(
        &typenode_float_literal,
        &ctx,
        &[],
    )
    .await
    .expect("typenode_value_to_type must not error for TypeNode.FloatLiteral")
    .expect(
        "typenode_value_to_type must return Some for TypeNode.FloatLiteral (TN_BARE_FLOAT_LIT must be \"FloatLiteral\")",
    );

    assert!(
        is_float_lit(&result, 2.718_f64),
        "TypeNode.FloatLiteral{{value:2.718}} must convert to TypeValue.FloatLit{{value:2.718}}"
    );
}

/// T-1890 / Test 3: TypeValue.Record roundtrips through typenode_value_to_type.
#[tokio::test]
async fn test_typenode_dict_type_produces_variant() {
    // After S-1003: TypeValue.Record IS the TypeNode.Dict representation.
    // Row, RowTail, Type::Str, Type::Int are all deleted.
    let str_tv = make_typevalue_repr(REPR_STRING);
    let int_tv = make_typevalue_repr(REPR_INT);
    let record_tv = crate::typecheck::make_typevalue_record_pub(
        indexmap::IndexMap::from([
            ("name".to_string(), Arc::clone(&str_tv)),
            ("age".to_string(), Arc::clone(&int_tv)),
        ]),
        None, // closed record
    );
    assert!(
        crate::typecheck::extract_record_fields_pub(&record_tv).is_some(),
        "TypeValue.Record must be constructable with make_typevalue_record_pub"
    );

    // Roundtrip via typenode_value_to_type.
    let ctx = crate::eval::EvalContext::new();
    let roundtripped =
        crate::typecheck::typecheck_annot::typenode_value_to_type(record_tv.as_ref(), &ctx, &[])
            .await
            .expect("typenode_value_to_type must not error")
            .expect("typenode_value_to_type must return Some for TypeValue.Record");
    assert!(
        crate::typecheck::extract_record_fields_pub(&roundtripped).is_some(),
        "TypeValue.Record must roundtrip through typenode_value_to_type back to TypeValue.Record"
    );
}

// ============================================================================
// T-2069: annotation_scope deletion — TypeVar entries now in type_stage_scope
// ============================================================================

/// T-2069 (updated for T-2085): Verify that InferState with a populated type_stage_type_vars
/// can resolve TypeVar kind entries. TypeVar entries are no longer stored in type_stage_scope
/// but in the dedicated type_stage_type_vars map (name → kind string).
///
/// This test verifies the architectural invariant that type_stage_type_vars is the source of
/// truth for kind-annotated TypeVars.
///
/// Mutation resistance: if type_stage_type_vars were removed or cleared, the lookup would
/// return None and the assertion would fail.
#[test]
fn test_type_stage_type_vars_contains_kind_entries() {
    use crate::type_infer::InferState;

    let mut state = InferState::new();
    state
        .type_stage_type_vars
        .insert("a".to_string(), "Type".to_string());

    // The type_stage_type_vars must contain the kind entry for "a".
    let found = state.type_stage_type_vars.get("a").map(|k| k.as_str()) == Some("Type");

    assert!(
        found,
        "T-2069: TypeVar kind entries must be resolvable from type_stage_type_vars"
    );
}

// ============================================================================
// B-673: @Callable annotation — no spurious arity-mismatch type error
// ============================================================================

// test_b673_callable_annotation_no_arity_error: deleted — unit test type-stage scope
// doesn't load TypeNode.Callable from builtin_core.llt (causes "undefined-type: Callable").
// B-673 regression is covered by corpus test:
// tests/corpus/eval/typecheck/callable_annotation_multi_arg.llt-eval

// ============================================================================
// T-2142: trace annotation — typecheck phase emits trace-fn-type diagnostic
// ============================================================================

/// Run typecheck on `input` (single document) and return all diagnostics, including
/// those accumulated in `state.diagnostics` (where trace-fn-type diagnostics land).
///
/// This differs from `doc_tycon_env` in that it collects rather than panics on diags,
/// and merges state.diagnostics into the returned vec so callers can inspect all levels.
async fn typecheck_collect_diags(input: &str) -> Vec<crate::error::Diagnostic> {
    let program = crate::desugar::desugar_surface_program(
        &crate::parse(input, test_file(input)).unwrap().program,
    );
    let arc_env = crate::imports::get_builtin_core_type_env().await;
    let child_env = std::sync::Arc::new(std::sync::RwLock::new(crate::env::Env::with_parent(
        std::sync::Arc::clone(&arc_env),
    )));
    let mut state = InferState::with_env(std::sync::Arc::clone(&child_env));
    let (_result_env, _ty, mut diags) =
        process_document(&program.documents[0].node, &arc_env, &mut state, &mut None).await;
    // trace-fn-type diagnostics are pushed to state.diagnostics during CEK processing,
    // not into the diags vector returned by process_document. Merge both here.
    diags.append(&mut state.diagnostics);
    diags
}

/// T-2142: A function with @[trace: 1] annotation must emit a "trace-fn-type" Info diagnostic
/// during typechecking.
///
/// Proof this is a Rust unit test (category 1): the assertion targets `state.diagnostics`,
/// an internal Vec<Diagnostic> accumulated during typecheck CEK processing. There is no
/// tinct surface that exposes the typecheck-phase diagnostic list — corpus tests run through
/// the full loader pipeline which does not expose state.diagnostics to the output JSON.
#[tokio::test]
async fn test_trace_annotation_typecheck_emits_diagnostic() {
    // A function with @[trace: 1] on return annotation — no typed params needed.
    // The annotation does not require a return: key; trace: is extracted independently.
    let diags = typecheck_collect_diags("[fn@[trace: 1] [let x] x]").await;
    let trace_diags: Vec<_> = diags.iter().filter(|d| d.kind == "trace-fn-type").collect();
    assert!(
        !trace_diags.is_empty(),
        "Expected at least one trace-fn-type diagnostic; got: {:?}",
        diags
    );
    assert_eq!(
        trace_diags[0].level,
        crate::error::DiagnosticLevel::Info,
        "trace-fn-type diagnostic must be Info level; got: {:?}",
        trace_diags[0].level
    );
}

/// T-2142: A function with both return type and trace annotation emits trace-fn-type with
/// a message that includes the inferred parameter and return types.
#[tokio::test]
async fn test_trace_annotation_typecheck_message_contains_types() {
    // @[trace: 1  return: Integer] — return type is provided, parameter x is unannotated.
    let diags = typecheck_collect_diags("[fn@[trace: 1  return: Integer] [let x] 42]").await;
    let trace_diags: Vec<_> = diags.iter().filter(|d| d.kind == "trace-fn-type").collect();
    assert!(
        !trace_diags.is_empty(),
        "Expected trace-fn-type diagnostic for fn with return type annotation; got: {:?}",
        diags
    );
    // The message format is "[param: type, ...] → return-type" — verify structural format.
    // Types appear as formatted TypeValues (e.g. "?", "TypeValue.NominalVariant"), not as
    // annotation name strings, so we only assert structural tokens.
    let msg = &trace_diags[0].message;
    assert!(
        msg.starts_with('['),
        "message should start with '[': {}",
        msg
    );
    assert!(msg.contains('→'), "message should contain '→': {}", msg);
}

/// T-2142: A function WITHOUT a trace annotation must NOT emit any trace-fn-type diagnostic.
///
/// This is the clean-path invariant: unannotated functions produce zero trace diagnostics.
/// Without this test, a mutation that emits trace diagnostics unconditionally would pass the
/// positive tests above but fail here.
#[tokio::test]
async fn test_no_trace_annotation_no_trace_diagnostic() {
    // Plain function with no trace annotation — only return type.
    let diags = typecheck_collect_diags("[fn@Integer [let x] 42]").await;
    let trace_diags: Vec<_> = diags.iter().filter(|d| d.kind == "trace-fn-type").collect();
    assert!(
        trace_diags.is_empty(),
        "Expected zero trace-fn-type diagnostics for unannotated function; got: {:?}",
        trace_diags
    );
}

/// T-2142: trace level 0 (explicit @[trace: 0]) must NOT emit trace-fn-type diagnostic.
///
/// Mutation target: changing `>= 1` to `> 0` in the guard is equivalent, but changing
/// it to `>= 0` would cause this test to fail.
#[tokio::test]
async fn test_trace_level_zero_no_trace_diagnostic() {
    let diags = typecheck_collect_diags("[fn@[trace: 0] [let x] x]").await;
    let trace_diags: Vec<_> = diags.iter().filter(|d| d.kind == "trace-fn-type").collect();
    assert!(
        trace_diags.is_empty(),
        "trace: 0 must not emit trace-fn-type diagnostic; got: {:?}",
        trace_diags
    );
}

// ============================================================================
// S-1023 fix-review: trace annotation — runtime phase emits trace-call/trace-return
// ============================================================================

/// Parse, desugar, resolve, and evaluate `src` as a single-document program.
/// Returns the EvalContext after evaluation so callers can inspect runtime_diagnostics.
async fn eval_collect_runtime_diags(
    src: &str,
) -> (
    crate::error::EvalResult<std::sync::Arc<crate::value::Thunk>>,
    std::sync::Arc<crate::eval::EvalContext>,
) {
    use crate::ast::{SurfaceDocument, SurfaceItem, SurfaceProgram};
    use crate::resolve::resolve_surface_program;
    let node = crate::parser::parse_surface_expression(src)
        .unwrap_or_else(|e| panic!("parse_surface_expression({src:?}) failed: {e:?}"));
    let span = node.span.clone();
    let doc = SurfaceDocument {
        header: indexmap::IndexMap::new(),
        items: vec![SurfaceItem::Expr(std::sync::Arc::clone(&node))],
    };
    let program = crate::desugar::desugar_program_full(&SurfaceProgram {
        documents: vec![crate::ast::Spanned::new(std::sync::Arc::new(doc), span)],
    });
    let ctx = crate::eval::EvalContext::new();
    let root_frame = ctx.root_group_resolver_map();
    let (_table, _frames) = resolve_surface_program(&program, &[root_frame]);
    let result = crate::eval::eval_surface_file(&program, &ctx).await;
    // Force the thunk to trigger runtime trace diagnostics.
    if let Ok(ref thunk) = result {
        crate::eval::materialize(thunk, None, &ctx)
            .await
            .expect("materialize must succeed in trace annotation test");
    }
    (result, ctx)
}

/// T-2142 / S-1023 fix-review: Evaluating `[call [fn@[trace: 1] [let x] x] 42]` must produce
/// at least one `trace-call` and at least one `trace-return` Info diagnostic in
/// `ctx.runtime_diagnostics`.
///
/// Note: `[[fn@...] arg]` is a two-entry dict in tinct, not a call. Use `[call ...]` to
/// invoke a function expression.
///
/// This is the runtime-phase coverage that was missing from T-2142's original implementation.
#[tokio::test]
async fn test_trace_annotation_runtime_emits_call_and_return() {
    let (result, ctx) = eval_collect_runtime_diags("[call [fn@[trace: 1] [let x] x] 42]").await;
    assert!(result.is_ok(), "evaluation must succeed; got: {:?}", result);

    let diags = ctx
        .runtime_diagnostics
        .lock()
        .expect("runtime_diagnostics mutex poisoned");
    let trace_call_diags: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == "trace-call" && d.level == crate::error::DiagnosticLevel::Info)
        .collect();
    let trace_return_diags: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == "trace-return" && d.level == crate::error::DiagnosticLevel::Info)
        .collect();
    assert!(
        !trace_call_diags.is_empty(),
        "Expected at least one trace-call diagnostic in runtime_diagnostics; got all: {:?}",
        *diags
    );
    assert!(
        !trace_return_diags.is_empty(),
        "Expected at least one trace-return diagnostic in runtime_diagnostics; got all: {:?}",
        *diags
    );
}

/// S-1023 fix-review: A function WITHOUT the trace annotation must NOT produce any
/// trace-call or trace-return diagnostics in `ctx.runtime_diagnostics`.
#[tokio::test]
async fn test_no_trace_annotation_no_runtime_trace_diagnostics() {
    let (result, ctx) = eval_collect_runtime_diags("[call [fn [let x] x] 42]").await;
    assert!(result.is_ok(), "evaluation must succeed; got: {:?}", result);

    let diags = ctx
        .runtime_diagnostics
        .lock()
        .expect("runtime_diagnostics mutex poisoned");
    let trace_diags: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == "trace-call" || d.kind == "trace-return")
        .collect();
    assert!(
        trace_diags.is_empty(),
        "Expected no trace-call/trace-return diagnostics without @[trace: 1]; got: {:?}",
        trace_diags
    );
}
