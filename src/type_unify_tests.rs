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
    // Numeric class is already registered in InferState::new()
    let numeric_class = state.class_env.get("Numeric").unwrap();
    state.constraints.push(Constraint::Class {
        class: std::sync::Arc::new(numeric_class.clone()),
        vars: vec!["t0".to_string()],
        origin_name: None,
        origin_span: None,
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
    // Create a dummy ClassDecl for testing
    use crate::types::{ClassDecl, Kind};
    let my_class = std::sync::Arc::new(ClassDecl {
        name: "MyClass".to_string(),
        params: vec![("a".to_string(), Kind::Type)],
        superclasses: vec![],
        determines: vec![],
        resolver: None,
        resolver_injective: false,
    });
    state.constraints.push(Constraint::Class {
        class: my_class,
        vars: vec!["t0".to_string()],
        origin_name: None,
        origin_span: None,
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

    // Comparable is promotable and already registered in InferState::new()
    let comparable_class = state.class_env.get("Comparable").unwrap();
    state.constraints.push(Constraint::Class {
        class: std::sync::Arc::new(comparable_class.clone()),
        vars: vec!["t0".to_string()],
        origin_name: None,
        origin_span: None,
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

    // Add Numeric constraint (already registered in InferState::new())
    let numeric_class = state.class_env.get("Numeric").unwrap();
    state.constraints.push(Constraint::Class {
        class: std::sync::Arc::new(numeric_class.clone()),
        vars: vec!["t0".to_string()],
        origin_name: None,
        origin_span: None,
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

// ============================================================================
// type-soundness sprint tests
// ============================================================================

/// Union-vs-Union deferral: when both Unions contain inference vars, the equality
/// is deferred (not hard-errored) and pushed to state.deferred_equalities.
/// This covers the arm at type_unify.rs lines 1998-2004.
#[test]
fn test_union_vs_union_with_typevars_defers() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    // Register levels for the type vars
    state.levels.insert("a".to_string(), 0);
    state.levels.insert("b".to_string(), 0);

    // Union([Int, TypeVar(a)]) ~ Union([Str, TypeVar(b)])
    let lhs = Type::Union(vec![Type::Int, Type::TypeVar("a".to_string(), 0)]);
    let rhs = Type::Union(vec![Type::Str, Type::TypeVar("b".to_string(), 0)]);

    let result = unify(&lhs, &rhs, &mut subst, &mut state, span);

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
#[test]
fn test_union_vs_union_concrete_no_deferral() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    // Union([Int]) ~ Union([Int]) — concrete, no TypeVars
    let lhs = Type::Union(vec![Type::Int]);
    let rhs = Type::Union(vec![Type::Int]);

    // This falls through to the generic _ => Err arm (no C-Var1 match either),
    // not the deferral arm. Deferred_equalities should remain empty.
    let _ = unify(&lhs, &rhs, &mut subst, &mut state, span);

    assert_eq!(
        state.deferred_equalities.len(),
        0,
        "Concrete Union-vs-Union should NOT push a deferred equality"
    );
}

/// chr-normalization: Occurs check for TypeStageApp args
#[test]
fn test_unify_type_var_occurs_in_type_stage_app() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    state.levels.insert("a".to_string(), 0);

    let type_var_a = Type::TypeVar("a".to_string(), 0);
    let type_stage_app_f_a = Type::TypeStageApp {
        fn_name: "F".to_string(),
        args: vec![Type::TypeVar("a".to_string(), 0)],
    };

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

#[test]
fn test_unify_variadic_zero_with_concrete_arity() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Bool),
        variadic: false,
    };

    let result = unify(&any_function, &concrete_fn, &mut subst, &mut state, span);

    assert!(
        result.is_ok(),
        "Zero-param variadic should unify with concrete arity, got error: {:?}",
        result.unwrap_err()
    );
}

#[test]
fn test_unify_variadic_zero_with_zero_non_variadic() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    let concrete_fn = Type::Function {
        params: vec![],
        ret: Box::new(Type::Int),
        variadic: false,
    };

    let result = unify(&any_function, &concrete_fn, &mut subst, &mut state, span);

    assert!(
        result.is_err(),
        "Zero-param variadic should NOT unify with 0-param non-variadic"
    );
}

#[test]
fn test_unify_variadic_zero_with_multi_param() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int), (None, Type::Str), (None, Type::Bool)],
        ret: Box::new(Type::Float),
        variadic: false,
    };

    let result = unify(&any_function, &concrete_fn, &mut subst, &mut state, span);

    assert!(
        result.is_ok(),
        "Zero-param variadic should unify with multi-param function, got error: {:?}",
        result.unwrap_err()
    );
}

