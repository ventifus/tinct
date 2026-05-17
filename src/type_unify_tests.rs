//! Unit tests for type_precision_fixes sprint tasks

use super::{
    promote_literal_for_constrained_var, resolve_has_field, unify, MAX_RESOLVE_HAS_FIELD_DEPTH,
};
use crate::ast::Span;
use crate::types::{Constraint, InferState, Kind, Label, Row, Substitution, Type};
use std::collections::HashMap;

/// Task 1a: resolve_has_field on Type::Top should return Top (not Unknown)
#[test]
fn test_resolve_has_field_top_returns_top() {
    let mut state = InferState::new();
    let label = Label::Concrete("x".to_string());
    let span = Span::origin();

    let result = resolve_has_field(&label, &Type::Top, &mut state, span, 0);

    assert!(result.is_ok());
    match result.unwrap() {
        Type::Top => {} // Expected
        other => panic!("Expected Top, got {:?}", other),
    }
}

/// Task 1b: resolve_has_field with depth overflow should error (not return Unknown)
#[test]
fn test_resolve_has_field_depth_overflow_errors() {
    let mut state = InferState::new();
    let label = Label::Concrete("x".to_string());
    let span = Span::origin();

    // Create a simple record to test depth overflow
    let mut fields = HashMap::new();
    fields.insert("x".to_string(), Type::Int);
    let record_ty = Type::Record(Row { fields });

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
#[test]
fn test_types_are_disjoint_single_field_records() {
    let mut fields1 = HashMap::new();
    fields1.insert("x".to_string(), Type::Int);
    let rec1 = Type::Record(Row { fields: fields1 });

    let mut fields2 = HashMap::new();
    fields2.insert("y".to_string(), Type::Str);
    let rec2 = Type::Record(Row { fields: fields2 });

    assert!(
        Type::types_are_disjoint(&rec1, &rec2),
        "{{x: Int}} and {{y: Str}} should be disjoint"
    );
}

/// Task 3b: Single-field records with same key are NOT disjoint
#[test]
fn test_types_are_not_disjoint_same_key_records() {
    let mut fields1 = HashMap::new();
    fields1.insert("x".to_string(), Type::Int);
    let rec1 = Type::Record(Row { fields: fields1 });

    let mut fields2 = HashMap::new();
    fields2.insert("x".to_string(), Type::Str);
    let rec2 = Type::Record(Row { fields: fields2 });

    assert!(
        !Type::types_are_disjoint(&rec1, &rec2),
        "{{x: Int}} and {{x: Str}} should NOT be disjoint (conservative - field overlap)"
    );
}

/// Task 3c: Multi-field records are conservatively NOT disjoint
#[test]
fn test_types_are_not_disjoint_multi_field_records() {
    let mut fields1 = HashMap::new();
    fields1.insert("x".to_string(), Type::Int);
    fields1.insert("a".to_string(), Type::Bool);
    let rec1 = Type::Record(Row { fields: fields1 });

    let mut fields2 = HashMap::new();
    fields2.insert("y".to_string(), Type::Str);
    fields2.insert("b".to_string(), Type::Float);
    let rec2 = Type::Record(Row { fields: fields2 });

    assert!(
        !Type::types_are_disjoint(&rec1, &rec2),
        "Multi-field records {{x:Int,a:Bool}} and {{y:Str,b:Float}} should NOT be disjoint (conservative)"
    );
}

/// Task 4a: Literal promotion restricted to promotable classes
#[test]
fn test_promote_literal_restricted_to_promotable_classes() {
    let mut state = InferState::new();

    // Add a Numeric constraint (promotable)
    state.constraints.push(Constraint::Class {
        class: "Numeric".to_string(),
        vars: vec!["t0".to_string()],
        fundeps: vec![],
    });

    let promoted = promote_literal_for_constrained_var("t0", Type::IntLiteral(42), &state);

    match promoted {
        Type::Int => {} // Expected: Numeric is promotable
        other => panic!("Expected Int, got {:?}", other),
    }
}

/// Task 4b: Literal NOT promoted for non-promotable classes
#[test]
fn test_promote_literal_not_promoted_for_non_promotable_class() {
    let mut state = InferState::new();

    // Add a non-promotable constraint (e.g., custom class "MyClass")
    state.constraints.push(Constraint::Class {
        class: "MyClass".to_string(),
        vars: vec!["t0".to_string()],
        fundeps: vec![],
    });

    let result = promote_literal_for_constrained_var("t0", Type::IntLiteral(42), &state);

    match result {
        Type::IntLiteral(42) => {} // Expected: NOT promoted
        other => panic!("Expected IntLiteral(42), got {:?}", other),
    }
}

/// Task 4c: String literal promotion restricted to promotable classes
#[test]
fn test_promote_string_literal_restricted() {
    let mut state = InferState::new();

    // Comparable is promotable
    state.constraints.push(Constraint::Class {
        class: "Comparable".to_string(),
        vars: vec!["t0".to_string()],
        fundeps: vec![],
    });

    let promoted =
        promote_literal_for_constrained_var("t0", Type::StringLiteral("hello".to_string()), &state);

    match promoted {
        Type::Str => {} // Expected: Comparable is promotable
        other => panic!("Expected Str, got {:?}", other),
    }
}

/// Task 4d: Label-kinded TypeVars never promote
#[test]
fn test_promote_literal_label_kind_never_promotes() {
    let mut state = InferState::new();

    // Add Numeric constraint
    state.constraints.push(Constraint::Class {
        class: "Numeric".to_string(),
        vars: vec!["t0".to_string()],
        fundeps: vec![],
    });

    // Mark as Label kind
    state.kind_env.insert("t0".to_string(), Kind::Label);

    let result =
        promote_literal_for_constrained_var("t0", Type::StringLiteral("x".to_string()), &state);

    match result {
        Type::StringLiteral(s) if s == "x" => {} // Expected: Label kind prevents promotion
        other => panic!("Expected StringLiteral(\"x\"), got {:?}", other),
    }
}

/// chr-normalization: Occurs check for TypeStageApp args
///
/// unify(TypeVar("a"), TypeStageApp("F", [TypeVar("a")])) must fail with an
/// infinite-type error — TypeVar "a" occurs in its own binding via TypeStageApp.
/// This verifies lower_levels_check_occurs traverses TypeStageApp.args (line 1249-1255).
#[test]
fn test_unify_type_var_occurs_in_type_stage_app() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    // Register level for "a" so lower_levels_check_occurs can look it up
    state.levels.insert("a".to_string(), 0);

    let type_var_a = Type::TypeVar("a".to_string(), 0);
    let type_stage_app_f_a = Type::TypeStageApp {
        fn_name: "F".to_string(),
        args: vec![Type::TypeVar("a".to_string(), 0)],
    };

    // unify(a, F(a)) should fail: "a" occurs in F(a)
    let result = unify(
        &type_var_a,
        &type_stage_app_f_a,
        &mut subst,
        &mut state,
        span,
    );

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
