//! Unit tests for type_precision_fixes sprint tasks

use super::{
    constrain, promote_literal_for_constrained_var, resolve_has_field, unify,
    MAX_RESOLVE_HAS_FIELD_DEPTH,
};

/// Async wrapper for `unify` — for use in tests only.
async fn unify_sync<'a>(
    a: &'a crate::types::Type,
    b: &'a crate::types::Type,
    state: &'a mut crate::types::InferState,
    constraints: &'a mut Vec<crate::types::Constraint>,
    span: crate::ast::Span,
) -> Result<(), crate::error::TypeDiagnostic> {
    unify(a, b, state, constraints, span).await
}
use crate::rust_span;
use crate::type_class::ConstraintArg;
use crate::type_def::{TyConDef, Variance};
use crate::types::{Constraint, InferState, Kind, Label, Row, Type, TypeEnv};
use indexmap::IndexMap;
use std::collections::HashMap;

/// Task 1a: resolve_has_field on Type::Any should return Top (not Unknown)
#[tokio::test]
async fn test_resolve_has_field_top_returns_top() {
    let mut state = InferState::new();
    let label = Label::Concrete("x".to_string());
    let span = rust_span!();

    let result = resolve_has_field(&label, &Type::Any, &mut state, span, 0);

    assert!(result.is_ok());
    match result.unwrap() {
        Type::Any => {} // Expected
        other => panic!("Expected Top, got {:?}", other),
    }
}

/// Task 1b: resolve_has_field with depth overflow should error (not return Unknown)
#[tokio::test]
async fn test_resolve_has_field_depth_overflow_errors() {
    let mut state = InferState::new();
    let label = Label::Concrete("x".to_string());
    let span = rust_span!();

    // Create a simple record to test depth overflow
    let mut fields = IndexMap::new();
    fields.insert("x".to_string(), Type::Int);
    let record_ty = Type::Dict(Row {
        fields,
        tail: crate::type_def::RowTail::Empty,
    });

    // Call with depth exceeding MAX_RESOLVE_HAS_FIELD_DEPTH
    let result = resolve_has_field(
        &label,
        &record_ty,
        &mut state,
        span,
        MAX_RESOLVE_HAS_FIELD_DEPTH + 1,
    );

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .message
        .contains("HasField recursion depth exceeded"));
}

/// Task 3a: Single-field records with different keys are disjoint
#[tokio::test]
async fn test_types_are_disjoint_single_field_records() {
    let mut fields1 = IndexMap::new();
    fields1.insert("x".to_string(), Type::Int);
    let rec1 = Type::Dict(Row {
        fields: fields1,
        tail: crate::type_def::RowTail::Empty,
    });

    let mut fields2 = IndexMap::new();
    fields2.insert("y".to_string(), Type::Str);
    let rec2 = Type::Dict(Row {
        fields: fields2,
        tail: crate::type_def::RowTail::Empty,
    });

    assert!(
        Type::types_are_disjoint(&rec1, &rec2),
        "{{x: Int}} and {{y: Str}} should be disjoint"
    );
}

/// Task 3b: Single-field records with same key are NOT disjoint
#[tokio::test]
async fn test_types_are_not_disjoint_same_key_records() {
    let mut fields1 = IndexMap::new();
    fields1.insert("x".to_string(), Type::Int);
    let rec1 = Type::Dict(Row {
        fields: fields1,
        tail: crate::type_def::RowTail::Empty,
    });

    let mut fields2 = IndexMap::new();
    fields2.insert("x".to_string(), Type::Str);
    let rec2 = Type::Dict(Row {
        fields: fields2,
        tail: crate::type_def::RowTail::Empty,
    });

    assert!(
        !Type::types_are_disjoint(&rec1, &rec2),
        "{{x: Int}} and {{x: Str}} should NOT be disjoint (conservative - field overlap)"
    );
}

/// Task 3c: Multi-field records are conservatively NOT disjoint
#[tokio::test]
async fn test_types_are_not_disjoint_multi_field_records() {
    let mut fields1 = IndexMap::new();
    fields1.insert("x".to_string(), Type::Int);
    fields1.insert("a".to_string(), Type::TyCon("Boolean".to_string()));
    let rec1 = Type::Dict(Row {
        fields: fields1,
        tail: crate::type_def::RowTail::Empty,
    });

    let mut fields2 = IndexMap::new();
    fields2.insert("y".to_string(), Type::Str);
    fields2.insert("b".to_string(), Type::Float);
    let rec2 = Type::Dict(Row {
        fields: fields2,
        tail: crate::type_def::RowTail::Empty,
    });

    assert!(
        !Type::types_are_disjoint(&rec1, &rec2),
        "Multi-field records {{x:Int,a:Bool}} and {{y:Str,b:Float}} should NOT be disjoint (conservative)"
    );
}

// test_promote_literal_restricted_to_promotable_classes — deleted: Numeric class no longer in InferState::new() after type-foundations sprint.

/// Literal promotion applies uniformly for ANY class constraint.
/// IntLiteral(42) with a "MyClass" constraint is promoted to Int.
#[tokio::test]
async fn test_promote_literal_promoted_for_any_class() {
    let state = InferState::new();

    // Any class constraint triggers literal promotion — no class-name whitelist.
    use crate::types::{ClassDecl, Kind};
    let my_class = std::sync::Arc::new(ClassDecl {
        name: "MyClass".to_string(),
        params: vec![("a".to_string(), Kind::Type)],
        superclasses: vec![],
        determines: vec![],
        resolver: None,
        resolver_injective: false,
        structural_discharge: crate::type_class::StructuralDischarge::None,
        method_signatures: vec![],
    });
    let constraints: Vec<Constraint> = vec![Constraint::Class {
        class: my_class,
        vars: vec![ConstraintArg::Var("t0".to_string())],
        origin_name: None,
        origin_span: None,
    }];

    let result =
        promote_literal_for_constrained_var("t0", Type::IntLiteral(42), &constraints, &state);

    match result {
        Type::Int => {} // Expected: promoted to Int for any class constraint
        other => panic!("Expected Int, got {:?}", other),
    }
}

// test_promote_string_literal_restricted — deleted: Comparable class no longer in InferState::new() after type-foundations sprint.

// test_promote_literal_label_kind_never_promotes — deleted: Numeric class no longer in InferState::new() after type-foundations sprint.

// ============================================================================
// type-soundness sprint tests
// ============================================================================

/// Union-vs-Union deferral: when both Unions contain inference vars, the equality
/// is deferred (not hard-errored) and pushed to state.deferred_equalities.
/// This covers the arm at type_unify.rs lines 1998-2004.
#[tokio::test]
async fn test_union_vs_union_with_typevars_defers() {
    let mut state = InferState::new();

    let span = rust_span!();

    // Register levels for the type vars
    state.set_level("a".to_string(), 0);
    state.set_level("b".to_string(), 0);

    // Union([Int, TypeVar(a)]) ~ Union([Str, TypeVar(b)])
    let lhs = Type::Union(vec![Type::Int, Type::TypeVar("a".to_string(), 0)]);
    let rhs = Type::Union(vec![Type::Str, Type::TypeVar("b".to_string(), 0)]);

    let result = unify_sync(&lhs, &rhs, &mut state, &mut Vec::new(), span).await;

    // Should succeed (not a hard error)
    assert!(
        result.is_ok(),
        "Union-vs-Union with TypeVars should defer, not error: {:?}",
        result.unwrap_err()
    );
    // Should have pushed exactly one deferred equality
    assert_eq!(
        state.deferred_equalities.len(),
        1,
        "Expected 1 deferred equality, got {}",
        state.deferred_equalities.len()
    );
}

/// Union-vs-Union without inference vars: should not defer, should attempt element unification.
/// Both Unions are concrete (no TypeVars), so the deferral arm does NOT fire.
#[tokio::test]
async fn test_union_vs_union_concrete_no_deferral() {
    let mut state = InferState::new();

    let span = rust_span!();

    // Union([Int]) ~ Union([Int]) — concrete, no TypeVars
    let lhs = Type::Union(vec![Type::Int]);
    let rhs = Type::Union(vec![Type::Int]);

    // This falls through to the generic _ => Err arm (no C-Var1 match either),
    // not the deferral arm. Deferred_equalities should remain empty.
    let _ = unify_sync(&lhs, &rhs, &mut state, &mut Vec::new(), span).await;

    assert_eq!(
        state.deferred_equalities.len(),
        0,
        "Concrete Union-vs-Union should NOT push a deferred equality"
    );
}

/// chr-normalization: Occurs check for TypeStageApp args
#[tokio::test]
async fn test_unify_type_var_occurs_in_type_stage_app() {
    let mut state = InferState::new();

    let span = rust_span!();

    state.set_level("a".to_string(), 0);

    let type_var_a = Type::TypeVar("a".to_string(), 0);
    let type_stage_app_f_a = Type::TypeStageApp {
        fn_name: "F".to_string(),
        args: vec![Type::TypeVar("a".to_string(), 0)],
    };

    let result = unify_sync(
        &type_var_a,
        &type_stage_app_f_a,
        &mut state,
        &mut Vec::new(),
        span,
    )
    .await;

    assert!(
        result.is_err(),
        "Expected occurs-check failure, but unification succeeded"
    );
    let err = result.unwrap_err();
    assert!(
        err.message.contains("infinite type"),
        "Expected 'infinite type' in error message, got: {}",
        err.message
    );
}