#[test]
fn test_is_subtype_concrete_to_any_function() {
    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Bool),
        variadic: false,
    };

    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    assert!(
        Type::is_subtype(&concrete_fn, &any_function),
        "Concrete function should be subtype of any-function"
    );

    assert!(
        !Type::is_subtype(&any_function, &concrete_fn),
        "Any-function should NOT be subtype of concrete function"
    );
}

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

#[test]
fn test_unify_concrete_fn_with_any_function_symmetric() {
    let mut state = InferState::new();
    let mut subst = Substitution::new();
    let span = Span::origin();

    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Bool),
        variadic: false,
    };

    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

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

#[test]
fn test_is_consistent_any_function_with_concrete() {
    let any_function = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    let concrete_fn = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Bool),
        variadic: false,
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

    assert!(
        Type::is_consistent(&any_function, &zero_param_fn),
        "Any-function should be consistent with zero-param non-variadic"
    );
}

#[test]
fn test_types_are_disjoint_function_vs_int() {
    let fn_ty = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Bool),
        variadic: false,
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

#[test]
fn test_types_are_disjoint_function_vs_primitives() {
    let fn_ty = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    assert!(Type::types_are_disjoint(&fn_ty, &Type::Int));
    assert!(Type::types_are_disjoint(&fn_ty, &Type::Float));
    assert!(Type::types_are_disjoint(&fn_ty, &Type::Str));
    assert!(Type::types_are_disjoint(&fn_ty, &Type::Bool));
    assert!(Type::types_are_disjoint(&fn_ty, &Type::Bytes));

    assert!(Type::types_are_disjoint(&Type::Int, &fn_ty));
    assert!(Type::types_are_disjoint(&Type::Float, &fn_ty));
    assert!(Type::types_are_disjoint(&Type::Str, &fn_ty));
    assert!(Type::types_are_disjoint(&Type::Bool, &fn_ty));
    assert!(Type::types_are_disjoint(&Type::Bytes, &fn_ty));
}

#[test]
fn test_types_are_disjoint_function_vs_literals() {
    let fn_ty = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Str),
        variadic: false,
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

    assert!(
        Type::types_are_disjoint(&fn_ty, &record_ty),
        "Function should be disjoint from Record"
    );
    assert!(
        Type::types_are_disjoint(&record_ty, &fn_ty),
        "Record should be disjoint from Function (symmetric)"
    );
}

#[test]
fn test_types_are_disjoint_function_vs_seq() {
    let fn_ty = Type::Function {
        params: vec![],
        ret: Box::new(Type::Unknown),
        variadic: true,
    };

    let seq_ty = Type::Seq(Box::new(Type::Int));

    assert!(
        Type::types_are_disjoint(&fn_ty, &seq_ty),
        "Function should be disjoint from Seq"
    );
    assert!(
        Type::types_are_disjoint(&seq_ty, &fn_ty),
        "Seq should be disjoint from Function (symmetric)"
    );
}

#[test]
fn test_types_are_disjoint_function_vs_map() {
    let fn_ty = Type::Function {
        params: vec![(None, Type::Int)],
        ret: Box::new(Type::Str),
        variadic: false,
    };

    let map_ty = Type::Map(Box::new(Type::Str), Box::new(Type::Int));

    assert!(
        Type::types_are_disjoint(&fn_ty, &map_ty),
        "Function should be disjoint from Map"
    );
    assert!(
        Type::types_are_disjoint(&map_ty, &fn_ty),
        "Map should be disjoint from Function (symmetric)"
    );
}

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

    assert!(
        !Type::types_are_disjoint(&fn1, &fn2),
        "Different function types should NOT be disjoint (conservative)"
    );
}

