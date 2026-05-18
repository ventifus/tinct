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

// ============================================================================
// fn-narrowing-variadic sprint tests
// ============================================================================

/// Test that zero-param variadic unifies with concrete 1-param function
#[test]
fn test_unify_variadic_zero_with_concrete_arity() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    // Zero-param variadic: the "any function" type
    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    // Concrete 1-param function: Fn(Int) -> Bool
    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Bool),
        variadic: false,
    };

    // These should unify (zero-param variadic accepts any function)
    let result = unify(&any_function, &concrete_fn, &mut subst, &mut state, span);

    assert!(
        result.is_ok(),
        "Zero-param variadic should unify with concrete arity, got error: {:?}",
        result.unwrap_err()
    );
}

/// Test that zero-param variadic does NOT unify with 0-param non-variadic
#[test]
fn test_unify_variadic_zero_with_zero_non_variadic() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    // Zero-param variadic: Function{params:[], ret:Unknown, variadic:true}
    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    // Zero-param non-variadic: Fn() -> Int (a concrete 0-arity function)
    let concrete_fn = Type::Function {
        params: vec![],
        ret: Box::new(Type::Int),
        variadic: false,
    };

    // These should NOT unify (different semantics: one accepts args, one doesn't)
    let result = unify(&any_function, &concrete_fn, &mut subst, &mut state, span);

    assert!(
        result.is_err(),
        "Zero-param variadic should NOT unify with 0-param non-variadic (different signatures)"
    );
}

/// Test that zero-param variadic unifies with multi-param function
#[test]
fn test_unify_variadic_zero_with_multi_param() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    // Zero-param variadic: the "any function" type
    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    // Concrete 3-param function: Fn(Int, Str, Bool) -> Float
    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int), (None, Type::Str), (None, Type::Bool)],
        ret: Box::new(Type::Float),
        variadic: false,
    };

    // These should unify
    let result = unify(&any_function, &concrete_fn, &mut subst, &mut state, span);

    assert!(
        result.is_ok(),
        "Zero-param variadic should unify with multi-param function, got error: {:?}",
        result.unwrap_err()
    );
}

/// Test is_subtype: concrete function is subtype of zero-param variadic
#[test]
fn test_is_subtype_concrete_to_any_function() {
    // Concrete function: Fn(Int) -> Bool
    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Bool),
        variadic: false,
    };

    // Zero-param variadic: the "any function" type (supertype)
    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    // Concrete <: AnyFunction (concrete is more specific)
    assert!(
        Type::is_subtype(&concrete_fn, &any_function),
        "Concrete function should be subtype of any-function"
    );

    // AnyFunction is NOT a subtype of concrete (it's the other way around)
    assert!(
        !Type::is_subtype(&any_function, &concrete_fn),
        "Any-function should NOT be subtype of concrete function"
    );
}

/// Test is_subtype reflexivity: any-function is a subtype of any-function (τ <: τ).
/// Uses TWO DISTINCT objects (different allocations) to exercise the Function arm directly.
/// A same-reference test would short-circuit at `a == b => true` before entering the arm.
#[test]
fn test_is_subtype_any_function_reflexivity() {
    let any_fn1 = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };
    let any_fn2 = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    assert!(
        Type::is_subtype(&any_fn1, &any_fn2),
        "Any-function should be a subtype of any-function (reflexivity — distinct objects)"
    );
    assert!(
        Type::is_subtype(&any_fn2, &any_fn1),
        "Any-function subtyping should be symmetric (both directions)"
    );
}

/// Test that two distinct any-function values unify (zero-param variadic with zero-param variadic).
/// The unify path falls through to the standard equality check (both have params:[], variadic:true),
/// so this should succeed.
#[test]
fn test_unify_two_any_functions() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    let any_function_1 = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    let any_function_2 = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    let result = unify(
        &any_function_1,
        &any_function_2,
        &mut subst,
        &mut state,
        span,
    );

    assert!(
        result.is_ok(),
        "Two any-function types should unify, got error: {:?}",
        result.unwrap_err()
    );
}

/// Test symmetric unify: unify(concrete_fn, any_function) should succeed.
/// The sprint implementation has branches for both orders; this test verifies the
/// reverse direction (concrete_fn as first arg) is also covered.
#[test]
fn test_unify_concrete_fn_with_any_function_symmetric() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    // Concrete 1-param function: Fn(Int) -> Bool
    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Bool),
        variadic: false,
    };

    // Zero-param variadic: the "any function" type
    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    // Symmetric direction: concrete as first argument
    let result = unify(&concrete_fn, &any_function, &mut subst, &mut state, span);

    assert!(
        result.is_ok(),
        "unify(concrete_fn, any_function) should succeed (symmetric direction), got error: {:?}",
        result.unwrap_err()
    );
}

// ============================================================================
// fn-narrowing-followup sprint tests
// ============================================================================

/// Task 1: is_consistent should accept any-function with all function types
#[test]
fn test_is_consistent_any_function_with_concrete() {
    // Any-function type: Function{params:[], variadic:true}
    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    // Concrete function: Fn(Int) -> Bool
    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Bool),
        variadic: false,
    };

    // Any-function ~ Concrete function (should be consistent)
    assert!(
        Type::is_consistent(&any_function, &concrete_fn),
        "Any-function should be consistent with concrete function"
    );

    // Symmetric: Concrete ~ Any-function
    assert!(
        Type::is_consistent(&concrete_fn, &any_function),
        "Concrete function should be consistent with any-function (symmetric)"
    );
}