// ============================================================================
// fn-narrowing-variadic sprint tests
// ============================================================================

#[tokio::test]
async fn test_unify_variadic_zero_with_concrete_arity() {
    let mut state = InferState::new();

    let span = rust_span!();

    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::TyCon("Boolean".to_string())),
        typed_variadics: vec![],
        rest: None,
        required_count: 1,
    };

    let result = unify_sync(
        &any_function,
        &concrete_fn,
        &mut state,
        &mut Vec::new(),
        span,
    )
    .await;

    assert!(
        result.is_ok(),
        "Zero-param variadic should unify with concrete arity, got error: {:?}",
        result.unwrap_err()
    );
}

#[tokio::test]
async fn test_unify_variadic_zero_with_zero_non_variadic() {
    let mut state = InferState::new();

    let span = rust_span!();

    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    let concrete_fn = Type::Function {
        params: vec![],
        ret: Box::new(Type::Int),
        typed_variadics: vec![],
        rest: None,
        required_count: 0,
    };

    let result = unify_sync(
        &any_function,
        &concrete_fn,
        &mut state,
        &mut Vec::new(),
        span,
    )
    .await;

    assert!(
        result.is_err(),
        "Zero-param variadic should NOT unify with 0-param non-variadic"
    );
}

#[tokio::test]
async fn test_unify_variadic_zero_with_multi_param() {
    let mut state = InferState::new();

    let span = rust_span!();

    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    let concrete_fn = Type::Function {
        params: vec![
            (None, Type::Int),
            (None, Type::Str),
            (None, Type::TyCon("Boolean".to_string())),
        ],
        ret: Box::new(Type::Float),
        typed_variadics: vec![],
        rest: None,
        required_count: 3,
    };

    let result = unify_sync(
        &any_function,
        &concrete_fn,
        &mut state,
        &mut Vec::new(),
        span,
    )
    .await;

    assert!(
        result.is_ok(),
        "Zero-param variadic should unify with multi-param function, got error: {:?}",
        result.unwrap_err()
    );
}

#[tokio::test]
async fn test_is_subtype_concrete_to_any_function() {
    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::TyCon("Boolean".to_string())),
        typed_variadics: vec![],
        rest: None,
        required_count: 1,
    };

    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    assert!(
        Type::is_subtype(&concrete_fn, &any_function, None),
        "Concrete function should be subtype of any-function"
    );

    assert!(
        !Type::is_subtype(&any_function, &concrete_fn, None),
        "Any-function should NOT be subtype of concrete function"
    );
}

#[tokio::test]
async fn test_is_subtype_any_function_reflexivity() {
    let any_fn1 = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };
    let any_fn2 = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    assert!(
        Type::is_subtype(&any_fn1, &any_fn2, None),
        "Any-function should be a subtype of any-function (reflexivity — distinct objects)"
    );
    assert!(
        Type::is_subtype(&any_fn2, &any_fn1, None),
        "Any-function subtyping should be symmetric (both directions)"
    );
}

#[tokio::test]
async fn test_unify_two_any_functions() {
    let mut state = InferState::new();

    let span = rust_span!();

    let any_function_1 = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    let any_function_2 = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    let result = unify_sync(
        &any_function_1,
        &any_function_2,
        &mut state,
        &mut Vec::new(),
        span,
    )
    .await;

    assert!(
        result.is_ok(),
        "Two any-function types should unify, got error: {:?}",
        result.unwrap_err()
    );
}

#[tokio::test]
async fn test_unify_concrete_fn_with_any_function_symmetric() {
    let mut state = InferState::new();

    let span = rust_span!();

    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::TyCon("Boolean".to_string())),
        typed_variadics: vec![],
        rest: None,
        required_count: 1,
    };

    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    let result = unify_sync(
        &concrete_fn,
        &any_function,
        &mut state,
        &mut Vec::new(),
        span,
    )
    .await;

    assert!(
        result.is_ok(),
        "unify(concrete_fn, any_function) should succeed (symmetric direction), got error: {:?}",
        result.unwrap_err()
    );
}

// ============================================================================
// fn-narrowing-followup sprint tests
// ============================================================================

#[tokio::test]
async fn test_is_consistent_any_function_with_concrete() {
    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::TyCon("Boolean".to_string())),
        typed_variadics: vec![],
        rest: None,
        required_count: 1,
    };

    assert!(
        Type::is_consistent(&any_function, &concrete_fn),
        "Any-function should be consistent with concrete function"
    );

    assert!(
        Type::is_consistent(&concrete_fn, &any_function),
        "Concrete function should be consistent with any-function (symmetric)"
    );
}

#[tokio::test]
async fn test_is_consistent_any_function_with_multi_param() {
    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    let multi_param_fn = Type::Function {
        params: vec![
            (None, Type::Int),
            (None, Type::Str),
            (Some("x".to_string()), Type::TyCon("Boolean".to_string())),
        ],
        ret: Box::new(Type::Float),
        typed_variadics: vec![],
        rest: None,
        required_count: 3,
    };

    assert!(
        Type::is_consistent(&any_function, &multi_param_fn),
        "Any-function should be consistent with multi-param function"
    );
}

#[tokio::test]
async fn test_is_consistent_any_function_with_zero_param_non_variadic() {
    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    let zero_param_fn = Type::Function {
        params: vec![],
        ret: Box::new(Type::Int),
        typed_variadics: vec![],
        rest: None,
        required_count: 0,
    };

    assert!(
        Type::is_consistent(&any_function, &zero_param_fn),
        "Any-function should be consistent with zero-param non-variadic"
    );
}

#[tokio::test]
async fn test_types_are_disjoint_function_vs_int() {
    let fn_ty = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::TyCon("Boolean".to_string())),
        typed_variadics: vec![],
        rest: None,
        required_count: 1,
    };

    assert!(
        Type::types_are_disjoint(&fn_ty, &Type::Int),
        "Function should be disjoint from Int"
    );
    assert!(
        Type::types_are_disjoint(&Type::Int, &fn_ty),
        "Int should be disjoint from Function (symmetric)"
    );
}

#[tokio::test]
async fn test_types_are_disjoint_function_vs_primitives() {
    let fn_ty = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    assert!(Type::types_are_disjoint(&fn_ty, &Type::Int));
    assert!(Type::types_are_disjoint(&fn_ty, &Type::Float));
    assert!(Type::types_are_disjoint(&fn_ty, &Type::Str));
    assert!(Type::types_are_disjoint(&fn_ty, &Type::Bytes));

    assert!(Type::types_are_disjoint(&Type::Int, &fn_ty));
    assert!(Type::types_are_disjoint(&Type::Float, &fn_ty));
    assert!(Type::types_are_disjoint(&Type::Str, &fn_ty));
    assert!(Type::types_are_disjoint(&Type::Bytes, &fn_ty));
}

#[tokio::test]
async fn test_types_are_disjoint_function_vs_literals() {
    let fn_ty = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Str),
        typed_variadics: vec![],
        rest: None,
        required_count: 1,
    };

    assert!(Type::types_are_disjoint(&fn_ty, &Type::IntLiteral(42)));
    assert!(Type::types_are_disjoint(&Type::IntLiteral(42), &fn_ty));

    assert!(Type::types_are_disjoint(
        &fn_ty,
        &Type::StringLiteral("hello".to_string())
    ));
    assert!(Type::types_are_disjoint(
        &Type::StringLiteral("hello".to_string()),
        &fn_ty
    ));
}

#[tokio::test]
async fn test_types_are_disjoint_function_vs_record() {
    let fn_ty = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::TyCon("Boolean".to_string())),
        typed_variadics: vec![],
        rest: None,
        required_count: 1,
    };

    let mut fields = IndexMap::new();
    fields.insert("x".to_string(), Type::Int);
    let record_ty = Type::Dict(Row {
        fields,
        tail: crate::type_def::RowTail::Empty,
    });

    assert!(
        Type::types_are_disjoint(&fn_ty, &record_ty),
        "Function should be disjoint from Record"
    );
    assert!(
        Type::types_are_disjoint(&record_ty, &fn_ty),
        "Record should be disjoint from Function (symmetric)"
    );
}

// ============================================================================
// S-861: equirecursive-checker tests (T-1076 + T-1077)
// ============================================================================