/// Handle capability PartialEq limitation: structural equality fails when capability rows
/// contain TypeVars with different names, even if they are unifiable.
/// Known-safe: unify() drives type checking, PartialEq only affects HashMap lookups
/// (false negatives are conservative).
#[test]
fn test_handle_capability_partialeq_limitation() {
    // Create two Handle types with different TypeVar names
    let handle_a = Type::Handle(Box::new(Type::TypeVar("a".to_string(), 0)));
    let handle_b = Type::Handle(Box::new(Type::TypeVar("b".to_string(), 0)));

    // PartialEq will return false (structural inequality)
    assert_ne!(
        handle_a, handle_b,
        "Handle types with different TypeVar names are not structurally equal"
    );

    // However, they should unify successfully
    let mut state = InferState::new();
    let mut subst = Substitution::new();

    let result = unify(&handle_a, &handle_b, &mut subst, &mut state, Span::origin());

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
#[test]
fn test_reverse_fd_back_propagates_determining_type() {
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
    });

    // Register the class in class_env.
    state.class_env.insert(ClassDecl {
        name: "MySeq".to_string(),
        params: vec![("t".to_string(), Kind::Type), ("s".to_string(), Kind::Type)],
        superclasses: vec![],
        determines: vec![(vec![0], vec![1])],
        resolver: None,
        resolver_injective: true,
    });

    // Register instance: MySeq Int Str
    // MPTC instances are encoded as Record with numbered fields.
    let mut instance_fields = HashMap::new();
    instance_fields.insert("0".to_string(), Type::Int); // pos 0 = Int (determining)
    instance_fields.insert("1".to_string(), Type::Str); // pos 1 = Str (determined)
    let instance_type = Type::Record(Row {
        fields: instance_fields,
    });
    let inst = InstanceDecl {
        class_name: "MySeq".to_string(),
        instance_type,
        det_positions: vec![0], // determining position indices
        method_types: HashMap::new(),
    };
    state.instance_env.insert(inst).unwrap();

    // Create type variables t0 (determining, pos 0) and t1 (determined, pos 1).
    state.levels.insert("t0".to_string(), 0);
    state.levels.insert("t1".to_string(), 0);

    // Add the constraint: MySeq [t0, t1]
    state.constraints.push(Constraint::Class {
        class: my_class,
        vars: vec!["t0".to_string(), "t1".to_string()],
        origin_name: None,
        origin_span: None,
    });

    // Unify t1 (determined position) with Str.
    // This should trigger the reverse FD and back-propagate t0 = Int.
    let mut subst = Substitution::new();
    let t1 = Type::TypeVar("t1".to_string(), 0);
    let result = unify(&t1, &Type::Str, &mut subst, &mut state, Span::origin());

    assert!(
        result.is_ok(),
        "Unifying determined var with concrete type should succeed: {:?}",
        result.unwrap_err()
    );

    // Check that t0 was back-propagated to Int via reverse FD.
    let t0_bound = state.subst.apply(&Type::TypeVar("t0".to_string(), 0));
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
#[test]
fn test_reverse_fd_does_not_fire_when_not_injective() {
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
    });

    state.class_env.insert(ClassDecl {
        name: "MyNonInj".to_string(),
        params: vec![("t".to_string(), Kind::Type), ("s".to_string(), Kind::Type)],
        superclasses: vec![],
        determines: vec![(vec![0], vec![1])],
        resolver: None,
        resolver_injective: false,
    });

    // Register instance: MyNonInj Int Str
    let mut instance_fields = HashMap::new();
    instance_fields.insert("0".to_string(), Type::Int);
    instance_fields.insert("1".to_string(), Type::Str);
    let instance_type = Type::Record(Row {
        fields: instance_fields,
    });
    let inst = InstanceDecl {
        class_name: "MyNonInj".to_string(),
        instance_type,
        det_positions: vec![0],
        method_types: HashMap::new(),
    };
    state.instance_env.insert(inst).unwrap();

    state.levels.insert("t0".to_string(), 0);
    state.levels.insert("t1".to_string(), 0);

    state.constraints.push(Constraint::Class {
        class: my_class,
        vars: vec!["t0".to_string(), "t1".to_string()],
        origin_name: None,
        origin_span: None,
    });

    // Unify t1 with Str — should NOT back-propagate t0.
    let mut subst = Substitution::new();
    let t1 = Type::TypeVar("t1".to_string(), 0);
    let result = unify(&t1, &Type::Str, &mut subst, &mut state, Span::origin());

    assert!(
        result.is_ok(),
        "Unification should succeed: {:?}",
        result.unwrap_err()
    );

    // t0 must remain unbound (no reverse FD fired).
    let t0_bound = state.subst.apply(&Type::TypeVar("t0".to_string(), 0));
    assert!(
        matches!(t0_bound, Type::TypeVar(ref n, _) if n == "t0"),
        "With non-injective resolver, t0 must remain unbound, but got: {:?}",
        t0_bound
    );
}