/// Task 1: is_consistent should accept any-function with multi-param functions
#[test]
fn test_is_consistent_any_function_with_multi_param() {
    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    let multi_param_fn = Type::Function {
        params: vec![
            (None, Type::Int),
            (None, Type::Str),
            (Some("x".to_string()), Type::Bool),
        ],
        ret: Box::new(Type::Float),
        variadic: false,
    };

    assert!(
        Type::is_consistent(&any_function, &multi_param_fn),
        "Any-function should be consistent with multi-param function"
    );
}

/// Task 1: is_consistent should accept any-function with zero-param non-variadic
#[test]
fn test_is_consistent_any_function_with_zero_param_non_variadic() {
    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    let zero_param_fn = Type::Function {
        params: vec![],
        ret: Box::new(Type::Int),
        variadic: false,
    };

    // Under consistency, these ARE consistent (gradual typing allows it)
    assert!(
        Type::is_consistent(&any_function, &zero_param_fn),
        "Any-function should be consistent with zero-param non-variadic"
    );
}

/// Task 2: types_are_disjoint for Function vs Int
#[test]
fn test_types_are_disjoint_function_vs_int() {
    let fn_ty = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Bool),
        variadic: false,
    };

    // Function vs Int (both directions)
    assert!(
        Type::types_are_disjoint(&fn_ty, &Type::Int),
        "Function should be disjoint from Int"
    );
    assert!(
        Type::types_are_disjoint(&Type::Int, &fn_ty),
        "Int should be disjoint from Function (symmetric)"
    );
}

/// Task 2: types_are_disjoint for Function vs all primitive types
#[test]
fn test_types_are_disjoint_function_vs_primitives() {
    let fn_ty = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    // Function is disjoint from all primitives
    assert!(Type::types_are_disjoint(&fn_ty, &Type::Int));
    assert!(Type::types_are_disjoint(&fn_ty, &Type::Float));
    assert!(Type::types_are_disjoint(&fn_ty, &Type::Str));
    assert!(Type::types_are_disjoint(&fn_ty, &Type::Bool));
    assert!(Type::types_are_disjoint(&fn_ty, &Type::Bytes));

    // Symmetric
    assert!(Type::types_are_disjoint(&Type::Int, &fn_ty));
    assert!(Type::types_are_disjoint(&Type::Float, &fn_ty));
    assert!(Type::types_are_disjoint(&Type::Str, &fn_ty));
    assert!(Type::types_are_disjoint(&Type::Bool, &fn_ty));
    assert!(Type::types_are_disjoint(&Type::Bytes, &fn_ty));
}

/// Task 2: types_are_disjoint for Function vs literal types
#[test]
fn test_types_are_disjoint_function_vs_literals() {
    let fn_ty = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Str),
        variadic: false,
    };

    // Function vs IntLiteral
    assert!(Type::types_are_disjoint(&fn_ty, &Type::IntLiteral(42)));
    assert!(Type::types_are_disjoint(&Type::IntLiteral(42), &fn_ty));

    // Function vs StringLiteral
    assert!(Type::types_are_disjoint(
        &fn_ty,
        &Type::StringLiteral("hello".to_string())
    ));
    assert!(Type::types_are_disjoint(
        &Type::StringLiteral("hello".to_string()),
        &fn_ty
    ));
}

/// Task 2: types_are_disjoint for Function vs Record
#[test]
fn test_types_are_disjoint_function_vs_record() {
    let fn_ty = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Bool),
        variadic: false,
    };

    let mut fields = HashMap::new();
    fields.insert("x".to_string(), Type::Int);
    let record_ty = Type::Record(Row { fields });

    // Function vs Record (both directions)
    assert!(
        Type::types_are_disjoint(&fn_ty, &record_ty),
        "Function should be disjoint from Record"
    );
    assert!(
        Type::types_are_disjoint(&record_ty, &fn_ty),
        "Record should be disjoint from Function (symmetric)"
    );
}

/// Task 2: types_are_disjoint for Function vs Seq
#[test]
fn test_types_are_disjoint_function_vs_seq() {
    let fn_ty = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    let seq_ty = Type::Seq(Box::new(Type::Int));

    // Function vs Seq (both directions)
    assert!(
        Type::types_are_disjoint(&fn_ty, &seq_ty),
        "Function should be disjoint from Seq"
    );
    assert!(
        Type::types_are_disjoint(&seq_ty, &fn_ty),
        "Seq should be disjoint from Function (symmetric)"
    );
}

/// Task 2: types_are_disjoint for Function vs Map
#[test]
fn test_types_are_disjoint_function_vs_map() {
    let fn_ty = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Str),
        variadic: false,
    };

    let map_ty = Type::Map(Box::new(Type::Str), Box::new(Type::Int));

    // Function vs Map (both directions)
    assert!(
        Type::types_are_disjoint(&fn_ty, &map_ty),
        "Function should be disjoint from Map"
    );
    assert!(
        Type::types_are_disjoint(&map_ty, &fn_ty),
        "Map should be disjoint from Function (symmetric)"
    );
}

/// Task 2: Two different function types are NOT disjoint (conservative)
#[test]
fn test_types_are_not_disjoint_function_vs_function() {
    let fn1 = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Bool),
        variadic: false,
    };

    let fn2 = Type::Function {
        params: vec![(None, Type::Str)],
        ret: Box::new(Type::Float),
        variadic: false,
    };

    // Two different functions are NOT disjoint (conservative - they're both functions)
    assert!(
        !Type::types_are_disjoint(&fn1, &fn2),
        "Different function types should NOT be disjoint (conservative)"
    );
}