/// T-1077: apply_type on Recursive recurses into body, does not look up var in subst.
/// Given μvar.body, applying a substitution {x → Int} should substitute through the body
/// but leave the μ-binder name (var) unchanged even if var coincidentally equals x.
#[tokio::test]
async fn test_apply_type_recursive_does_not_bind_var_name() {
    let mut state = InferState::new();

    let span = rust_span!();

    // Create a TypeVar _t0 and bind it to Int
    state.set_level("_t0".to_string(), 0);
    let tv = Type::TypeVar("_t0".to_string(), 0);
    let _ = unify_sync(&tv, &Type::Int, &mut state, &mut Vec::new(), span).await;

    // Recursive type: μμ_var.{head: _t0, tail: TypeVar(μ_var, 0)}
    // The body contains _t0 which should be substituted to Int
    // The binder "μ_var" is a μ-name, not in subst.type_map
    let rec_ty = Type::Recursive {
        var: "μ_var".to_string(),
        body: Box::new(Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("elem".to_string(), Type::TypeVar("_t0".to_string(), 0));
                m.insert("self".to_string(), Type::TypeVar("μ_var".to_string(), 0));
                m
            },
            tail: crate::type_def::RowTail::Empty,
        })),
    };

    let applied = state.apply(&rec_ty);

    // The Recursive wrapper must survive — binder name unchanged
    match &applied {
        Type::Recursive { var, body } => {
            assert_eq!(var, "μ_var", "μ-binder name must not change after apply");
            // _t0 inside body should be substituted to Int
            let body_record = match body.as_ref() {
                Type::Dict(r) => r,
                other => panic!("Expected Record body, got {:?}", other),
            };
            let elem_ty = body_record.fields.get("elem").expect("elem field missing");
            assert_eq!(
                *elem_ty,
                Type::Int,
                "_t0 in body should be substituted to Int"
            );
        }
        other => panic!("Expected Recursive after apply, got {:?}", other),
    }
}

/// T-1076 Arm 3: unify(Recursive{va, ba}, Recursive{vb, bb}) opens both with a shared fresh var.
/// Two isomorphic recursive types (same shape, different binder names) should unify.
#[tokio::test]
async fn test_unify_recursive_recursive_isomorphic() {
    let mut state = InferState::new();

    let span = rust_span!();

    // μ_a. {head: Int, tail: TypeVar("_a", 0)}
    let rec_a = Type::Recursive {
        var: "_a".to_string(),
        body: Box::new(Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("head".to_string(), Type::Int);
                m.insert("tail".to_string(), Type::TypeVar("_a".to_string(), 0));
                m
            },
            tail: crate::type_def::RowTail::Empty,
        })),
    };

    // μ_b. {head: Int, tail: TypeVar("_b", 0)} — same shape, different binder name
    let rec_b = Type::Recursive {
        var: "_b".to_string(),
        body: Box::new(Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("head".to_string(), Type::Int);
                m.insert("tail".to_string(), Type::TypeVar("_b".to_string(), 0));
                m
            },
            tail: crate::type_def::RowTail::Empty,
        })),
    };

    let result = unify_sync(&rec_a, &rec_b, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_ok(),
        "Two isomorphic recursive types should unify, got error: {:?}",
        result.unwrap_err()
    );
}

/// T-1076 Arm 3: unify(Recursive{..}, Recursive{..}) with different field types should fail.
#[tokio::test]
async fn test_unify_recursive_recursive_incompatible_fields() {
    let mut state = InferState::new();

    let span = rust_span!();

    // μ_a. {head: Int, tail: TypeVar("_a", 0)}
    let rec_int = Type::Recursive {
        var: "_a".to_string(),
        body: Box::new(Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("head".to_string(), Type::Int);
                m.insert("tail".to_string(), Type::TypeVar("_a".to_string(), 0));
                m
            },
            tail: crate::type_def::RowTail::Empty,
        })),
    };

    // μ_b. {head: Str, tail: TypeVar("_b", 0)} — different head type
    let rec_str = Type::Recursive {
        var: "_b".to_string(),
        body: Box::new(Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("head".to_string(), Type::Str);
                m.insert("tail".to_string(), Type::TypeVar("_b".to_string(), 0));
                m
            },
            tail: crate::type_def::RowTail::Empty,
        })),
    };

    let result = unify_sync(&rec_int, &rec_str, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_err(),
        "Recursive types with different field types should NOT unify"
    );
}

/// T-1076 Ordering: TypeVar arm fires before Recursive arms.
/// unify(TypeVar, Recursive) must bind the TypeVar to the full Recursive type,
/// not unfold the Recursive first.
#[tokio::test]
async fn test_unify_typevar_binds_to_recursive_type() {
    let mut state = InferState::new();

    let span = rust_span!();

    state.set_level("_t0".to_string(), 1);

    let tv = Type::TypeVar("_t0".to_string(), 1);
    let rec_ty = Type::Recursive {
        var: "_μ".to_string(),
        body: Box::new(Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".to_string(), Type::Int);
                m.insert("self".to_string(), Type::TypeVar("_μ".to_string(), 0));
                m
            },
            tail: crate::type_def::RowTail::Empty,
        })),
    };

    let result = unify_sync(&tv, &rec_ty, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_ok(),
        "TypeVar should unify with Recursive type: {:?}",
        result.unwrap_err()
    );

    // After unification, applying the substitution to _t0 should yield the Recursive type
    let applied = state.apply(&tv);
    assert!(
        matches!(applied, Type::Recursive { .. }),
        "TypeVar should be bound to the full Recursive type, not its opened body; got {:?}",
        applied
    );
}

/// T-1076 Arm 4: unify(Recursive, concrete) opens left side.
/// μa.{x: Int, tail: a} unified with {x: Int, tail: Unknown} should succeed
/// (Unknown is consistent with any type — but here we use a record without tail
/// to test the opening behavior gives a coherent result).
#[tokio::test]
async fn test_unify_recursive_left_with_typevar_right() {
    let mut state = InferState::new();

    let span = rust_span!();

    state.set_level("_t42".to_string(), 1);

    // μ_r. {x: Int}  — a trivial "recursive" type whose body doesn't reference the var
    // This is non-contractive in the full sense, but valid for testing Arm 4 mechanics:
    // the opened body is just {x: Int}, which unifies with the right side.
    let rec_ty = Type::Recursive {
        var: "_r".to_string(),
        body: Box::new(Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".to_string(), Type::Int);
                m
            },
            tail: crate::type_def::RowTail::Empty,
        })),
    };

    let record_ty = Type::Dict(Row {
        fields: {
            let mut m = IndexMap::new();
            m.insert("x".to_string(), Type::Int);
            m
        },
        tail: crate::type_def::RowTail::Empty,
    });

    let result = unify_sync(&rec_ty, &record_ty, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_ok(),
        "Recursive(body={{x:Int}}) should unify with {{x:Int}}: {:?}",
        result.unwrap_err()
    );
}

/// T-1076 Arm 5: symmetric of Arm 4 (concrete on left, Recursive on right).
#[tokio::test]
async fn test_unify_concrete_left_with_recursive_right() {
    let mut state = InferState::new();

    let span = rust_span!();

    let record_ty = Type::Dict(Row {
        fields: {
            let mut m = IndexMap::new();
            m.insert("x".to_string(), Type::Int);
            m
        },
        tail: crate::type_def::RowTail::Empty,
    });

    let rec_ty = Type::Recursive {
        var: "_r".to_string(),
        body: Box::new(Type::Dict(Row {
            fields: {
                let mut m = IndexMap::new();
                m.insert("x".to_string(), Type::Int);
                m
            },
            tail: crate::type_def::RowTail::Empty,
        })),
    };

    let result = unify_sync(&record_ty, &rec_ty, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_ok(),
        "{{x:Int}} should unify with Recursive(body={{x:Int}}): {:?}",
        result.unwrap_err()
    );
}

#[tokio::test]
async fn test_types_are_disjoint_function_vs_tycon_app() {
    let fn_ty = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        typed_variadics: vec![],
        rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
        required_count: 0,
    };

    let app_ty = Type::App(Box::new(Type::TyCon("Coll".into())), Box::new(Type::Int));

    assert!(
        Type::types_are_disjoint(&fn_ty, &app_ty),
        "Function should be disjoint from TyCon App"
    );
    assert!(
        Type::types_are_disjoint(&app_ty, &fn_ty),
        "TyCon App should be disjoint from Function (symmetric)"
    );
}

#[tokio::test]
async fn test_types_are_disjoint_function_vs_map() {
    let fn_ty = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Str),
        typed_variadics: vec![],
        rest: None,
        required_count: 1,
    };

    let map_ty = Type::map(Type::Str, Type::Int);

    assert!(
        Type::types_are_disjoint(&fn_ty, &map_ty),
        "Function should be disjoint from Map"
    );
    assert!(
        Type::types_are_disjoint(&map_ty, &fn_ty),
        "Map should be disjoint from Function (symmetric)"
    );
}

#[tokio::test]
async fn test_types_are_not_disjoint_function_vs_function() {
    let fn1 = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::TyCon("Boolean".to_string())),
        typed_variadics: vec![],
        rest: None,
        required_count: 1,
    };

    let fn2 = Type::Function {
        params: vec![(None, Type::Str)],
        ret: Box::new(Type::Float),
        typed_variadics: vec![],
        rest: None,
        required_count: 1,
    };

    assert!(
        !Type::types_are_disjoint(&fn1, &fn2),
        "Different function types should NOT be disjoint (conservative)"
    );
}

/// Handle capability PartialEq limitation: structural equality fails when capability rows
/// contain TypeVars with different names, even if they are unifiable.
/// Known-safe: unify() drives type checking, PartialEq only affects HashMap lookups
/// (false negatives are conservative).
#[tokio::test]
async fn test_handle_capability_partialeq_limitation() {
    // Create two Handle types with different TypeVar names
    let handle_a = Type::handle(Type::TypeVar("a".to_string(), 0));
    let handle_b = Type::handle(Type::TypeVar("b".to_string(), 0));

    // PartialEq will return false (structural inequality)
    assert_ne!(
        handle_a, handle_b,
        "Handle types with different TypeVar names are not structurally equal"
    );

    // However, they should unify successfully
    let mut state = InferState::new();

    let result = unify_sync(
        &handle_a,
        &handle_b,
        &mut state,
        &mut Vec::new(),
        rust_span!(),
    )
    .await;

    assert!(
        result.is_ok(),
        "Handle types with different TypeVar names should unify successfully"
    );
}

// ============================================================================
// T-913: Reverse functional dependency (bidirectional FD) inference tests
// ============================================================================

/// T-913: Reverse FD — binding a determined-position variable fires back-propagation.
///
/// Setup:
///   class MySeq with FD (0) → (1) and resolver_injective = true.
///   instance MySeq Int Str  (determining = Int at pos 0, determined = Str at pos 1)
///   constraint: MySeq [t0, t1]
///
/// Test: unify t1 (determined) with Str → should back-propagate t0 = Int.
#[tokio::test]
async fn test_reverse_fd_back_propagates_determining_type() {
    use crate::types::{ClassDecl, InstanceDecl};
    use std::sync::Arc;

    let mut state = InferState::new();

    // Create a class with FD (pos 0) → (pos 1) and injective resolver.
    let my_class = Arc::new(ClassDecl {
        name: "MySeq".to_string(),
        params: vec![("t".to_string(), Kind::Type), ("s".to_string(), Kind::Type)],
        superclasses: vec![],
        determines: vec![(vec![0], vec![1])], // pos 0 determines pos 1
        resolver: None,
        resolver_injective: true,
        structural_discharge: crate::type_class::StructuralDischarge::None,
        method_signatures: vec![],
    });

    // Register the class in state.env.
    state.env.write().unwrap().insert_class(ClassDecl {
        name: "MySeq".to_string(),
        params: vec![("t".to_string(), Kind::Type), ("s".to_string(), Kind::Type)],
        superclasses: vec![],
        determines: vec![(vec![0], vec![1])],
        resolver: None,
        resolver_injective: true,
        structural_discharge: crate::type_class::StructuralDischarge::None,
        method_signatures: vec![],
    });

    // Register instance: MySeq Int Str
    // MPTC instances are encoded as Record with numbered fields.
    let mut instance_fields = IndexMap::new();
    instance_fields.insert("0".to_string(), Type::Int); // pos 0 = Int (determining)
    instance_fields.insert("1".to_string(), Type::Str); // pos 1 = Str (determined)
    let instance_type = Type::Dict(Row {
        fields: instance_fields,
        tail: crate::type_def::RowTail::Empty,
    });
    let inst = InstanceDecl {
        class_name: "MySeq".to_string(),
        instance_type,
        det_positions: vec![0], // determining position indices
        method_types: HashMap::new(),
    };
    let mangled = format!("ɪɴꜱᴛᴀɴᴄᴇ⧼{} {}⧽", inst.class_name, inst.instance_type);
    state.env.write().unwrap().insert_instance(mangled, inst);

    // Create type variables t0 (determining, pos 0) and t1 (determined, pos 1).
    state.set_level("t0".to_string(), 0);
    state.set_level("t1".to_string(), 0);

    // Add the constraint: MySeq [t0, t1]
    let mut constraints: Vec<Constraint> = vec![Constraint::Class {
        class: my_class,
        vars: vec![
            ConstraintArg::Var("t0".to_string()),
            ConstraintArg::Var("t1".to_string()),
        ],
        origin_name: None,
        origin_span: None,
    }];

    // Unify t1 (determined position) with Str.
    // This should trigger the reverse FD and back-propagate t0 = Int.

    let t1 = Type::TypeVar("t1".to_string(), 0);
    let result = unify_sync(&t1, &Type::Str, &mut state, &mut constraints, rust_span!()).await;

    assert!(
        result.is_ok(),
        "Unifying determined var with concrete type should succeed: {:?}",
        result.unwrap_err()
    );

    // Check that t0 was back-propagated to Int via reverse FD.
    let t0_bound = state.apply(&Type::TypeVar("t0".to_string(), 0));
    assert!(
        matches!(t0_bound, Type::Int),
        "Reverse FD should have back-propagated t0 = Int, but got: {:?}",
        t0_bound
    );
}

/// T-913: Reverse FD does NOT fire when resolver_injective = false.
///
/// Same setup as above but with resolver_injective = false — the determining
/// variable must NOT be back-propagated when the resolver is not injective.
#[tokio::test]
async fn test_reverse_fd_does_not_fire_when_not_injective() {
    use crate::types::{ClassDecl, InstanceDecl};
    use std::sync::Arc;

    let mut state = InferState::new();

    // Class with the same FD but NOT injective.
    let my_class = Arc::new(ClassDecl {
        name: "MyNonInj".to_string(),
        params: vec![("t".to_string(), Kind::Type), ("s".to_string(), Kind::Type)],
        superclasses: vec![],
        determines: vec![(vec![0], vec![1])],
        resolver: None,
        resolver_injective: false, // NOT injective
        structural_discharge: crate::type_class::StructuralDischarge::None,
        method_signatures: vec![],
    });

    state.env.write().unwrap().insert_class(ClassDecl {
        name: "MyNonInj".to_string(),
        params: vec![("t".to_string(), Kind::Type), ("s".to_string(), Kind::Type)],
        superclasses: vec![],
        determines: vec![(vec![0], vec![1])],
        resolver: None,
        resolver_injective: false,
        structural_discharge: crate::type_class::StructuralDischarge::None,
        method_signatures: vec![],
    });

    // Register instance: MyNonInj Int Str
    let mut instance_fields = IndexMap::new();
    instance_fields.insert("0".to_string(), Type::Int);
    instance_fields.insert("1".to_string(), Type::Str);
    let instance_type = Type::Dict(Row {
        fields: instance_fields,
        tail: crate::type_def::RowTail::Empty,
    });
    let inst = InstanceDecl {
        class_name: "MyNonInj".to_string(),
        instance_type,
        det_positions: vec![0],
        method_types: HashMap::new(),
    };
    let mangled = format!("ɪɴꜱᴛᴀɴᴄᴇ⧼{} {}⧽", inst.class_name, inst.instance_type);
    state.env.write().unwrap().insert_instance(mangled, inst);

    state.set_level("t0".to_string(), 0);
    state.set_level("t1".to_string(), 0);

    let mut constraints: Vec<Constraint> = vec![Constraint::Class {
        class: my_class,
        vars: vec![
            ConstraintArg::Var("t0".to_string()),
            ConstraintArg::Var("t1".to_string()),
        ],
        origin_name: None,
        origin_span: None,
    }];

    // Unify t1 with Str — should NOT back-propagate t0.

    let t1 = Type::TypeVar("t1".to_string(), 0);
    let result = unify_sync(&t1, &Type::Str, &mut state, &mut constraints, rust_span!()).await;

    assert!(
        result.is_ok(),
        "Unification should succeed: {:?}",
        result.unwrap_err()
    );

    // t0 must remain unbound (no reverse FD fired).
    let t0_bound = state.apply(&Type::TypeVar("t0".to_string(), 0));
    assert!(
        matches!(t0_bound, Type::TypeVar(ref n, _) if n == "t0"),
        "With non-injective resolver, t0 must remain unbound, but got: {:?}",
        t0_bound
    );
}

// ============================================================================
// T-994: Level semantics unit tests (type-system-health-s841-followup sprint)
// ============================================================================

/// TypeVarEntry stores level, binding, and kind in one place.
#[tokio::test]
async fn test_type_var_entry_stores_level_binding_kind() {
    let mut state = InferState::new();

    // Register a TypeVar with specific level
    state.set_level("a".to_string(), 3);
    assert_eq!(state.get_level("a"), Some(3));

    // Initially unbound
    assert!(state.lookup_binding("a").is_none());

    // Bind it
    state.bind_type_var("a".to_string(), Type::Int);
    assert_eq!(state.lookup_binding("a"), Some(Type::Int));

    // Kind defaults to Type
    assert_eq!(state.get_kind("a"), Some(Kind::Type));

    // Set a non-default kind
    state.set_kind("b".to_string(), Kind::Operator);
    assert_eq!(state.get_kind("b"), Some(Kind::Operator));
}

/// bind_type_var writes to the unified type_vars map.
#[tokio::test]
async fn test_bind_type_var_writes_to_type_vars() {
    let mut state = InferState::new();

    state.set_level("var1".to_string(), 1);
    state.bind_type_var("var1".to_string(), Type::Int);

    state.set_level("var2".to_string(), 2);
    state.bind_type_var("var2".to_string(), Type::Str);

    // Both bindings are in the same map
    assert_eq!(state.lookup_binding("var1"), Some(Type::Int));
    assert_eq!(state.lookup_binding("var2"), Some(Type::Str));
    assert!(state.lookup_binding("nonexistent").is_none());
}

/// kind_env() builds a HashMap view of non-Type kinds.
#[tokio::test]
async fn test_kind_env_view() {
    let mut state = InferState::new();

    state.set_level("a".to_string(), 0);
    state.set_kind("a".to_string(), Kind::Type); // default, should not appear in kind_env
    state.set_level("b".to_string(), 0);
    state.set_kind("b".to_string(), Kind::Operator);
    state.set_level("c".to_string(), 0);
    state.set_kind("c".to_string(), Kind::Label);

    let ke = state.kind_env();
    assert!(
        !ke.contains_key("a"),
        "Kind::Type should not appear in kind_env()"
    );
    assert_eq!(ke.get("b"), Some(&Kind::Operator));
    assert_eq!(ke.get("c"), Some(&Kind::Label));
}

/// TypeVars snapshot/restore pattern.
/// This tests that cloning and restoring state.type_vars preserves bindings correctly.
#[tokio::test]
async fn test_type_vars_snapshot_restore_pattern() {
    let mut state = InferState::new();

    // Bind a variable in the initial state.
    state.set_level("original_var".to_string(), 0);
    state.bind_type_var("original_var".to_string(), Type::Int);

    // Snapshot state.type_vars before a probe.
    // per_origin_counter is intentionally NOT saved/restored — it advances monotonically.
    let saved_type_vars = state.type_vars.clone();

    // Simulate a probe that adds a new binding.
    state.set_level("probe_var".to_string(), 0);
    state.bind_type_var("probe_var".to_string(), Type::Str);

    // Verify probe binding is present before restore.
    assert_eq!(
        state.lookup_binding("probe_var"),
        Some(Type::Str),
        "Probe binding should be present before restore"
    );

    // Restore state.type_vars (discarding probe bindings).
    state.type_vars = saved_type_vars;

    // Verify original binding is preserved and probe binding is gone.
    assert_eq!(
        state.lookup_binding("original_var"),
        Some(Type::Int),
        "Original binding should be preserved after restore"
    );
    assert!(
        state.lookup_binding("probe_var").is_none(),
        "Probe binding should be gone after restore"
    );
}

// ============================================================================
// T-996: fd_in_progress cycle guard unit test
// ============================================================================

/// fd_in_progress prevents infinite mutual recursion in FD improvement.
///
/// Setup: Class with bidirectional FD (0) → (1) AND (1) → (0) (injective).
/// Without the fd_in_progress guard, binding t0 triggers:
///   forward(t0) → reverse(t1) → forward(t0) → … (infinite loop).
/// With the guard, the second forward(t0) attempt is skipped because t0 is already in progress.
#[tokio::test]
async fn test_fd_in_progress_terminates_mutual_recursion() {
    use crate::types::{ClassDecl, InstanceDecl};
    use std::sync::Arc;

    let mut state = InferState::new();

    // Create a class with a single injective FD: (0) → (1).
    // The injective flag enables reverse lookup: knowing t1=Str allows back-propagating t0=Int.
    // The fd_in_progress guard prevents the cycle:
    //   forward(t0=Int) → bind t1=Str → reverse(t1=Str) → try bind t0=Int → fd_in_progress skip.
    // This tests the guard without requiring a bidirectional FD declaration,
    // which would cause the second direction's forward lookup to fail (different det_positions).
    let my_class = Arc::new(ClassDecl {
        name: "BiDir".to_string(),
        params: vec![("a".to_string(), Kind::Type), ("b".to_string(), Kind::Type)],
        superclasses: vec![],
        determines: vec![
            (vec![0], vec![1]), // a determines b (forward only)
        ],
        resolver: None,
        resolver_injective: true, // Enables reverse lookup: b=Str → a=Int
        structural_discharge: crate::type_class::StructuralDischarge::None,
        method_signatures: vec![],
    });

    state.env.write().unwrap().insert_class(ClassDecl {
        name: "BiDir".to_string(),
        params: vec![("a".to_string(), Kind::Type), ("b".to_string(), Kind::Type)],
        superclasses: vec![],
        determines: vec![(vec![0], vec![1])],
        resolver: None,
        resolver_injective: true,
        structural_discharge: crate::type_class::StructuralDischarge::None,
        method_signatures: vec![],
    });

    // Register instance: BiDir Int Str
    let mut instance_fields = IndexMap::new();
    instance_fields.insert("0".to_string(), Type::Int);
    instance_fields.insert("1".to_string(), Type::Str);
    let instance_type = Type::Dict(Row {
        fields: instance_fields,
        tail: crate::type_def::RowTail::Empty,
    });
    let inst = InstanceDecl {
        class_name: "BiDir".to_string(),
        instance_type,
        det_positions: vec![0], // Position 0 determines position 1 (forward FD)
        method_types: HashMap::new(),
    };
    let mangled = format!("ɪɴꜱᴛᴀɴᴄᴇ⧼{} {}⧽", inst.class_name, inst.instance_type);
    state.env.write().unwrap().insert_instance(mangled, inst);

    // Create type variables.
    state.set_level("t0".to_string(), 0);
    state.set_level("t1".to_string(), 0);

    // Add the constraint.
    let mut constraints: Vec<Constraint> = vec![Constraint::Class {
        class: my_class,
        vars: vec![
            ConstraintArg::Var("t0".to_string()),
            ConstraintArg::Var("t1".to_string()),
        ],
        origin_name: None,
        origin_span: None,
    }];

    // Unify t0 with Int. This should:
    // 1. Forward FD: t0=Int → t1=Str
    // 2. Reverse FD: t1=Str → attempt to bind t0 (but t0 is in fd_in_progress, so skip)
    // Result: terminates successfully without infinite loop.

    let t0 = Type::TypeVar("t0".to_string(), 0);
    let result = unify_sync(&t0, &Type::Int, &mut state, &mut constraints, rust_span!()).await;

    assert!(
        result.is_ok(),
        "Mutual FD should terminate with fd_in_progress guard: {:?}",
        result.unwrap_err()
    );

    // Verify both variables were bound correctly.
    // Both t0 and t1 are bound in state.type_vars (the unified binding store).
    let t0_bound = state.apply(&Type::TypeVar("t0".to_string(), 0));
    let t1_bound = state.apply(&Type::TypeVar("t1".to_string(), 0));

    assert!(
        matches!(t0_bound, Type::Int),
        "t0 should be bound to Int, got: {:?}",
        t0_bound
    );
    assert!(
        matches!(t1_bound, Type::Str),
        "t1 should be bound to Str via FD, got: {:?}",
        t1_bound
    );

    // Verify fd_in_progress is cleared after unification completes.
    assert!(
        state.fd_in_progress.is_empty(),
        "fd_in_progress should be empty after unification completes"
    );
}

// ============================================================================
// T-1020: Variance/TyConDef/UNIFY-TYCON/UNIFY-UNIFORM unit tests
// ============================================================================

/// T-1020a: Variance enum Display — each variant has the expected display string.
#[tokio::test]
async fn test_variance_debug_display() {
    // We test Debug since Variance derives Debug.
    assert_eq!(format!("{:?}", Variance::Covariant), "Covariant");
    assert_eq!(format!("{:?}", Variance::Contravariant), "Contravariant");
    assert_eq!(format!("{:?}", Variance::Invariant), "Invariant");
    assert_eq!(format!("{:?}", Variance::Phantom), "Phantom");
}

/// T-1020b: Variance ordering — Covariant, Contravariant, Invariant, Phantom are all distinct.
/// PartialEq is derived, so equality and inequality work correctly.
#[tokio::test]
async fn test_variance_equality_and_distinctness() {
    assert_eq!(Variance::Covariant, Variance::Covariant);
    assert_eq!(Variance::Contravariant, Variance::Contravariant);
    assert_eq!(Variance::Invariant, Variance::Invariant);
    assert_eq!(Variance::Phantom, Variance::Phantom);

    // All four variants are mutually distinct.
    assert_ne!(Variance::Covariant, Variance::Contravariant);
    assert_ne!(Variance::Covariant, Variance::Invariant);
    assert_ne!(Variance::Covariant, Variance::Phantom);
    assert_ne!(Variance::Contravariant, Variance::Invariant);
    assert_ne!(Variance::Contravariant, Variance::Phantom);
    assert_ne!(Variance::Invariant, Variance::Phantom);
}

/// T-1020c: Variance Copy — can be copied without moving.
#[tokio::test]
async fn test_variance_is_copy() {
    let v = Variance::Covariant;
    let v2 = v; // Copy
    assert_eq!(v, v2);
}

/// T-1020d: TyConDef construction with variance and constructors.
/// Unit constructor (arity 0) and field constructor (arity 1) can be stored.
#[tokio::test]
async fn test_tycondef_construction() {
    let def = TyConDef {
        params: vec!["a".to_string()],
        body: Type::Unknown,
        constraints: vec![],
        variance: vec![Variance::Covariant],
        constructors: vec![("Maybe.Some".to_string(), 1), ("Maybe.None".to_string(), 0)],
        builtin_type: None,
        annotation: None,
        field_annotations: indexmap::IndexMap::new(),
        constructor_constants: indexmap::IndexMap::new(),
        definition_span: None,
    };

    assert_eq!(def.variance, vec![Variance::Covariant]);
    assert_eq!(def.constructors.len(), 2);
    assert_eq!(def.constructors[0], ("Maybe.Some".to_string(), 1));
    assert_eq!(def.constructors[1], ("Maybe.None".to_string(), 0));
    assert!(def.builtin_type.is_none());
}

/// T-1020e: TyConDef with multiple variance parameters (bivariant map).
#[tokio::test]
async fn test_tycondef_multi_variance() {
    let def = TyConDef {
        params: vec!["a".to_string(), "b".to_string()],
        body: Type::Unknown,
        constraints: vec![],
        variance: vec![Variance::Contravariant, Variance::Covariant],
        constructors: vec![],
        builtin_type: None,
        annotation: None,
        field_annotations: indexmap::IndexMap::new(),
        constructor_constants: indexmap::IndexMap::new(),
        definition_span: None,
    };

    assert_eq!(def.variance.len(), 2);
    assert_eq!(def.variance[0], Variance::Contravariant);
    assert_eq!(def.variance[1], Variance::Covariant);
}

/// T-1020f: TyConDef with builtin_type discriminant.
#[tokio::test]
async fn test_tycondef_builtin_type() {
    let def = TyConDef {
        params: vec!["a".to_string()],
        body: Type::Unknown,
        constraints: vec![],
        variance: vec![Variance::Covariant],
        constructors: vec![],
        builtin_type: Some("Seq".to_string()),
        annotation: None,
        field_annotations: indexmap::IndexMap::new(),
        constructor_constants: indexmap::IndexMap::new(),
        definition_span: None,
    };

    assert_eq!(def.builtin_type, Some("Seq".to_string()));
}

/// T-1020g: UNIFY-TYCON — same name unifies successfully.
/// Two TyCon("Color") values should unify with Ok(()).
#[tokio::test]
async fn test_unify_tycon_same_name_ok() {
    let mut state = InferState::new();

    let span = rust_span!();

    let ty1 = Type::TyCon("Color".to_string());
    let ty2 = Type::TyCon("Color".to_string());

    let result = unify_sync(&ty1, &ty2, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_ok(),
        "TyCon(\"Color\") ~ TyCon(\"Color\") should unify: {:?}",
        result.unwrap_err()
    );
}

/// T-1020h: UNIFY-TYCON — different names fail unification.
/// TyCon("Color") and TyCon("Shape") are distinct nominal types.
#[tokio::test]
async fn test_unify_tycon_different_name_err() {
    let mut state = InferState::new();

    let span = rust_span!();

    let ty1 = Type::TyCon("Color".to_string());
    let ty2 = Type::TyCon("Shape".to_string());

    let result = unify_sync(&ty1, &ty2, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_err(),
        "TyCon(\"Color\") and TyCon(\"Shape\") must not unify"
    );
}

/// T-1020i: UNIFY-TYCON — TyCon does not unify with TyCon of empty string.
/// Name equality is required regardless of triviality.
#[tokio::test]
async fn test_unify_tycon_vs_empty_name_err() {
    let mut state = InferState::new();

    let span = rust_span!();

    let ty1 = Type::TyCon("Foo".to_string());
    let ty2 = Type::TyCon("".to_string());

    let result = unify_sync(&ty1, &ty2, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_err(),
        "TyCon(\"Foo\") and TyCon(\"\") must not unify"
    );
}

/// T-1020j: UNIFY-UNIFORM — two Uniform tails with the same value type should not error
/// when unified and all named fields conform to the Uniform value type (T-1007/T-1024).
///
/// After T-1024 implementation: the Uniform constraint applies to ALL entries (named and
/// unnamed). `{x: Int, _: Int}` unified with itself should succeed (Int <: Int).
/// But `{x: Int, _: Str}` is a contradiction (Int does not conform to Str) and should fail.
#[tokio::test]
async fn test_unify_uniform_same_value_type_records_ok() {
    let mut state = InferState::new();

    let span = rust_span!();

    // Consistent: named field type matches Uniform value type.
    // {x: Int, _ : Int} ~ {x: Int, _ : Int} — should unify successfully.
    let mut fields = IndexMap::new();
    fields.insert("x".to_string(), Type::Int);
    let row = crate::type_def::Row {
        fields: fields.clone(),
        tail: crate::type_def::RowTail::Uniform {
            key: None,
            value: Box::new(Type::Int),
        },
    };
    let rec1 = Type::Dict(row.clone());
    let rec2 = Type::Dict(row);

    let result = unify_sync(&rec1, &rec2, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_ok(),
        "Identical Uniform-tailed records with consistent named fields should unify: {:?}",
        result.unwrap_err()
    );
}

/// T-1020j2: UNIFY-UNIFORM — named field type must conform to Uniform value type (T-1007 step 3).
/// `{x: Int, _: Str}` is a contradiction: x is Int but the Uniform constraint requires Str.
#[tokio::test]
async fn test_unify_uniform_inconsistent_named_field_type_errors() {
    let mut state = InferState::new();

    let span = rust_span!();

    // Inconsistent: named field type does NOT match Uniform value type.
    // {x: Int, _: Str} ~ {} — should fail because x:Int does not conform to Uniform(Str).
    // Two non-identical records force unify_rows to run the UNIFY-UNIFORM check.
    // Identical records would short-circuit via `a == b` in unify(), bypassing the check.
    let mut fields = IndexMap::new();
    fields.insert("x".to_string(), Type::Int);
    let row1 = crate::type_def::Row {
        fields,
        tail: crate::type_def::RowTail::Uniform {
            key: None,
            value: Box::new(Type::Str),
        },
    };
    let row2 = crate::type_def::Row {
        fields: IndexMap::new(),
        tail: crate::type_def::RowTail::Empty,
    };
    let rec1 = Type::Dict(row1);
    let rec2 = Type::Dict(row2);

    let result = unify_sync(&rec1, &rec2, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_err(),
        "Uniform-tailed record with non-conforming named field should fail unification"
    );
    let err = result.unwrap_err();
    let err_msg = err.message;
    assert!(
        err_msg.contains("does not conform to Uniform constraint"),
        "Expected Uniform constraint violation, got: {err_msg}"
    );
}

/// T-1116a: UNIFY-UNIFORM Empty+Uniform — TypeVar join.
/// Unifying `{x: Int}` (Empty-tailed) with `{_ : α}` (Uniform with TypeVar) should
/// bind α to Int (the join of all named fields from both rows).
#[tokio::test]
async fn test_unify_empty_uniform_typevar_join() {
    let mut state = InferState::new();

    let span = rust_span!();

    // LHS: {x: Int} with Empty tail
    let mut fields_lhs = IndexMap::new();
    fields_lhs.insert("x".to_string(), Type::Int);
    let row_lhs = crate::type_def::Row {
        fields: fields_lhs,
        tail: crate::type_def::RowTail::Empty,
    };

    // RHS: {_ : α} with Uniform TypeVar tail
    let alpha = "_t_eu_test1".to_string();
    state.set_level(alpha.clone(), 0);
    let row_rhs = crate::type_def::Row {
        fields: IndexMap::new(),
        tail: crate::type_def::RowTail::Uniform {
            key: None,
            value: Box::new(Type::TypeVar(alpha.clone(), 0)),
        },
    };

    let rec_lhs = Type::Dict(row_lhs);
    let rec_rhs = Type::Dict(row_rhs);

    let result = unify_sync(&rec_lhs, &rec_rhs, &mut state, &mut Vec::new(), span).await;
    assert!(
        result.is_ok(),
        "Empty+Uniform TypeVar join should succeed: {:?}",
        result.unwrap_err()
    );

    // After unification, α should be bound to Int (the field type from the Empty side).
    let resolved = state.apply(&Type::TypeVar(alpha, 0));
    assert_eq!(
        resolved,
        Type::Int,
        "α should be bound to Int after Empty+Uniform join"
    );
}

/// T-1116b: UNIFY-UNIFORM Empty+Uniform — concrete subtype failure.
/// Unifying `{x: Int}` (Empty-tailed) with `{_ : Str}` (Uniform with concrete Str) should
/// fail because Int is not a subtype of Str.
#[tokio::test]
async fn test_unify_empty_uniform_concrete_subtype_fail() {
    let mut state = InferState::new();

    let span = rust_span!();

    // LHS: {x: Int} with Empty tail
    let mut fields_lhs = IndexMap::new();
    fields_lhs.insert("x".to_string(), Type::Int);
    let row_lhs = crate::type_def::Row {
        fields: fields_lhs,
        tail: crate::type_def::RowTail::Empty,
    };

    // RHS: {_ : Str} with Uniform concrete tail
    let row_rhs = crate::type_def::Row {
        fields: IndexMap::new(),
        tail: crate::type_def::RowTail::Uniform {
            key: None,
            value: Box::new(Type::Str),
        },
    };

    let rec_lhs = Type::Dict(row_lhs);
    let rec_rhs = Type::Dict(row_rhs);

    let result = unify_sync(&rec_lhs, &rec_rhs, &mut state, &mut Vec::new(), span).await;
    assert!(
        result.is_err(),
        "Empty+Uniform with non-conforming concrete type should fail"
    );
    let err = result.unwrap_err();
    let err_msg = err.message;
    assert!(
        err_msg.contains("does not conform to Uniform constraint")
            || err_msg.contains("cannot unify"),
        "Expected Uniform constraint violation or type mismatch, got: {err_msg}"
    );
}

/// T-1020k: Variance is preserved through Clone.
#[tokio::test]
async fn test_variance_clone() {
    let variances = vec![
        Variance::Covariant,
        Variance::Contravariant,
        Variance::Invariant,
        Variance::Phantom,
    ];
    let cloned = variances.clone();
    assert_eq!(variances, cloned);
}

/// T-1020l: TyConDef PartialEq — identical defs are equal.
#[tokio::test]
async fn test_tycondef_partialeq() {
    let def1 = TyConDef {
        params: vec![],
        body: Type::Unknown,
        constraints: vec![],
        variance: vec![Variance::Invariant],
        constructors: vec![("X.A".to_string(), 0), ("X.B".to_string(), 1)],
        builtin_type: None,
        annotation: None,
        field_annotations: indexmap::IndexMap::new(),
        constructor_constants: indexmap::IndexMap::new(),
        definition_span: None,
    };
    let def2 = TyConDef {
        params: vec![],
        body: Type::Unknown,
        constraints: vec![],
        variance: vec![Variance::Invariant],
        constructors: vec![("X.A".to_string(), 0), ("X.B".to_string(), 1)],
        builtin_type: None,
        annotation: None,
        field_annotations: indexmap::IndexMap::new(),
        constructor_constants: indexmap::IndexMap::new(),
        definition_span: None,
    };
    assert_eq!(def1, def2);
}

/// T-1020m: TyConDef PartialEq — different variance makes defs unequal.
#[tokio::test]
async fn test_tycondef_partialeq_different_variance() {
    let def1 = TyConDef {
        params: vec![],
        body: Type::Unknown,
        constraints: vec![],
        variance: vec![Variance::Covariant],
        constructors: vec![],
        builtin_type: None,
        annotation: None,
        field_annotations: indexmap::IndexMap::new(),
        constructor_constants: indexmap::IndexMap::new(),
        definition_span: None,
    };
    let def2 = TyConDef {
        params: vec![],
        body: Type::Unknown,
        constraints: vec![],
        variance: vec![Variance::Invariant],
        constructors: vec![],
        builtin_type: None,
        annotation: None,
        field_annotations: indexmap::IndexMap::new(),
        constructor_constants: indexmap::IndexMap::new(),
        definition_span: None,
    };
    assert_ne!(def1, def2);
}

/// T-1098: RowTail::Uniform polarity — infer_variance on a Uniform-tailed record
/// should report the value type param as Covariant (positive position).
#[tokio::test]
async fn test_infer_variance_uniform_tail_covariant() {
    use std::rc::Rc;
    let env = Rc::new(TypeEnv::new());

    // Type alias body: {x: a, _: a} — both named field and Uniform tail use param "a"
    // in positive position, so inferred variance should be Covariant.
    let mut fields = IndexMap::new();
    fields.insert("x".to_string(), Type::TypeVar("_t0".to_string(), 0));
    let body = Type::Dict(crate::type_def::Row {
        fields,
        tail: crate::type_def::RowTail::Uniform {
            key: None,
            value: Box::new(Type::TypeVar("_t0".to_string(), 0)),
        },
    });

    let variances =
        crate::typecheck::typecheck_annot::infer_variance(&body, &["_t0".to_string()], &env);
    assert_eq!(variances.len(), 1);
    assert_eq!(
        variances[0],
        Variance::Covariant,
        "Uniform tail value type param should be Covariant (positive polarity)"
    );
}

// ============================================================================
// T-1112: UNIFY-TYCON-EXPAND tests
// ============================================================================

/// T-1112a: UNIFY-TYCON-EXPAND — TyCon with registered body should unify with
/// a NominalVariant that is a member of its union body.
///
/// When `@Color` (a zero-arity TyCon) is unified with `NominalVariant{tag:"Color.Red", ...}`,
/// UNIFY-TYCON-EXPAND looks up the registered body (Union of NominalVariants) and checks
/// membership via is_subtype.
#[tokio::test]
async fn test_unify_tycon_expand_nominal_variant_member_ok() {
    use std::sync::Arc;

    let mut state = InferState::new();

    let span = rust_span!();

    // Build body: Union([NominalVariant{Red}, NominalVariant{Green}])
    let red = Type::NominalVariant {
        tycon: "Color".to_string(),
        ctor: "Red".to_string(),
        fields: crate::type_def::Row {
            fields: IndexMap::new(),
            tail: crate::type_def::RowTail::Empty,
        },
    };
    let green = Type::NominalVariant {
        tycon: "Color".to_string(),
        ctor: "Green".to_string(),
        fields: crate::type_def::Row {
            fields: IndexMap::new(),
            tail: crate::type_def::RowTail::Empty,
        },
    };
    let body = Type::Union(vec![red.clone(), green.clone()]);

    // Register TyConDef for "Color" in tycon_env with the body
    let tycon_def = Arc::new(TyConDef {
        params: vec![],
        body: body.clone(),
        constraints: vec![],
        variance: vec![],
        constructors: vec![("Color.Red".to_string(), 0), ("Color.Green".to_string(), 0)],
        builtin_type: None,
        annotation: None,
        field_annotations: indexmap::IndexMap::new(),
        constructor_constants: indexmap::IndexMap::new(),
        definition_span: None,
    });
    state.tycon_env.insert("Color".to_string(), tycon_def);

    let tycon = Type::TyCon("Color".to_string());

    // Unify @Color with NominalVariant(Red) — should succeed via UNIFY-TYCON-EXPAND
    let result = unify_sync(&tycon, &red, &mut state, &mut Vec::new(), span.clone()).await;
    assert!(
        result.is_ok(),
        "TyCon(@Color) should unify with NominalVariant(Red) via body expansion, got: {:?}",
        result.unwrap_err()
    );
}

/// T-1112b: UNIFY-TYCON-EXPAND — TyCon should NOT unify with a NominalVariant
/// that is NOT a member of its union body.
#[tokio::test]
async fn test_unify_tycon_expand_nominal_variant_non_member_fails() {
    use std::sync::Arc;

    let mut state = InferState::new();

    let span = rust_span!();

    let red = Type::NominalVariant {
        tycon: "Color".to_string(),
        ctor: "Red".to_string(),
        fields: crate::type_def::Row {
            fields: IndexMap::new(),
            tail: crate::type_def::RowTail::Empty,
        },
    };
    let green = Type::NominalVariant {
        tycon: "Color".to_string(),
        ctor: "Green".to_string(),
        fields: crate::type_def::Row {
            fields: IndexMap::new(),
            tail: crate::type_def::RowTail::Empty,
        },
    };
    let body = Type::Union(vec![red.clone(), green.clone()]);

    let tycon_def = Arc::new(TyConDef {
        params: vec![],
        body,
        constraints: vec![],
        variance: vec![],
        constructors: vec![("Color.Red".to_string(), 0), ("Color.Green".to_string(), 0)],
        builtin_type: None,
        annotation: None,
        field_annotations: indexmap::IndexMap::new(),
        constructor_constants: indexmap::IndexMap::new(),
        definition_span: None,
    });
    state.tycon_env.insert("Color".to_string(), tycon_def);

    let tycon = Type::TyCon("Color".to_string());

    // Blue is not in the union — should fail
    let blue = Type::NominalVariant {
        tycon: "Color".to_string(),
        ctor: "Blue".to_string(),
        fields: crate::type_def::Row {
            fields: IndexMap::new(),
            tail: crate::type_def::RowTail::Empty,
        },
    };
    let result = unify_sync(&tycon, &blue, &mut state, &mut Vec::new(), span).await;
    assert!(
        result.is_err(),
        "TyCon(@Color) should NOT unify with NominalVariant(Blue) which is not in Color's union"
    );
}

/// T-1112c: UNIFY-TYCON-EXPAND — TyCon with no registered body cannot unify with NominalVariant.
///
/// When the TyCon name is not in tycon_env, the type is opaque and NominalVariant unification
/// must fail (type mismatch).
#[tokio::test]
async fn test_unify_tycon_expand_no_registered_body_fails() {
    let mut state = InferState::new();

    let span = rust_span!();

    // "Unknown" is not registered in tycon_env
    let tycon = Type::TyCon("Unknown".to_string());
    let variant = Type::NominalVariant {
        tycon: "Unknown".to_string(),
        ctor: "A".to_string(),
        fields: crate::type_def::Row {
            fields: IndexMap::new(),
            tail: crate::type_def::RowTail::Empty,
        },
    };

    let result = unify_sync(&tycon, &variant, &mut state, &mut Vec::new(), span).await;
    assert!(
        result.is_err(),
        "TyCon with no registered body should not unify with NominalVariant"
    );
}

/// T-1112d: UNIFY-TYCON-EXPAND symmetry — NominalVariant on LHS, TyCon on RHS.
///
/// The arm is symmetric: both (TyCon, NominalVariant) and (NominalVariant, TyCon)
/// should produce the same result.
#[tokio::test]
async fn test_unify_tycon_expand_symmetric() {
    use std::sync::Arc;

    let mut state = InferState::new();

    let span = rust_span!();

    let red = Type::NominalVariant {
        tycon: "Color".to_string(),
        ctor: "Red".to_string(),
        fields: crate::type_def::Row {
            fields: IndexMap::new(),
            tail: crate::type_def::RowTail::Empty,
        },
    };
    let body = Type::Union(vec![red.clone()]);

    let tycon_def = Arc::new(TyConDef {
        params: vec![],
        body,
        constraints: vec![],
        variance: vec![],
        constructors: vec![("Color.Red".to_string(), 0)],
        builtin_type: None,
        annotation: None,
        field_annotations: indexmap::IndexMap::new(),
        constructor_constants: indexmap::IndexMap::new(),
        definition_span: None,
    });
    state
        .tycon_env
        .insert("Color".to_string(), Arc::clone(&tycon_def));

    let tycon = Type::TyCon("Color".to_string());

    // (TyCon, NominalVariant) direction
    let r1 = unify_sync(&tycon, &red, &mut state, &mut Vec::new(), span.clone()).await;
    // (NominalVariant, TyCon) direction
    let r2 = unify_sync(&red, &tycon, &mut state, &mut Vec::new(), span).await;

    assert!(
        r1.is_ok(),
        "TyCon ~ NominalVariant should succeed: {:?}",
        r1.unwrap_err()
    );
    assert!(
        r2.is_ok(),
        "NominalVariant ~ TyCon should succeed: {:?}",
        r2.unwrap_err()
    );
}

// ============================================================================
// S-883: constrain() and compact() unit tests (TEST-1)
// ============================================================================

/// constrain() Error absorption: constrain(Error, Int) must return Ok(()) and not propagate
/// a cascade error. This covers the `(Type::Error, _) | (_, Type::Error) => Ok(())` arm.
#[tokio::test]
async fn test_constrain_error_absorption() {
    let mut state = InferState::new();

    let span = rust_span!();

    let result = constrain(
        &Type::error_note("test error sentinel"),
        &Type::Int,
        &mut state,
        &mut Vec::new(),
        span.clone(),
    )
    .await;
    assert!(
        result.is_ok(),
        "constrain(Error, Int) should absorb silently (Error absorption arm), got: {:?}",
        result.unwrap_err()
    );

    let result2 = constrain(
        &Type::Int,
        &Type::error_note("test error sentinel"),
        &mut state,
        &mut Vec::new(),
        span,
    )
    .await;
    assert!(
        result2.is_ok(),
        "constrain(Int, Error) should absorb silently (Error absorption arm), got: {:?}",
        result2.unwrap_err()
    );
}

/// unify() Error absorption: unify(Error, T) must return Ok(()) and not propagate
/// a cascade error. This covers the `(Type::Error(_), _) | (_, Type::Error(_)) => Ok(())` arm.
/// T-1645: Type::Error unifies with everything.
#[tokio::test]
async fn test_unify_error_absorption() {
    let mut state = InferState::new();

    let span = rust_span!();

    // Test unify(Error, Int) — Error on left side
    let result = unify_sync(
        &Type::error_note("test error sentinel"),
        &Type::Int,
        &mut state,
        &mut Vec::new(),
        span.clone(),
    )
    .await;
    assert!(
        result.is_ok(),
        "unify(Error, Int) should absorb silently (Error absorption arm), got: {:?}",
        result.unwrap_err()
    );

    // Test unify(Int, Error) — Error on right side (symmetric)
    let result2 = unify_sync(
        &Type::Int,
        &Type::error_note("test error sentinel"),
        &mut state,
        &mut Vec::new(),
        span.clone(),
    )
    .await;
    assert!(
        result2.is_ok(),
        "unify(Int, Error) should absorb silently (Error absorption arm), got: {:?}",
        result2.unwrap_err()
    );

    // Test unify(Error, Str) — Error with different concrete type
    let result3 = unify_sync(
        &Type::error_note("test error sentinel"),
        &Type::Str,
        &mut state,
        &mut Vec::new(),
        span.clone(),
    )
    .await;
    assert!(
        result3.is_ok(),
        "unify(Error, Str) should absorb silently (Error absorption arm), got: {:?}",
        result3.unwrap_err()
    );

    // Test unify(Str, Error) — symmetric variant
    let result4 = unify_sync(
        &Type::Str,
        &Type::error_note("test error sentinel"),
        &mut state,
        &mut Vec::new(),
        span,
    )
    .await;
    assert!(
        result4.is_ok(),
        "unify(Str, Error) should absorb silently (Error absorption arm), got: {:?}",
        result4.unwrap_err()
    );
}

/// C-Var1: constrain(Int, Union([Str, TypeVar(α), TypeVar(β)])) should add a lower bound
/// on α and β (multiple TypeVars → bound accumulation path).
/// With a single TypeVar, C-Var1 binds directly via subst; with multiple TypeVars it uses bounds.
/// This test uses two TypeVars to exercise the multi-TypeVar bounds accumulation path.
/// Covers the C-VAR1 arm: `(_, Union(members)) if TypeVar in members`.
#[tokio::test]
async fn test_constrain_cvar1_multi_typevar_in_union_adds_bounds() {
    let mut state = InferState::new();

    let span = rust_span!();

    // Register α and β at level 0.
    state.set_level("α".to_string(), 0);
    state.set_level("β".to_string(), 0);

    let alpha = Type::TypeVar("α".to_string(), 0);
    let beta = Type::TypeVar("β".to_string(), 0);
    // Two TypeVars in the union: C-Var1 multi-TypeVar path → adds to bounds, not subst.
    let sup = Type::Union(vec![Type::Str, alpha.clone(), beta.clone()]);

    let result = constrain(&Type::Int, &sup, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_ok(),
        "C-Var1 multi-TypeVar rewrite should succeed, got: {:?}",
        result.unwrap_err()
    );

    // Both α and β must have lower bound entries (multi-TypeVar path adds bounds to all TypeVars).
    let alpha_has_bounds = state
        .bounds
        .get("α")
        .map(|b| !b.lower.is_empty())
        .unwrap_or(false);
    let beta_has_bounds = state
        .bounds
        .get("β")
        .map(|b| !b.lower.is_empty())
        .unwrap_or(false);
    assert!(
        alpha_has_bounds && beta_has_bounds,
        "C-Var1 multi-TypeVar path must add lower bounds for ALL TypeVars in state.bounds"
    );
    // The bound should be Type::Int (the sub type being constrained).
    assert!(
        state
            .bounds
            .get("α")
            .map(|b| b.lower.contains(&Type::Int))
            .unwrap_or(false),
        "C-Var1 multi-TypeVar: α's lower bound must contain Int"
    );
    assert!(
        state
            .bounds
            .get("β")
            .map(|b| b.lower.contains(&Type::Int))
            .unwrap_or(false),
        "C-Var1 multi-TypeVar: β's lower bound must contain Int"
    );
}

/// C-Var1 single TypeVar: constrain(Int, Union([Str, TypeVar(α)])) binds α directly via subst.
/// Int is not a subtype of Str, so the residual `Int & ~Str = Int` is bound to α.
#[tokio::test]
async fn test_constrain_cvar1_single_typevar_binds_subst() {
    let mut state = InferState::new();

    let span = rust_span!();

    // Register α at level 0.
    state.set_level("α".to_string(), 0);

    let alpha = Type::TypeVar("α".to_string(), 0);
    let sup = Type::Union(vec![Type::Str, alpha.clone()]);

    let result = constrain(&Type::Int, &sup, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_ok(),
        "C-Var1 single-TypeVar rewrite should succeed, got: {:?}",
        result.unwrap_err()
    );

    // With a single TypeVar, C-Var1 binds α in the substitution (equational constraint).
    // Int & ~Str ≈ Int (since Int and Str are disjoint). α must be bound to Int (or equivalent).
    let alpha_applied = state.apply(&alpha);
    assert!(
        !matches!(alpha_applied, Type::TypeVar(ref n, _) if n == "α"),
        "C-Var1 single-TypeVar must bind α in subst (not leave it free); got: {:?}",
        alpha_applied
    );
    assert_eq!(
        alpha_applied,
        Type::Int,
        "C-Var1 single TypeVar must bind α to the residual type Int; got: {:?}",
        alpha_applied
    );
}

/// TypeVar lower bound accumulation: constrain(Int, TypeVar(α)) must add Int as a lower bound
/// on α rather than binding α = Int in the substitution.
/// Covers the `(_, TypeVar(α)) if !sub.has_inference_vars()` arm.
#[tokio::test]
async fn test_constrain_typevar_lower_bound_added() {
    let mut state = InferState::new();

    let span = rust_span!();

    // Register β at level 0.
    state.set_level("β".to_string(), 0);

    let beta = Type::TypeVar("β".to_string(), 0);

    let result = constrain(&Type::Int, &beta, &mut state, &mut Vec::new(), span).await;

    assert!(
        result.is_ok(),
        "constrain(Int, TypeVar(β)) should succeed, got: {:?}",
        result.unwrap_err()
    );

    // β must NOT be bound in the substitution (directional bound accumulation, not equality).
    let beta_applied = state.apply(&beta);
    assert!(
        matches!(beta_applied, Type::TypeVar(ref n, _) if n == "β"),
        "constrain(Int, TypeVar) must not bind β in subst (use bounds instead); got: {:?}",
        beta_applied
    );

    // β must have Int as a lower bound in state.bounds.
    let bounds = state.bounds.get("β").expect("β should have a bounds entry");
    assert!(
        !bounds.lower.is_empty(),
        "β must have at least one lower bound after constrain(Int, TypeVar(β))"
    );
    assert!(
        bounds.lower.contains(&Type::Int),
        "β's lower bound must include Int; got: {:?}",
        bounds.lower
    );
}
