//! Unification, constraint solving, and substitution application for Hindley-Milner
//! polymorphism with Boolean-Algebraic Subtyping (BAS) and structural record types.
//!
//! Type variable bindings are stored in `InferState.type_vars` (an `IndexMap<String, TypeVarEntry>`).
//! The old `Substitution` struct has been removed; all binding operations go through
//! `InferState.bind_type_var()` and lookups through `InferState.type_vars.get()`.

use indexmap::IndexMap;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

use crate::ast::Span;
use crate::type_def::substitute_recvar;
use crate::type_errors::{GenericTypeError, TypeErrorTyped, UnificationFailure};
use crate::type_infer::TypeVarEntry;

use super::*;

/// Maximum recursion depth for constraint satisfaction checking.
/// Prevents infinite loops when checking constraints on recursive types.
const MAX_CONSTRAINT_DEPTH: usize = 256;

/// Check if a type satisfies a type class constraint.
/// Returns true if the type is an instance of the class.
///
/// This function handles structural meta-rules for the type lattice only:
///
/// 1. **Gradual/lattice meta-rules**: Unknown satisfies all constraints vacuously
///    (AGT existential lifting); Never vacuously (uninhabited).
///
/// 2. **Union/Intersection**: A compound type satisfies a constraint iff all of its
///    members do (both branches must be safe for union-typed runtime values).
///
/// All class-specific membership (primitive types, records, nominals, maps) is handled
/// by `InstanceEnv::resolve_instance` in `check_constraints_on_var` — driven by instance
/// declarations in prelude. This function returns `false` for all concrete types so the
/// caller falls through to instance resolution.
///
/// This keeps `satisfies_constraint_inner` free of class name strings.
pub fn satisfies_constraint(ty: &Type, class_name: &str) -> bool {
    satisfies_constraint_inner(ty, class_name, 0)
}

/// Internal implementation of constraint satisfaction with depth tracking.
/// Conservative: returns false if depth limit exceeded (treat as constraint not satisfied).
fn satisfies_constraint_inner(ty: &Type, class_name: &str, depth: usize) -> bool {
    // Depth guard: prevent unbounded recursion on pathological recursive types
    if depth >= MAX_CONSTRAINT_DEPTH {
        return false;
    }

    // Unknown (the gradual dynamic type ?) satisfies all constraints vacuously.
    // AGT existential lifting: C(?) = ∃t ∈ γ(?). C(t) holds for any non-empty
    // class because γ(?) = STypes and every class has at least one instance
    // (Garcia, Clark & Tanter, POPL 2016). The runtime ClassEnv dispatch provides
    // the actual check at the gradual boundary.
    if matches!(ty, Type::Unknown) {
        return true;
    }

    // [CONSTRAIN-NEVER]: C(⊥) ⊢ satisfied (vacuously — Never is uninhabited)
    if matches!(ty, Type::Never) {
        return true;
    }

    // [CONSTRAIN-UNION]: C(τ₁ | τ₂) ⊢ satisfied iff C(τ₁) ∧ C(τ₂) (ALL members).
    // A union-typed value could be either alternative at runtime, so both branches must
    // satisfy the constraint. Use all(), NOT any().
    if let Type::Union(members) = ty {
        return members
            .iter()
            .all(|member| satisfies_constraint_inner(member, class_name, depth + 1));
    }

    // [CONSTRAIN-INTER]: C(τ₁ & τ₂) ⊢ satisfied iff C(τ₁) ∧ C(τ₂) (ALL members).
    if let Type::Intersection(members) = ty {
        return members
            .iter()
            .all(|member| satisfies_constraint_inner(member, class_name, depth + 1));
    }

    // All concrete types (primitives, records, nominals, maps) are handled by
    // InstanceEnv::resolve_instance in check_constraints_on_var. Return false so the
    // caller falls through to instance resolution.
    false
}

/// Check if a constraint is entailed by a context (set of constraints).
/// Returns true if the target constraint is directly present in the context,
/// or if it is implied via superclass relationships.
///
/// For example, if the context contains `Comparable a`, then `Equatable a` is
/// entailed because Comparable has Equatable as a superclass.
///
/// This implements superclass entailment for constraint simplification during
/// let-generalization. See Jones (1992) "Type Classes: Exploring the Design Space".
pub fn entails(class_env: &ClassEnv, context: &[Constraint], target: &Constraint) -> bool {
    // Direct check: is target directly in context?
    if context.contains(target) {
        return true;
    }

    // Superclass check: is there a constraint C in context such that
    // C has target.class as a superclass (transitively)?
    // Only applicable to single-param Class constraints (MPTC entailment is future work)
    if let Constraint::Class {
        class: target_class,
        vars: target_vars,
        ..
    } = target
    {
        if target_vars.len() == 1 {
            // Only compare Var positions — superclass entailment requires the same type variable.
            // Ground positions are already resolved and don't participate in entailment chains.
            if let Some(target_var_name) = target_vars[0].as_var() {
                for constraint in context {
                    if let Constraint::Class { class, vars, .. } = constraint {
                        if vars.len() == 1
                            && vars[0].as_var() == Some(target_var_name)
                            && is_superclass_of(class_env, &class.name, &target_class.name)
                        {
                            return true;
                        }
                    }
                }
            }
        }
    }

    // HasField entailment: HasField with same label and dict_var entails HasField
    // (field_var can differ as it's an existential output)
    if let Constraint::HasField {
        label: target_label,
        dict_var: target_dict,
        ..
    } = target
    {
        for constraint in context {
            if let Constraint::HasField {
                label, dict_var, ..
            } = constraint
            {
                if label == target_label && dict_var == target_dict {
                    return true;
                }
            }
        }
    }

    false
}

/// Check if `subclass` has `superclass` as a superclass (transitively).
///
/// For example:
/// - `is_superclass_of(env, "Numeric", "Equatable")` returns true because
///   Numeric has Equatable in its superclass list.
/// - `is_superclass_of(env, "Equatable", "Numeric")` returns false (wrong direction).
///
/// This computes the transitive closure of the superclass relation.
fn is_superclass_of(class_env: &ClassEnv, subclass: &str, superclass: &str) -> bool {
    let mut visited = HashSet::new();
    is_superclass_of_impl(class_env, subclass, superclass, &mut visited)
}

/// Implementation of is_superclass_of with cycle detection.
fn is_superclass_of_impl(
    class_env: &ClassEnv,
    subclass: &str,
    superclass: &str,
    visited: &mut HashSet<String>,
) -> bool {
    // If they're the same, trivially true
    if subclass == superclass {
        return true;
    }

    // Cycle detection: if we've already visited this class, stop
    if !visited.insert(subclass.to_string()) {
        return false;
    }

    // Get the subclass declaration
    let Some(subclass_decl) = class_env.get(subclass) else {
        return false;
    };

    // Check direct superclasses (now tuples of (class_name, param_name))
    if subclass_decl
        .superclasses
        .iter()
        .any(|(class_name, _param)| class_name == superclass)
    {
        return true;
    }

    // Check transitive superclasses (recursively)
    for (direct_super, _param) in &subclass_decl.superclasses {
        if is_superclass_of_impl(class_env, direct_super, superclass, visited) {
            return true;
        }
    }

    false
}

/// Check all constraints on a type variable when it gets bound to a concrete type.
/// Returns an error if any constraint is violated.
///
/// This function performs two checks:
/// 1. For concrete types (Int, Str, etc.), check against the fixed instance sets
///    using `satisfies_constraint`.
/// 2. For type constructors and user-defined types, attempt instance resolution
///    using `InstanceEnv::resolve_instance`.
///
/// If no instance is found and the type is not in the fixed instance set,
/// a type error is returned.
///
/// Instance resolution recursion depth is tracked in `state.instance_resolution_depth` to
/// prevent infinite loops through the cycle:
///   check_constraints_on_var → resolve_instance → unify → check_constraints_on_var
///
/// The depth counter accumulates across recursive calls — each entry into resolve_instance
/// increments, each exit decrements. Because increments and decrements are matched, the counter
/// naturally returns to 0 after each independent constraint resolution chain completes. This
/// matches GHC's -freduction-depth semantics (Sulzmann et al. 2007 §3.2).
async fn check_constraints_on_var(
    var_name: &str,
    concrete_ty: &Type,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) -> Result<(), TypeError> {
    // Collect only the constraints that apply to var_name (immutable scan first).
    // This avoids cloning the entire Vec<Constraint> — we clone only the constraints
    // that match, which is typically 0–2 per variable binding even in constraint-heavy
    // programs.
    #[derive(Clone)]
    enum ApplicableConstraint {
        SingleParam {
            class: String,
            structural_discharge: crate::type_class::StructuralDischarge,
        },
        MultiParam {
            class: String,
            /// Constraint arguments — Var positions hold variable names; Ground positions
            /// hold concrete types that were resolved before generalization (B-398).
            args: Vec<ConstraintArg>,
            fundeps: Vec<(Vec<usize>, Vec<usize>)>,
            resolver_injective: bool,
        },
        /// HasField constraint on this var as the dict_var.
        /// Fired when the dict TypeVar is bound to a concrete type.
        HasField {
            label: crate::type_def::Label,
            field_var: String,
        },
    }

    let applicable: Vec<ApplicableConstraint> = constraints
        .iter()
        .filter_map(|c| match c {
            Constraint::Class { class, vars, .. }
                if vars.len() == 1 && vars[0].as_var() == Some(var_name) =>
            {
                Some(ApplicableConstraint::SingleParam {
                    class: class.name.clone(),
                    structural_discharge: class.structural_discharge.clone(),
                })
            }
            Constraint::Class { class, vars, .. }
                if vars.len() > 1 && vars.iter().any(|v| v.as_var() == Some(var_name)) =>
            {
                Some(ApplicableConstraint::MultiParam {
                    class: class.name.clone(),
                    args: vars.clone(),
                    fundeps: class.determines.clone(),
                    resolver_injective: class.resolver_injective,
                })
            }
            Constraint::HasField {
                label,
                dict_var,
                field_var,
            } if dict_var.as_str() == var_name => Some(ApplicableConstraint::HasField {
                label: label.clone(),
                field_var: field_var.clone(),
            }),
            _ => None,
        })
        .collect();

    for constraint in applicable {
        match constraint {
            ApplicableConstraint::HasField { label, field_var } => {
                // HasField dict_var is now bound to concrete_ty — resolve the field type.
                // resolve_has_field is sync; it looks up label in concrete_ty and returns the field type.
                // On success, unify field_var TypeVar with the found type.
                // On error (field absent from closed record, type not a dict), propagate.
                match resolve_has_field(&label, concrete_ty, state, span.clone(), 0) {
                    Ok(field_ty) => {
                        // Found the field — unify the field TypeVar with the resolved type.
                        let field_var_ty = Type::TypeVar(field_var.clone(), state.level);
                        let mut sub_constraints = Vec::new();
                        if let Err(e) = Box::pin(unify(
                            &field_var_ty,
                            &field_ty,
                            state,
                            &mut sub_constraints,
                            span.clone(),
                        ))
                        .await
                        {
                            return Err(e);
                        }
                        constraints.extend(sub_constraints);
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
            ApplicableConstraint::SingleParam {
                class,
                structural_discharge,
            } => {
                // Structural discharge: typeclass declared with a StructuralDischarge rule
                // is satisfied by structural inspection rather than instance lookup.
                // This is a general mechanism — no class names hardcoded here.
                use crate::type_class::StructuralDischarge;
                match &structural_discharge {
                    StructuralDischarge::ClosedDict => {
                        match concrete_ty {
                            Type::Dict(crate::type_def::Row {
                                tail: crate::type_def::RowTail::Empty,
                                ..
                            }) => {
                                // Closed dict — constraint satisfied.
                                continue;
                            }
                            Type::Dict(crate::type_def::Row {
                                tail: crate::type_def::RowTail::Uniform { .. },
                                ..
                            }) => {
                                // Open dict — constraint violated.
                                return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                                    message: format!(
                                        "open dict (Dict) does not satisfy Record — Record requires a closed dict with known fields; use @Dict to accept any dict"
                                    ),
                                    span: span.clone(),
                                    notes: vec![],
                                    call_stack: vec![],
                                })));
                            }
                            _ => {
                                // Non-dict type — fall through to normal constraint checking.
                            }
                        }
                    }
                    StructuralDischarge::None => {}
                }

                // Single-parameter type class constraint (e.g., Numeric a)
                // First, check via satisfies_constraint (lattice meta-rules: Unknown, Never,
                // Union, Intersection). Returns false for all concrete types — those are
                // handled by InstanceEnv below.
                if satisfies_constraint(concrete_ty, &class) {
                    continue;
                }

                // satisfies_constraint returned false: try instance resolution for all
                // concrete types (primitives, records, nominals) and user-defined instances.
                // This enables user-defined instances (future work: dictionary construction)
                // Build a temporary InstanceEnv snapshot to avoid borrow checker conflict:
                // resolve_instance takes &self on InstanceEnv AND &mut state simultaneously.
                const MAX_INSTANCE_RESOLUTION_DEPTH: u32 = 64;
                if state.instance_resolution_depth >= MAX_INSTANCE_RESOLUTION_DEPTH {
                    // Too deep — return a type error instead of silently skipping.
                    // The recursion cycle is: check_constraints_on_var → resolve_instance →
                    // unify → check_constraints_on_var. This matches GHC's -freduction-depth
                    // semantics (Sulzmann et al. 2007 §3.2).
                    return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                        message: format!(
                            "instance resolution depth limit exceeded (max {}) — possible recursive instance definitions for constraint {}",
                            MAX_INSTANCE_RESOLUTION_DEPTH,
                            class
                        ),
                        span: span.clone(),
                        notes: vec![], call_stack: vec![],
                    })));
                }
                state.instance_resolution_depth += 1;
                let inst_env = state.build_instance_env_snapshot();
                let resolve_result =
                    Box::pin(inst_env.resolve_instance(&class, concrete_ty, state)).await;
                state.instance_resolution_depth -= 1;

                match resolve_result {
                    Ok(Some(_)) => {
                        // Instance found - constraint satisfied
                        continue;
                    }
                    Ok(None) => {
                        // No instance found — try widening literal types (IntLiteral → Int,
                        // StringLiteral → Str) and retry. This handles the case where a
                        // literal type doesn't have a direct instance but its parent type does.
                        let widened = crate::typecheck::typecheck_call::widen_literal_types(
                            concrete_ty.clone(),
                        );
                        if widened != *concrete_ty {
                            // Widened type differs — retry with widened type
                            if satisfies_constraint(&widened, &class) {
                                continue;
                            }
                            let inst_env2 = state.build_instance_env_snapshot();
                            state.instance_resolution_depth += 1;
                            let retry_result =
                                Box::pin(inst_env2.resolve_instance(&class, &widened, state)).await;
                            state.instance_resolution_depth -= 1;
                            match retry_result {
                                Ok(Some(_)) => continue,
                                _ => {}
                            }
                        }
                        // No instance found even after widening - constraint violated
                        return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                            message: format!(
                                "type {} does not satisfy constraint {}",
                                concrete_ty, class
                            ),
                            span: span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        })));
                    }
                    Err(ambig_msg) => {
                        // Ambiguous instances — equally specific matches, coherence violation
                        return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                            message: ambig_msg,
                            span: span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        })));
                    }
                }
            }
            ApplicableConstraint::MultiParam {
                class,
                args,
                fundeps,
                resolver_injective,
            } => {
                // Multi-parameter type class constraint with functional dependencies.
                // Check if this variable binding triggers FD improvement (forward or reverse).
                //
                // Mark var_name as in-progress before calling FD improvement. This entry in
                // fd_in_progress prevents FD improvement from re-binding var_name in an inner
                // call (idempotency guard). Specifically, when a reverse FD fires for var_name
                // and propagates a value to a determining variable, the forward FD for that
                // determining variable would try to re-bind var_name — but the in-progress
                // guard causes it to skip that re-binding. Without this guard, the cycle
                // reverse → forward → reverse → … hits MAX_FD_DEPTH (16).
                let was_inserted = state.fd_in_progress.insert(var_name.to_string());
                let fd_result = improve_functional_dependency(
                    &class,
                    &args,
                    &fundeps,
                    resolver_injective,
                    var_name,
                    concrete_ty,
                    state,
                    constraints,
                    span.clone(),
                )
                .await;
                if was_inserted {
                    state.fd_in_progress.remove(var_name);
                }
                fd_result?;
            }
        }
    }
    Ok(())
}

/// Functional dependency improvement for multi-parameter type classes (Jones 2000).
///
/// When a type variable α in a determining position of a functional dependency becomes ground,
/// and ALL determining positions are now ground, look up the matching instance and unify
/// the determined positions with the instance's result types.
///
/// For Add a b c with FD (a,b) → c: when both a and b are ground, resolve c from the instance table.
/// Depth limit for functional dependency improvement recursion.
/// Prevents infinite loops through the improve_functional_dependency → unify →
/// check_constraints_on_var → improve_functional_dependency cycle.
const MAX_FD_DEPTH: usize = 16;

#[allow(clippy::too_many_arguments)] // FD improvement requires all constraint components
async fn improve_functional_dependency(
    class: &str,
    args: &[ConstraintArg],
    fundeps: &[(Vec<usize>, Vec<usize>)],
    resolver_injective: bool,
    bound_var: &str,
    bound_type: &Type,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) -> Result<(), TypeError> {
    // Depth guard: prevent infinite recursion through the FD improvement cycle.
    if state.fd_depth >= MAX_FD_DEPTH {
        // F7 FIX: Return error instead of silently succeeding when depth limit is reached
        return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
            message: format!(
                "functional dependency improvement depth limit exceeded (max {}) — possible recursive FD chain for class {}",
                MAX_FD_DEPTH, class
            ),
            span,
            notes: vec![], call_stack: vec![],
        })));
    }
    state.fd_depth += 1;
    let result = improve_functional_dependency_inner(
        class,
        args,
        fundeps,
        resolver_injective,
        bound_var,
        bound_type,
        state,
        constraints,
        span,
    )
    .await;
    state.fd_depth -= 1;
    result
}

#[allow(clippy::too_many_arguments)] // FD improvement requires all constraint components
async fn improve_functional_dependency_inner(
    class: &str,
    args: &[ConstraintArg],
    fundeps: &[(Vec<usize>, Vec<usize>)],
    resolver_injective: bool,
    bound_var: &str,
    bound_type: &Type,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) -> Result<(), TypeError> {
    // For each functional dependency (determining → determined)
    for (det_positions, ded_positions) in fundeps {
        // Compute the positions of bound_var in the constraint arg list.
        // Only Var positions can match bound_var; Ground positions are already resolved.
        let bound_var_positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, arg)| arg.as_var() == Some(bound_var))
            .map(|(i, _)| i)
            .collect();

        let bound_in_determining = bound_var_positions
            .iter()
            .any(|p| det_positions.contains(p));
        let bound_in_determined = bound_var_positions
            .iter()
            .any(|p| ded_positions.contains(p));

        // ── Reverse FD improvement ────────────────────────────────────────────
        // When the resolver is injective AND the bound variable is in a determined
        // position (not a determining position), we can back-propagate:
        // knowing the output of the resolver tells us what the inputs must be.
        //
        // Algorithm:
        // 1. Collect current types for all determined positions (using bound_type
        //    for the in-flight binding, state for already-bound vars).
        // 2. If all determined positions are ground, scan the InstanceEnv for the
        //    instance whose determined-position type unifies with the bound types.
        // 3. Extract the determining-position types from that instance and unify
        //    them with the determining-position constraint vars.
        //
        // This is only attempted when resolver_injective = true because a non-injective
        // resolver may have multiple inputs mapping to the same output, making the
        // reverse mapping ambiguous.
        if resolver_injective && bound_in_determined && !bound_in_determining {
            // Collect the current types at all determined positions.
            let mut ded_types = Vec::new();
            for &pos in ded_positions {
                if pos >= args.len() {
                    continue;
                }
                let ty = match &args[pos] {
                    // In-flight binding for bound_var — use the type being bound right now.
                    ConstraintArg::Var(v) if v.as_str() == bound_var => state.apply(bound_type),
                    // Another Var position — look up in substitution.
                    ConstraintArg::Var(v) => state.apply(&Type::TypeVar(v.clone(), 0)),
                    // Ground position — type already known from generalization (B-398).
                    ConstraintArg::Ground(t) => t.clone(),
                };
                ded_types.push((pos, ty));
            }

            // Only attempt reverse improvement when all determined positions are ground.
            let all_ded_ground = ded_types.iter().all(|(_, ty)| !ty.has_inference_vars());
            if all_ded_ground {
                // Scan InstanceEnv for an instance whose determined-position type
                // unifies with the ground determined types we have.
                let instance_env = state.build_instance_env_snapshot();
                if let Some((determining_types, det_pos_list)) = Box::pin(
                    instance_env.reverse_lookup_mptc(
                        class,
                        ded_positions,
                        &ded_types
                            .iter()
                            .map(|(_, ty)| ty.clone())
                            .collect::<Vec<_>>(),
                        state,
                    ),
                )
                .await
                {
                    // Unify each determining-position variable with the back-propagated type.
                    //
                    // Guard: skip variables already being processed by an outer FD improvement
                    // call (in fd_in_progress). This prevents the mutual-recursion cycle:
                    //   reverse(t1=Str) → bind(t0=Int) → forward(t0=Int) → bind(t1=Str)
                    //   → reverse(t1=Str) → … (hits MAX_FD_DEPTH).
                    // The guard makes the forward FD's re-binding of t1 a no-op: t1 is already
                    // being bound in the outer check_constraints_on_var("t1", Str, …) call.
                    for (det_pos, det_ty) in det_pos_list.iter().zip(determining_types.iter()) {
                        if *det_pos >= args.len() {
                            continue;
                        }
                        // Only Var positions can be unified — Ground positions are already resolved.
                        let det_var = match &args[*det_pos] {
                            ConstraintArg::Var(v) => v.clone(),
                            ConstraintArg::Ground(_) => continue,
                        };
                        // Skip if this var is already being processed by an outer FD step.
                        if state.fd_in_progress.contains(det_var.as_str()) {
                            continue;
                        }
                        let det_type_var = Type::TypeVar(det_var.clone(), 0);
                        state.fd_in_progress.insert(det_var.clone());
                        let result = Box::pin(unify(
                            &det_type_var,
                            det_ty,
                            state,
                            constraints,
                            span.clone(),
                        ))
                        .await;
                        state.fd_in_progress.remove(det_var.as_str());
                        result?;
                    }
                }
                // If no instance matches, silently skip — the determined type may be
                // a type variable that hasn't been fully resolved yet, or no instance
                // is registered for this determined type (not an error in the reverse direction).
            }
        }

        // ── Forward FD improvement ────────────────────────────────────────────
        if !bound_in_determining {
            // This binding doesn't affect the forward direction of this FD
            continue;
        }

        // Collect the current types for all determining positions.
        //
        // CRITICAL: Two sources of truth must be consulted in order:
        //
        // 1. `state.type_vars` — the unified TypeVar table containing all bindings.
        //
        // 2. `bound_type` — the concrete type being bound to `bound_var` RIGHT NOW.
        //    check_constraints_on_var is called BEFORE the binding is written to state.type_vars
        //    (see U-VAR arm: check_constraints_on_var → bind_type_var). So looking up `bound_var`
        //    from state would return the unbound TypeVar — not the value being bound.
        //    We must use `bound_type` directly for the variable currently being bound.
        //
        // Lookup order: for `bound_var` → use `bound_type` (in-flight, not yet bound).
        //               for all other vars → apply state (has all bindings).
        let mut det_types = Vec::new();
        for &pos in det_positions {
            if pos >= args.len() {
                continue;
            }
            let ty = match &args[pos] {
                // In-flight binding for bound_var — use the type being bound right now.
                ConstraintArg::Var(v) if v.as_str() == bound_var => state.apply(bound_type),
                // Another Var position — look up in substitution.
                ConstraintArg::Var(v) => state.apply(&Type::TypeVar(v.clone(), 0)),
                // Ground position — type already known from generalization (B-398).
                ConstraintArg::Ground(t) => t.clone(),
            };
            det_types.push((pos, ty));
        }

        // Check if ALL determining positions are ground
        let all_det_ground = det_types.iter().all(|(_, ty)| !ty.has_inference_vars());

        if !all_det_ground {
            // Not all determining positions are ground yet - can't improve
            continue;
        }

        // All determining positions are ground - look up the instance.
        // Multiple paths:
        // 1. Indexable + Record: special case for HasField-style resolution
        // 2. Resolver classes: type-stage function normalization
        // 3. General MPTC: InstanceEnv lookup (with literal widening on miss)

        // Special case: Indexable on Record/Union/Intersection/Top types uses
        // resolve_has_field for field lookup instead of instance registration.
        // Records are structural (not nominal), so they don't register instances —
        // instead, resolve_has_field applies [HAS-FIELD-REC], [HAS-FIELD-UNION],
        // [HAS-FIELD-INTER], and [HAS-FIELD-TOP] rules from type_unify.rs.
        let indexable_record_result = if class == "Indexable" && det_types.len() == 2 {
            let container_ty = &det_types[0].1;
            let key_ty = &det_types[1].1;

            match (container_ty, key_ty) {
                (
                    Type::Dict(_) | Type::Intersection(_) | Type::Any,
                    Type::StringLiteral(field_name),
                ) => {
                    // Route through resolve_has_field to apply [HAS-FIELD-REC],
                    // [HAS-FIELD-INTER], and [HAS-FIELD-TOP] rules.
                    // Missing fields in Record case: gradual degradation to Unknown
                    // (the access is valid at runtime — returns null).
                    let label = Label::Concrete(field_name.clone());
                    match resolve_has_field(&label, container_ty, state, span.clone(), 0) {
                        Ok(field_ty) => Some(field_ty),
                        Err(_) => Some(Type::Unknown),
                    }
                }
                (Type::Union(members), Type::StringLiteral(field_name)) => {
                    // [HAS-FIELD-UNION]: distribute field lookup across union members.
                    // [get key (A | B)] → get(key, A) | get(key, B)
                    // Each member that has the field contributes its field type.
                    // Members that don't support field access (or lack the field)
                    // contribute Unknown — gradual degradation allows runtime null.
                    let label = Label::Concrete(field_name.clone());
                    let field_types: Vec<Type> = members
                        .iter()
                        .map(|member| {
                            resolve_has_field(&label, member, state, span.clone(), 1)
                                .unwrap_or(Type::Unknown)
                        })
                        .collect();
                    Some(Type::normalize_union(field_types))
                }
                (Type::Dict(_) | Type::Union(_) | Type::Intersection(_) | Type::Any, Type::Str) => {
                    // Str key (from promoted StringLiteral) — can't resolve statically
                    Some(Type::Unknown)
                }
                _ => None, // Not a Record/Union/Intersection/Top case — fall through to general logic
            }
        } else {
            None
        };

        // Extract class_decl with a scoped read lock so the guard drops before
        // any subsequent &mut state borrows (e.g. in lookup_mptc).
        let class_decl_for_fd = { state.env.read().unwrap().get_class(class) };
        let result_type = if let Some(ty) = indexable_record_result {
            ty
        } else if let Some(class_decl) = class_decl_for_fd {
            // Check for resolver or fall back to general MPTC instance lookup
            if let Some(ref resolver_name) = class_decl.resolver.clone() {
                // Resolver-based path: construct a TypeStageApp and normalize it.
                // Normalization calls evaluate_resolver() which invokes the type-stage function.
                let det_arg_types: Vec<Type> = det_types.iter().map(|(_, ty)| ty.clone()).collect();
                let stage_app = Type::TypeStageApp {
                    fn_name: resolver_name.clone(),
                    args: det_arg_types,
                };
                let mut norm_ctx = crate::type_normalize::NormCtxt::new(
                    state.type_stage_env.clone(),
                    state.eval_ctx.clone(),
                );
                let resolved =
                    crate::type_normalize::normalize(&stage_app, &state.type_vars, &mut norm_ctx)
                        .await;

                // If normalization returned a stuck TypeStageApp, we can't improve yet.
                // Defer: the deferred_equalities mechanism will retry when more types are ground.
                if matches!(resolved, Type::TypeStageApp { .. }) {
                    continue;
                }
                resolved
            } else {
                // No resolver — fall back to general MPTC instance lookup via InstanceEnv.
                // Literal widening: IntLiteral → Int, StringLiteral → Str. These are Rust-internal
                // types; prelude cannot declare instances for them, but class satisfaction is
                // covariant: if T <: S and S satisfies C, then T satisfies C. We widen on miss.
                let det_arg_types: Vec<Type> = det_types.iter().map(|(_, ty)| ty.clone()).collect();

                // Build a temporary InstanceEnv snapshot to avoid borrow checker conflict.
                let instance_env = state.build_instance_env_snapshot();
                let lookup_result =
                    Box::pin(instance_env.lookup_mptc(class, &det_arg_types, state)).await;

                // On miss, retry with widened literal types (IntLiteral→Int, StringLiteral→Str).
                let lookup_result = if lookup_result.is_none() {
                    let widened: Vec<Type> = det_arg_types
                        .iter()
                        .map(|ty| crate::typecheck::typecheck_call::widen_literal_types(ty.clone()))
                        .collect();
                    if widened != det_arg_types {
                        let instance_env2 = state.build_instance_env_snapshot();
                        Box::pin(instance_env2.lookup_mptc(class, &widened, state)).await
                    } else {
                        None
                    }
                } else {
                    lookup_result
                };

                match lookup_result {
                    Some(inst) => {
                        // Extract the determined type from the instance.
                        // For a multi-param MPTC instance, instance_type is a Record with
                        // numbered fields (0, 1, 2, …). Determined positions are those NOT in det_positions.
                        let det_position_set: std::collections::HashSet<usize> =
                            inst.det_positions.iter().copied().collect();

                        match &inst.instance_type {
                            Type::Dict(row) => {
                                // Find the first field index not in the determining set
                                let total_params = row.fields.len();
                                let determined_pos =
                                    (0..total_params).find(|i| !det_position_set.contains(i));
                                match determined_pos {
                                    Some(pos) => row
                                        .fields
                                        .get(&pos.to_string())
                                        .cloned()
                                        .ok_or_else(|| {
                                            TypeErrorTyped::Generic(
                                                GenericTypeError {
                                                    message: format!(
                                                        "no instance for {} (determined field {} missing)",
                                                        class, pos
                                                    ),
                                                    span: span.clone(),
                                                    notes: vec![], call_stack: vec![],
                                                },
                                            )
                                        })?,
                                    None => {
                                        return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                                            message: format!(
                                                "no instance for {} (no determined position found)",
                                                class
                                            ),
                                            span: span.clone(),
                                            notes: vec![], call_stack: vec![],
                                        })));
                                    }
                                }
                            }
                            _ => {
                                return Err(TypeError::from(TypeErrorTyped::Generic(
                                    GenericTypeError {
                                        message: format!(
                                            "no instance for {} (unexpected instance_type shape)",
                                            class
                                        ),
                                        span: span.clone(),
                                        notes: vec![],
                                        call_stack: vec![],
                                    },
                                )));
                            }
                        }
                    }
                    None => {
                        // B-317: All determining positions are ground at this point
                        // (checked at line 525). If lookup_mptc returns None, there's
                        // genuinely no matching instance — unless a determining position
                        // is `Unknown` (gradual) or a structural type that could satisfy
                        // the class at runtime. Only error when all determining positions
                        // are definitively non-matching scalar types.
                        //
                        // For Indexable specifically: Record/Union/Intersection/Top are
                        // structural types that may be sequences at runtime (Tinct's `[]`
                        // syntax is ambiguous between record and sequence). Unknown is the
                        // gradual escape hatch. Only scalar types (Int, Float, Bool, Str,
                        // Function, etc.) are definitively non-Indexable.
                        let should_defer = det_types
                            .iter()
                            .any(|(_, ty)| !is_definitely_no_instance_for(class, ty));
                        if should_defer {
                            continue;
                        }
                        return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                            message: format!("no instance for {}", class),
                            span: span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        })));
                    }
                }
            }
        } else {
            // Class not found in class_env — should not happen
            return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                message: format!("unknown class {}", class),
                span: span.clone(),
                notes: vec![],
                call_stack: vec![],
            })));
        };

        // Unify each determined position with the result type.
        // Ground positions are already resolved — skip them.
        for &ded_pos in ded_positions {
            if ded_pos >= args.len() {
                continue;
            }
            // Extract the variable name for this determined position.
            // Ground positions are already concrete — no further unification needed.
            let ded_var = match &args[ded_pos] {
                ConstraintArg::Var(v) => v.clone(),
                ConstraintArg::Ground(_) => continue,
            };

            // Guard: skip variables already being processed by an outer FD improvement
            // call (in fd_in_progress). This prevents the mutual-recursion cycle when
            // an injective resolver's reverse FD fires from binding the determined variable:
            //   forward(t0=Int) → bind(t1=Str) → reverse(t1=Str) → bind(t0=Int)
            //   → forward(t0=Int) → … (hits MAX_FD_DEPTH).
            // If ded_var is in fd_in_progress, the determined binding was initiated by an
            // outer FD step and will be completed there — skipping here is idempotent.
            if state.fd_in_progress.contains(ded_var.as_str()) {
                continue;
            }

            let ded_type_var = Type::TypeVar(ded_var.clone(), 0);

            // Bindings go directly into state.type_vars — no separate substitution needed.
            state.fd_in_progress.insert(ded_var.clone());
            let result = Box::pin(unify(
                &ded_type_var,
                &result_type,
                state,
                constraints,
                span.clone(),
            ))
            .await;
            state.fd_in_progress.remove(ded_var.as_str());
            result?;
        }
    }

    Ok(())
}

/// Returns true when `ty` is definitively not a member of `class` — i.e., we can
/// statically rule out any runtime instance.  Returns false when we cannot rule
/// it out, meaning we should defer the constraint rather than error.
///
/// Used in `improve_functional_dependency_inner` to decide whether to emit a
/// "no instance" error or silently defer (continue) when `lookup_mptc` returns
/// `None` despite all determining positions being ground.
///
/// Conservative rule: only return `true` for scalar/primitive types that are
/// structurally incompatible with the class. Structural types (Record, Union,
/// Intersection, App, etc.) and the gradual `Unknown` can always
/// potentially satisfy a class at runtime, so we return `false` for them.
fn is_definitely_no_instance_for(class: &str, ty: &Type) -> bool {
    match class {
        "Indexable" => {
            // A type is definitively non-Indexable only if it is a scalar/primitive
            // that cannot possibly be a container at runtime.
            // Record, Union, Intersection, Any, App, Unknown, NominalVariant — might work.
            // Scalars (Int/Float/Str and their literals), Function, Never — cannot.
            matches!(
                ty,
                Type::Int
                    | Type::Float
                    | Type::Str
                    | Type::Never
                    | Type::Function { .. }
                    | Type::IntLiteral(_)
                    | Type::StringLiteral(_)
            )
        }
        _ => {
            // For other MPTC classes, treat any ground non-Unknown type as
            // definitely non-matching when lookup_mptc returns None.
            // Unknown gets special handling (defer).
            !matches!(ty, Type::Unknown)
        }
    }
}

/// When binding a constrained type variable, promote literal types to their parent types.
/// This prevents `[+ 1 2]` from failing: without promotion, `_t0` (Numeric) would bind
/// to `IntLiteral(1)`, then unification of `IntLiteral(1)` with `IntLiteral(2)` would fail.
/// With promotion, `_t0` binds to `Int`, and both `IntLiteral(1)` and `IntLiteral(2)` unify
/// with `Int` via the literal-to-parent promotion rules.
///
/// When binding a constrained type variable, promote literal types to their parent types.
/// This prevents `[+ 1 2]` from failing: without promotion, `_t0` (Numeric) would bind
/// to `IntLiteral(1)`, then unification of `IntLiteral(1)` with `IntLiteral(2)` would fail.
/// With promotion, `_t0` binds to `Int`, and both `IntLiteral(1)` and `IntLiteral(2)` unify
/// with `Int` via the literal-to-parent promotion rules.
///
/// Promotion applies uniformly for ANY class constraint — no class-name whitelist.
/// IntLiteral → Int and StringLiteral → Str are safe for all classes because literal
/// instances always entail parent instances (a literal value is always a valid value of
/// the parent type).
fn promote_literal_for_constrained_var(
    var_name: &str,
    ty: Type,
    constraints: &[Constraint],
    state: &InferState,
) -> Type {
    // Label-kinded TypeVars must not be promoted regardless of constraint presence
    // (preserves StringLiteral identity for field access)
    if state.get_kind(var_name) == Some(Kind::Label) {
        return ty;
    }

    // Check if this variable has ANY class constraint — if so, promote literals.
    let has_any_class_constraint = constraints.iter().any(|c| match c {
        Constraint::Class { vars, .. } => vars.iter().any(|v| v.as_var() == Some(var_name)),
        _ => false,
    });

    if !has_any_class_constraint {
        return ty;
    }

    match ty {
        Type::IntLiteral(_) => Type::Int,
        Type::StringLiteral(_) => Type::Str,
        _ => ty,
    }
}

/// Resolve a HasField constraint against a dict type.
/// Returns the field type if the constraint can be satisfied, or an error.
///
/// Implements the [HAS-FIELD-*] rules from hkt-monads.md §Field Access Typing:
/// - [HAS-FIELD-REC]: Record with matching field → return field type
/// - [HAS-FIELD-UNION]: Union members → collect field types, return Union
/// - [HAS-FIELD-INTER]: Intersection → all members must have field, return Intersection of field types
/// - [HAS-FIELD-TOP]/[HAS-FIELD-UNKNOWN]: return Unknown
/// - [HAS-FIELD-NEVER]: return Never
const MAX_RESOLVE_HAS_FIELD_DEPTH: usize = 256;

pub fn resolve_has_field(
    label: &Label,
    dict_type: &Type,
    state: &mut InferState,
    span: Span,
    depth: usize,
) -> Result<Type, TypeError> {
    // Check recursion depth to prevent infinite loops on cyclic types
    if depth > MAX_RESOLVE_HAS_FIELD_DEPTH {
        return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
            message: "HasField recursion depth exceeded".to_string(),
            span,
            notes: vec![],
            call_stack: vec![],
        })));
    }

    // Resolve label to concrete string
    let label_str = match label {
        Label::Concrete(s) => s.clone(),
        Label::Var(var_name) => {
            // Look up the label var in substitution
            match state.lookup_binding(var_name) {
                Some(Type::StringLiteral(s)) => s,
                _ => {
                    return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                        message: format!(
                            "label variable {} not bound to a string literal",
                            var_name
                        ),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    })))
                }
            }
        }
    };

    // Apply substitution to dict_type to dereference any already-bound TypeVars
    let dict_type = state.apply(dict_type);

    match &dict_type {
        // [HAS-FIELD-REC]: Record with matching named field → return field type.
        // [HAS-FIELD-MAP]: Open dict (Uniform tail) → any field has the Uniform value type.
        // This handles Map[K:V] (Uniform { value: V }) — `get "key" map` returns V.
        // It also handles Dict (Uniform { value: Any }) — any field access returns Any.
        Type::Dict(row) => {
            if let Some(field_ty) = row.fields.get(&label_str) {
                // Named field found directly — return its type.
                Ok(field_ty.clone())
            } else {
                match &row.tail {
                    crate::type_def::RowTail::Uniform { value, .. } => {
                        // Open dict or typed Map: any field (beyond named ones) has value type.
                        // Map[K:V] has no named fields but Uniform { value: V } says all values are V.
                        Ok(*value.clone())
                    }
                    crate::type_def::RowTail::Empty => {
                        // Closed record (Record) with no matching field — type error.
                        Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                            message: format!("record has no field '{}'", label_str),
                            span,
                            notes: vec![], call_stack: vec![],
                        })))
                    }
                }
            }
        }

        // [HAS-FIELD-UNION]: Union members → collect field types, return Union
        Type::Union(members) => {
            let mut field_types = Vec::new();
            for member in members {
                let field_ty = resolve_has_field(label, member, state, span.clone(), depth + 1)?;
                field_types.push(field_ty);
            }
            Ok(Type::normalize_union(field_types))
        }

        // [HAS-FIELD-INTER]: Intersection → all members must have field, return Intersection of field types
        Type::Intersection(members) => {
            let mut field_types = Vec::new();
            for member in members {
                let field_ty = resolve_has_field(label, member, state, span.clone(), depth + 1)?;
                field_types.push(field_ty);
            }
            Ok(Type::normalize_intersection(field_types))
        }

        // [HAS-FIELD-TOP]: Top → Top (accessing a field on an untyped dict yields Top)
        Type::Any => Ok(Type::Any),

        // [HAS-FIELD-UNKNOWN]: Unknown → Unknown
        Type::Unknown => Ok(Type::Unknown),

        // [HAS-FIELD-NEVER]: Never → Never (vacuous)
        Type::Never => Ok(Type::Never),

        // TypeVar: defer constraint (handled by caller)
        Type::TypeVar(_, _) => Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
            message: "cannot resolve HasField constraint on unbound type variable (expected caller to defer)".to_string(),
            span,
            notes: vec![], call_stack: vec![],
        }))),

        // All other types don't support field access
        _ => Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
            message: format!("type {} does not support field access", dict_type),
            span,
            notes: vec![], call_stack: vec![],
        }))),
    }
}

const MAX_APPLY_DEPTH: usize = 256;

// T-991: Thread-local visited set for `apply_substitution`.
// Declared at module level so the static-initialization semantics are
// clear: the HashSet is allocated once per thread and reused across all calls.
thread_local! {
    static VISITED_TYPES: std::cell::RefCell<HashSet<String>> = std::cell::RefCell::new(HashSet::new());
}

/// Apply substitution to a type: resolve all bound TypeVars by looking up bindings
/// in the unified `type_vars` IndexMap.
pub fn apply_substitution(ty: &Type, type_vars: &IndexMap<String, TypeVarEntry>) -> Type {
    // Fast-path for concrete types: no type variables, return clone immediately.
    match ty {
        Type::Int
        | Type::IntLiteral(_)
        | Type::Float
        | Type::Str
        | Type::StringLiteral(_)
        | Type::Bytes
        | Type::Unknown
        | Type::Any
        | Type::Never
        | Type::Proxy
        | Type::Error(_)
        | Type::DirCap
        | Type::NetCap
        | Type::Uri
        | Type::Timestamp
        | Type::Duration
        | Type::ClockCap
        | Type::Timezone
        | Type::QuicSession
        | Type::Http2Session
        | Type::Http3Session
        | Type::DatagramHandle
        | Type::QuicDatagramHandle => {
            return ty.clone();
        }
        _ => {}
    }
    // Second fast-path: structured types with no inference variables are concrete.
    if !ty.has_inference_vars() {
        return ty.clone();
    }
    // Check if there are any bindings at all
    if type_vars.values().all(|e| e.binding.is_none()) {
        return ty.clone();
    }
    VISITED_TYPES.with(|visited_cell| {
        let mut visited = visited_cell.borrow_mut();
        visited.clear();
        apply_type_with_visited(ty, type_vars, 0, &mut visited).into_owned()
    })
}

/// Apply substitution with an externally-supplied visited set and depth tracking.
pub fn apply_type_with_visited<'a>(
    ty: &'a Type,
    type_vars: &IndexMap<String, TypeVarEntry>,
    depth: usize,
    visited_types: &mut HashSet<String>,
) -> Cow<'a, Type> {
    if depth >= MAX_APPLY_DEPTH {
        return Cow::Borrowed(ty);
    }
    match ty {
        Type::TypeVar(name, level) => {
            if visited_types.contains(name) {
                return Cow::Borrowed(ty);
            }
            let bound_opt = type_vars.get(name.as_str()).and_then(|e| e.binding.clone());
            match bound_opt {
                Some(bound) => {
                    visited_types.insert(name.clone());
                    let result =
                        apply_type_with_visited(&bound, type_vars, 0, visited_types).into_owned();
                    visited_types.remove(name);
                    Cow::Owned(result)
                }
                None => Cow::Owned(Type::TypeVar(name.clone(), *level)),
            }
        }
        Type::Dict(row) => {
            let applied_row = apply_row_with_visited(row, type_vars, depth + 1, visited_types);
            Cow::Owned(Type::Dict(applied_row))
        }
        Type::Function {
            params,
            ret,
            variadic,
            required_count,
        } => Cow::Owned(Type::Function {
            params: params
                .iter()
                .map(|(name, p_ty)| {
                    (
                        name.clone(),
                        apply_type_with_visited(p_ty, type_vars, depth + 1, visited_types)
                            .into_owned(),
                    )
                })
                .collect(),
            ret: Box::new(
                apply_type_with_visited(ret, type_vars, depth + 1, visited_types).into_owned(),
            ),
            variadic: *variadic,
            required_count: *required_count,
        }),
        Type::Union(members) => {
            let applied_members: Vec<Type> = members
                .iter()
                .map(|m| {
                    apply_type_with_visited(m, type_vars, depth + 1, visited_types).into_owned()
                })
                .collect();
            Cow::Owned(Type::normalize_union(applied_members))
        }
        Type::Intersection(members) => {
            let applied_members: Vec<Type> = members
                .iter()
                .map(|m| {
                    apply_type_with_visited(m, type_vars, depth + 1, visited_types).into_owned()
                })
                .collect();
            Cow::Owned(Type::normalize_intersection(applied_members))
        }
        Type::Negation(inner) => Cow::Owned(Type::Negation(Box::new(
            apply_type_with_visited(inner, type_vars, depth + 1, visited_types).into_owned(),
        ))),
        Type::App(f, a) => {
            let f_applied =
                apply_type_with_visited(f, type_vars, depth + 1, visited_types).into_owned();
            let a_applied =
                apply_type_with_visited(a, type_vars, depth + 1, visited_types).into_owned();

            // Operator(Name) applied to arg → App(TyCon(Name), arg).
            // All type constructors follow this uniform pattern.
            if let Type::Operator(ctor_name) = &f_applied {
                return Cow::Owned(Type::App(
                    Box::new(Type::TyCon(ctor_name.clone())),
                    Box::new(a_applied),
                ));
            }

            Cow::Owned(Type::App(Box::new(f_applied), Box::new(a_applied)))
        }
        Type::TypeStageApp { fn_name, args } => Cow::Owned(Type::TypeStageApp {
            fn_name: fn_name.clone(),
            args: args
                .iter()
                .map(|arg| {
                    apply_type_with_visited(arg, type_vars, depth + 1, visited_types).into_owned()
                })
                .collect(),
        }),
        Type::NominalVariant {
            tycon,
            ctor,
            fields,
        } => {
            let applied_fields =
                apply_row_with_visited(fields, type_vars, depth + 1, visited_types);
            Cow::Owned(Type::NominalVariant {
                tycon: tycon.clone(),
                ctor: ctor.clone(),
                fields: applied_fields,
            })
        }
        Type::TyCon(_) => Cow::Borrowed(ty),
        Type::Recursive { var, body } => {
            let applied_body = apply_type_with_visited(body, type_vars, depth + 1, visited_types);
            match applied_body {
                Cow::Borrowed(_) => Cow::Borrowed(ty),
                Cow::Owned(new_body) => Cow::Owned(Type::Recursive {
                    var: var.clone(),
                    body: Box::new(new_body),
                }),
            }
        }
        Type::Operator(name) => {
            if visited_types.contains(name) {
                return Cow::Borrowed(ty);
            }
            let bound_opt = type_vars.get(name.as_str()).and_then(|e| e.binding.clone());
            match bound_opt {
                Some(bound) => {
                    visited_types.insert(name.clone());
                    let result =
                        apply_type_with_visited(&bound, type_vars, 0, visited_types).into_owned();
                    visited_types.remove(name);
                    Cow::Owned(result)
                }
                None => Cow::Owned(Type::Operator(name.clone())),
            }
        }
        _ => Cow::Borrowed(ty),
    }
}

/// Apply substitution to a Row.
pub fn apply_row_with_visited(
    row: &Row,
    type_vars: &IndexMap<String, TypeVarEntry>,
    depth: usize,
    visited_types: &mut HashSet<String>,
) -> Row {
    if depth >= MAX_APPLY_DEPTH {
        return row.clone();
    }

    let new_fields: IndexMap<String, Type> = row
        .fields
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                apply_type_with_visited(v, type_vars, depth + 1, visited_types).into_owned(),
            )
        })
        .collect();

    let new_tail = match &row.tail {
        crate::type_def::RowTail::Empty => crate::type_def::RowTail::Empty,
        crate::type_def::RowTail::Uniform { key, value } => {
            let new_key = key.as_ref().map(|k| {
                Box::new(
                    apply_type_with_visited(k, type_vars, depth + 1, visited_types).into_owned(),
                )
            });
            let new_value = Box::new(
                apply_type_with_visited(value, type_vars, depth + 1, visited_types).into_owned(),
            );
            crate::type_def::RowTail::Uniform {
                key: new_key,
                value: new_value,
            }
        }
    };

    Row {
        fields: new_fields,
        tail: new_tail,
    }
}

/// BAS record unification: unify only the fields shared between both rows, then unify tails.
/// Fields unique to one row are ignored — BAS width subtyping handles openness
/// via is_subtype (a record with MORE fields satisfies an annotation with FEWER fields).
///
/// Tail unification rules (T-939/T-1007/T-1024/B-327):
///   (Empty, Empty)           — no-op (both closed, field unification above is sufficient)
///   (Empty, Uniform{V, ..})  — UNIFY-UNIFORM step 2/3: TypeVar join or concrete subtype check
///   (Uniform{V, ..}, Empty)  — UNIFY-UNIFORM step 2/3: symmetric case
///   (Uniform{V1, k1}, Uniform{V2, k2}) — unify V1 ~ V2; key types if both present (B-327);
///                               then validate named fields against unified V (T-1007 steps 2-3)
async fn unify_rows(
    row1: &Row,
    row2: &Row,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) -> Result<(), TypeError> {
    // Fast-path: identical field sets — unify all named fields, then fall through to tail.
    // Previously had an early return here, which silently swallowed tail mismatches when both
    // rows had identical named fields but different tails (e.g., Empty vs Uniform).
    if row1.fields.len() == row2.fields.len()
        && row1.fields.keys().all(|k| row2.fields.contains_key(k))
    {
        for (key, ty1) in &row1.fields {
            let ty2 = &row2.fields[key];
            Box::pin(unify(ty1, ty2, state, constraints, span.clone())).await?;
        }
        // Fall through to tail unification — do NOT return here.
    } else {
        // General case: unify only fields that appear in BOTH rows (intersection).
        // Fields unique to one side are not errors — BAS width subtyping handles them via is_subtype.
        let mut shared_count = 0;
        let mut row1_has_inference_vars = false;
        let mut row2_has_inference_vars = false;
        for (key, ty1) in &row1.fields {
            if ty1.has_inference_vars() {
                row1_has_inference_vars = true;
            }
            if let Some(ty2) = row2.fields.get(key) {
                shared_count += 1;
                Box::pin(unify(ty1, ty2, state, constraints, span.clone())).await?;
            }
        }
        for ty2 in row2.fields.values() {
            if ty2.has_inference_vars() {
                row2_has_inference_vars = true;
            }
        }

        // Disjoint record detection: two non-empty records with ZERO shared fields and all-concrete
        // field types are incompatible — no value can satisfy both `[a: Int]` and `[b: Str]` under
        // unification (BAS width subtyping is handled by is_subtype, not unification).
        //
        // Conservative guard: if either row contains inference variables (TypeVars), we cannot
        // determine incompatibility statically — the variable might be bound to a compatible type.
        // In that case, fall back to level-zeroing to prevent unsound generalization.
        if shared_count == 0 && !row1.fields.is_empty() && !row2.fields.is_empty() {
            if row1_has_inference_vars || row2_has_inference_vars {
                // Conservative path: cannot prove incompatibility statically.
                // Lower TypeVars in FTV(row1) ∩ FTV(row2) to level 0 to prevent unsound
                // generalization of variables constrained by both sides.
                //
                // Only variables appearing in BOTH rows are constrained by this cross-row
                // relationship. Variables unique to one row are independent and can be
                // generalized freely — zeroing them is unsoundly conservative.
                // (Kiselyov 2013: level-zeroing should target only actually-constrained vars.)
                let mut vars1 = HashSet::new();
                for ty in row1.fields.values() {
                    ty.collect_type_vars(&mut vars1);
                }
                let mut vars2 = HashSet::new();
                for ty in row2.fields.values() {
                    ty.collect_type_vars(&mut vars2);
                }
                for var_name in vars1.intersection(&vars2) {
                    if let Some(entry) = state.type_vars.get_mut(var_name.as_str()) {
                        entry.level = 0;
                    }
                }
            } else {
                // Both rows have concrete field types and no shared fields: structurally incompatible.
                return Err(TypeError::from(TypeErrorTyped::UnificationFailure(
                    UnificationFailure {
                        expected: Type::Dict(row1.clone()),
                        got: Type::Dict(row2.clone()),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    },
                )));
            }
        }
    }

    // Tail unification always executes — whether we took the fast-path or general path above.
    // Two rows with identical named fields but different tails (e.g., {a: Int, tail: Empty} vs
    // {a: Int, tail: Uniform{Int}}) are incompatible and must be rejected here.
    use crate::type_def::RowTail;
    match (&row1.tail, &row2.tail) {
        // Both closed — no tail constraint to unify
        (RowTail::Empty, RowTail::Empty) => {}

        // Both Uniform — unify value types, key types, then validate named fields
        // (T-1007/UNIFY-UNIFORM steps 1-3).
        (RowTail::Uniform { key: k1, value: v1 }, RowTail::Uniform { key: k2, value: v2 }) => {
            Box::pin(unify(v1, v2, state, constraints, span.clone())).await?;

            // B-327: Unify key type constraints when both sides specify them.
            // When only one side specifies a key type (asymmetric), the unconstrained
            // side is implicitly compatible with any key type (Unknown semantics) and
            // no error is emitted — the keyed side's constraint is preserved in its row.
            if let (Some(k1_ty), Some(k2_ty)) = (k1, k2) {
                Box::pin(unify(k1_ty, k2_ty, state, constraints, span.clone())).await?;
            }

            // UNIFY-UNIFORM steps 2-3: after unifying the value types, apply the
            // substitution to fixpoint and validate all named fields from both rows.
            //
            // After unify(v1, v2), v1 and v2 are the same type (one may be bound to the
            // other). Apply substitution to v1 to get the resolved value type.
            let v_fixed = state.apply(v1);

            // Collect named field types from both rows.
            let all_fields: Vec<Type> = row1
                .fields
                .values()
                .chain(row2.fields.values())
                .cloned()
                .collect();

            if !all_fields.is_empty() {
                if let Type::TypeVar(alpha, _) = &v_fixed {
                    // Step 2: V is still an unbound TypeVar α — compute join of all named
                    // field types and unify α with that join.
                    let join = Type::normalize_union(all_fields);
                    Box::pin(unify(
                        &Type::TypeVar(alpha.clone(), 0),
                        &join,
                        state,
                        constraints,
                        span.clone(),
                    ))
                    .await?;
                } else if !v_fixed.has_inference_vars() {
                    // Step 3: V is concrete — each named field Ti must be a subtype of V.
                    for field_ty in &all_fields {
                        let field_fixed = state.apply(field_ty);
                        if !Type::is_subtype(&field_fixed, &v_fixed, None) {
                            return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                                message: format!(
                                    "field type {field_fixed} does not conform to Uniform constraint {v_fixed}"
                                ),
                                span: span.clone(),
                                notes: vec![], call_stack: vec![],
                            })));
                        }
                    }
                }
                // If v_fixed has inference vars but is not a bare TypeVar (e.g., a partially
                // resolved union), defer validation — unification of the contained vars will
                // eventually make it concrete or a TypeVar, and the field check will occur
                // at the next unification call that resolves this tail.
            }
        }

        // Closed row unified with Uniform constraint (T-1024/UNIFY-UNIFORM step 2).
        //
        // When a closed (Empty-tailed) row is unified with a Uniform-tailed row:
        // - Apply substitution to the Uniform value type V to get V'.
        // - If V' is an unbound TypeVar α: compute the join of all named field types
        //   from BOTH rows and unify α with that join.
        // - If V' is concrete: each named field Ti from BOTH rows must satisfy
        //   is_subtype(Ti, V') — else emit a type error.
        //
        // Both rows' named fields are checked: the Empty-tailed row contributes its
        // named fields (all must conform), and so does the Uniform-tailed row (its own
        // named fields must also conform to its own constraint V). This mirrors the
        // Uniform+Uniform case which correctly chains both rows' fields.
        (RowTail::Empty, RowTail::Uniform { value, .. })
        | (RowTail::Uniform { value, .. }, RowTail::Empty) => {
            let v_fixed = state.apply(value);

            // Collect named field types from both rows (T-1024: both rows contribute).
            let field_types: Vec<Type> = row1
                .fields
                .values()
                .chain(row2.fields.values())
                .cloned()
                .collect();

            if field_types.is_empty() {
                // Neither row has any named fields — compatible with any Uniform.
                return Ok(());
            }

            if let Type::TypeVar(alpha, _) = &v_fixed {
                // V is an unbound TypeVar α: compute join of all named field types and unify.
                let join = Type::normalize_union(field_types);
                Box::pin(unify(
                    &Type::TypeVar(alpha.clone(), 0),
                    &join,
                    state,
                    constraints,
                    span.clone(),
                ))
                .await?;
            } else if !v_fixed.has_inference_vars() {
                // V is concrete: each named field Ti must be a subtype of V.
                for field_ty in &field_types {
                    let field_fixed = state.apply(field_ty);
                    if !Type::is_subtype(&field_fixed, &v_fixed, Some(&state.tycon_env)) {
                        return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                            message: format!(
                                "field type {field_fixed} does not conform to Uniform constraint {v_fixed}"
                            ),
                            span: span.clone(),
                            notes: vec![], call_stack: vec![],
                        })));
                    }
                }
            }
            // Partially resolved V (has inference vars but not a bare TypeVar): defer.
        }
    }

    Ok(())
}

// S-861: equirecursive-checker
// substitute_recvar and substitute_recvar_row are pub(crate) in type_def.rs (canonical
// location, next to unfold_once). Imported here via `use super::*` (type_def re-exports
// everything from the parent module). No local copy needed.

/// Lower levels of all type/row variables in `ty` to min(their level, cap_level).
/// Performs occurs check simultaneously: returns true if `occurs_name` appears in the tree.
/// No allocation -- directly updates `state.levels` in a single recursive walk.
fn lower_levels_check_occurs(
    ty: &Type,
    occurs_name: &str,
    cap_level: u32,
    state: &mut InferState,
) -> bool {
    match ty {
        Type::TypeVar(name, _) => {
            let found = name == occurs_name;
            let current_level = state.get_level(name).unwrap_or(0);
            state.set_level(name.clone(), current_level.min(cap_level));
            found
        }
        Type::Dict(row) => {
            let mut found = false;
            for ty in row.fields.values() {
                found |= lower_levels_check_occurs(ty, occurs_name, cap_level, state);
            }
            // Lower levels through RowTail::Uniform key and value types
            if let crate::type_def::RowTail::Uniform { key, value } = &row.tail {
                if let Some(k) = key {
                    found |= lower_levels_check_occurs(k, occurs_name, cap_level, state);
                }
                found |= lower_levels_check_occurs(value, occurs_name, cap_level, state);
            }
            found
        }
        Type::Function {
            params,
            ret,
            variadic: _,
            required_count: _,
        } => {
            let mut found = false;
            for (_name, p_ty) in params {
                found |= lower_levels_check_occurs(p_ty, occurs_name, cap_level, state);
            }
            found |= lower_levels_check_occurs(ret, occurs_name, cap_level, state);
            found
        }
        Type::Union(members) => {
            let mut found = false;
            for m in members {
                found |= lower_levels_check_occurs(m, occurs_name, cap_level, state);
            }
            found
        }
        Type::Intersection(members) => {
            let mut found = false;
            for m in members {
                found |= lower_levels_check_occurs(m, occurs_name, cap_level, state);
            }
            found
        }
        Type::Negation(inner) => lower_levels_check_occurs(inner, occurs_name, cap_level, state),
        Type::App(f, a) => {
            let mut found = false;
            found |= lower_levels_check_occurs(f, occurs_name, cap_level, state);
            found |= lower_levels_check_occurs(a, occurs_name, cap_level, state);
            found
        }
        Type::Operator(name) => {
            let found = name == occurs_name;
            let current_level = state.get_level(name).unwrap_or(0);
            state.set_level(name.clone(), current_level.min(cap_level));
            found
        }
        // Leaf types — no type variables to lower, no occurs check needed.
        // Exhaustive match ensures new compound types are not silently missed.
        Type::Int
        | Type::IntLiteral(_)
        | Type::Float
        | Type::Str
        | Type::StringLiteral(_)
        | Type::Bytes
        | Type::Proxy
        | Type::Unknown
        | Type::Any
        | Type::Error(_)
        | Type::DirCap
        | Type::NetCap
        | Type::TyCon(_)
        | Type::Uri
        | Type::Timestamp
        | Type::Duration
        | Type::ClockCap
        | Type::Timezone
        | Type::QuicSession
        | Type::Http2Session
        | Type::Http3Session
        | Type::QuicDatagramHandle
        | Type::DatagramHandle
        | Type::Never => false,
        Type::TypeStageApp { fn_name: _, args } => {
            let mut found = false;
            for arg in args {
                found |= lower_levels_check_occurs(arg, occurs_name, cap_level, state);
            }
            found
        }
        Type::NominalVariant {
            tycon: _,
            ctor: _,
            fields,
        } => {
            let mut found = false;
            for ty in fields.fields.values() {
                found |= lower_levels_check_occurs(ty, occurs_name, cap_level, state);
            }
            if let crate::type_def::RowTail::Uniform { key, value } = &fields.tail {
                if let Some(k) = key {
                    found |= lower_levels_check_occurs(k, occurs_name, cap_level, state);
                }
                found |= lower_levels_check_occurs(value, occurs_name, cap_level, state);
            }
            found
        }
        // S-860: equirecursive-types-core — recurse into the body.
        // The `var` binder name is a gensym'd μ-binder (not a unification variable), so it
        // never appears in `state.levels` and must not be treated as an occurs-check target.
        // Level lowering and occurs checking must recurse into the body because the body may
        // contain TypeVar inference variables (e.g., in a partially-inferred recursive type).
        // NOT recursing would leave TypeVars inside a Recursive body invisible to level
        // lowering — a soundness gap per the design review (agent_type-theorist mempalace).
        Type::Recursive { var: _, body } => {
            lower_levels_check_occurs(body, occurs_name, cap_level, state)
        }
    }
}

/// Transfer all `Constraint::Class` entries from `alpha` to `beta` (deduplicated).
///
/// Called during TypeVar→TypeVar binding (U-VAR-LEVEL and U-VAR-LEVEL-SYM arms) to move
/// α's class obligations onto β before α is eliminated. This allows the constraint check
/// to be deferred until β is eventually bound to a concrete type. `HasField` constraints
/// are NOT transferred — they reference the specific dict variable and must not migrate.
///
/// Returns immediately if α has no class constraints (fast path for the common case).
fn transfer_class_constraints(alpha: &str, beta: &str, constraints: &mut Vec<Constraint>) {
    // Collect all Class constraints on α (both single-param and MPTC).
    // A constraint applies to α if any Var position names α.
    let alpha_constraints: Vec<(Arc<ClassDecl>, Vec<ConstraintArg>)> = constraints
        .iter()
        .filter_map(|c| match c {
            Constraint::Class { class, vars, .. }
                if vars.iter().any(|v| v.as_var() == Some(alpha)) =>
            {
                Some((Arc::clone(class), vars.clone()))
            }
            _ => None,
        })
        .collect();
    if alpha_constraints.is_empty() {
        return;
    }

    // Transfer to β (deduplicated: only add if not already present).
    // F3 FIX: For MPTC constraints, substitute alpha→beta in the Var args.
    // Ground args are preserved as-is (they don't reference alpha).
    let beta_existing: HashSet<Vec<ConstraintArg>> = constraints
        .iter()
        .filter_map(|c| match c {
            Constraint::Class { class: _, vars, .. }
                if vars.iter().any(|v| v.as_var() == Some(beta)) =>
            {
                Some(vars.clone())
            }
            _ => None,
        })
        .collect();

    for (class, args) in alpha_constraints {
        // Substitute alpha → beta in Var args; Ground args pass through unchanged.
        let renamed_args: Vec<ConstraintArg> = args
            .iter()
            .map(|arg| match arg {
                ConstraintArg::Var(v) if v.as_str() == alpha => {
                    ConstraintArg::Var(beta.to_string())
                }
                other => other.clone(),
            })
            .collect();

        // F3 FIX: Check if the renamed constraint already exists (avoid duplicates)
        if !beta_existing.contains(&renamed_args) {
            constraints.push(Constraint::Class {
                class,
                vars: renamed_args,
                origin_name: None,
                origin_span: None,
            });
        }
    }
}

// bind_single_type_var_from_compound removed (T-1212): replaced by bas_cvar1_rewrite
// and bas_cvar2_rewrite which implement the full BAS C-Var1/2 constraint rewriting
// rules (Parreaux & Chau 2022, §3.2.1) with negation types and bounds.

/// [C-VAR1] BAS constraint rewriting for Union types containing TypeVars.
///
/// `τ₁ ≤ τ₂ ∨ α`  →  `τ₁ & ~τ₂ ≤ α`
///
/// Computes the residual `concrete & ~(union of concrete members)` and uses it
/// to determine what to bind the TypeVar to. The residual represents the part of
/// `concrete` not already covered by the union's non-variable members.
///
/// Parreaux & Chau (2022), §3.2.1, C-Var1 rule.
async fn bas_cvar1_rewrite(
    compound_members: &[Type],
    concrete: &Type,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) -> Result<(), TypeError> {
    // Partition into TypeVars and concrete (non-TypeVar) members
    let type_vars: Vec<&Type> = compound_members
        .iter()
        .filter(|m| matches!(m, Type::TypeVar(_, _)))
        .collect();
    let concrete_members: Vec<&Type> = compound_members
        .iter()
        .filter(|m| !matches!(m, Type::TypeVar(_, _)))
        .collect();

    if type_vars.is_empty() {
        // No TypeVars — this shouldn't happen (guard in match arm ensures at least one),
        // but handle gracefully: check subsumption
        if Type::is_subtype(
            concrete,
            &Type::Union(compound_members.to_vec()),
            Some(&state.tycon_env),
        ) {
            return Ok(());
        }
        return Err(TypeError::from(TypeErrorTyped::UnificationFailure(
            UnificationFailure {
                expected: concrete.clone(),
                got: Type::Union(compound_members.to_vec()),
                span,
                notes: vec![],
                call_stack: vec![],
            },
        )));
    }

    // Check if concrete is already covered by the non-var union members.
    // If concrete <: Union(concrete_members), the TypeVar is unconstrained by this equation.
    if !concrete_members.is_empty() {
        let concrete_union = if concrete_members.len() == 1 {
            concrete_members[0].clone()
        } else {
            Type::Union(concrete_members.iter().map(|t| (*t).clone()).collect())
        };
        if Type::is_subtype(concrete, &concrete_union, Some(&state.tycon_env)) {
            // concrete is already a subtype of the non-var part — no binding needed
            return Ok(());
        }
    }

    // Compute the residual: concrete & ~(union of concrete members)
    // This is the "leftover" that the TypeVar must account for.
    let bound_type = if concrete_members.is_empty() {
        // No concrete members in the union — residual is just `concrete` itself
        concrete.clone()
    } else {
        let concrete_union = if concrete_members.len() == 1 {
            concrete_members[0].clone()
        } else {
            Type::Union(concrete_members.iter().map(|t| (*t).clone()).collect())
        };
        // Compute concrete & ~concrete_union
        let residual = Type::Intersection(vec![
            concrete.clone(),
            Type::Negation(Box::new(concrete_union)),
        ]);
        // Simplify: check if residual is equivalent to concrete (when concrete_members
        // are disjoint from concrete, ~concrete_union doesn't remove anything)
        let residual_rdnf = crate::bas::to_rdnf(&residual);
        let mut sigma = std::collections::HashSet::new();
        if crate::bas::is_rdnf_empty(&residual_rdnf, Some(&state.tycon_env), &mut sigma) {
            // Residual is empty — concrete is fully covered. No binding needed.
            return Ok(());
        }
        // Use concrete directly as the bound when the concrete members are disjoint
        // (common case: concrete = Int, concrete_members = [Str] → Int & ~Str = Int)
        if concrete_members
            .iter()
            .all(|m| Type::types_are_disjoint(concrete, m))
        {
            concrete.clone()
        } else {
            // Use the full residual type
            Type::simplify_type(residual)
        }
    };

    // Bind each TypeVar
    for tv in &type_vars {
        let Type::TypeVar(var_name, _) = tv else {
            continue;
        };

        let alpha_level = state.get_level(var_name).unwrap_or(0);
        if lower_levels_check_occurs(&bound_type, var_name, alpha_level, state) {
            return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                message: format!("infinite type: {var_name} occurs in {bound_type}"),
                span,
                notes: vec![],
                call_stack: vec![],
            })));
        }

        if type_vars.len() == 1 {
            // Single TypeVar: bind directly (equational constraint)
            let promoted = promote_literal_for_constrained_var(
                var_name,
                bound_type.clone(),
                constraints,
                state,
            );
            check_constraints_on_var(var_name, &promoted, state, constraints, span.clone()).await?;
            state.bind_type_var(var_name.clone(), promoted);
            return state.check_type_vars_size(span);
        } else {
            // Multiple TypeVars: add as lower bound (inequality constraint)
            state
                .bounds
                .entry(var_name.clone())
                .or_insert_with(crate::bas::TypeVarBounds::new)
                .add_lower(bound_type.clone());
        }
    }

    Ok(())
}

/// [C-VAR2] BAS constraint rewriting for Intersection types containing TypeVars.
///
/// `α ∧ τ₁ ≤ τ₂`  →  `α ≤ ~τ₁ ∨ τ₂`
///
/// Computes the bound `concrete | ~(intersection of concrete members)` and uses it
/// to determine what to bind the TypeVar to.
///
/// Parreaux & Chau (2022), §3.2.1, C-Var2 rule.
async fn bas_cvar2_rewrite(
    compound_members: &[Type],
    concrete: &Type,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) -> Result<(), TypeError> {
    // Partition into TypeVars and concrete (non-TypeVar) members
    let type_vars: Vec<&Type> = compound_members
        .iter()
        .filter(|m| matches!(m, Type::TypeVar(_, _)))
        .collect();
    let concrete_members: Vec<&Type> = compound_members
        .iter()
        .filter(|m| !matches!(m, Type::TypeVar(_, _)))
        .collect();

    if type_vars.is_empty() {
        if Type::is_subtype(
            &Type::Intersection(compound_members.to_vec()),
            concrete,
            Some(&state.tycon_env),
        ) {
            return Ok(());
        }
        return Err(TypeError::from(TypeErrorTyped::UnificationFailure(
            UnificationFailure {
                expected: concrete.clone(),
                got: Type::Intersection(compound_members.to_vec()),
                span,
                notes: vec![],
                call_stack: vec![],
            },
        )));
    }

    // Check if a concrete member already implies the target.
    // If any concrete_member <: concrete, the TypeVar is unconstrained.
    if concrete_members
        .iter()
        .any(|m| Type::is_subtype(m, concrete, Some(&state.tycon_env)))
    {
        return Ok(());
    }

    // The bound for α is: concrete (simplified from ~τ₁ ∨ τ₂ when the intersection
    // members are disjoint from concrete). In practice, for most unification scenarios,
    // binding α = concrete is the principal choice.
    let bound_type = concrete.clone();

    // Bind each TypeVar
    for tv in &type_vars {
        let Type::TypeVar(var_name, _) = tv else {
            continue;
        };

        let alpha_level = state.get_level(var_name).unwrap_or(0);
        if lower_levels_check_occurs(&bound_type, var_name, alpha_level, state) {
            return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                message: format!("infinite type: {var_name} occurs in {bound_type}"),
                span,
                notes: vec![],
                call_stack: vec![],
            })));
        }

        if type_vars.len() == 1 {
            // Single TypeVar: bind directly
            let promoted = promote_literal_for_constrained_var(
                var_name,
                bound_type.clone(),
                constraints,
                state,
            );
            check_constraints_on_var(var_name, &promoted, state, constraints, span.clone()).await?;
            state.bind_type_var(var_name.clone(), promoted);
            return state.check_type_vars_size(span);
        } else {
            // Multiple TypeVars: add as upper bound
            state
                .bounds
                .entry(var_name.clone())
                .or_insert_with(crate::bas::TypeVarBounds::new)
                .add_upper(bound_type.clone());
        }
    }

    Ok(())
}

pub async fn unify(
    a: &Type,
    b: &Type,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) -> Result<(), TypeError> {
    // Apply current substitution to both sides (Robinson step: chase bound vars).
    // Shared visited set avoids redundant allocation across both apply() calls.
    let mut visited_types = HashSet::new();
    let mut visited_rows = HashSet::new(); // kept for apply_with_visited API compatibility
    let a_substituted = state.apply_with_visited(a, &mut visited_types, &mut visited_rows);
    visited_types.clear();
    let b_substituted = state.apply_with_visited(b, &mut visited_types, &mut visited_rows);

    // Normalize both types (for TypeStageApp reduction).
    // allow_eval is set to false inside unify to prevent runtime errors from propagating
    // into type inference (e.g., a failing resolver should produce a stuck TypeStageApp, not
    // a type error).
    let mut norm_ctx =
        crate::type_normalize::NormCtxt::new(state.type_stage_env.clone(), state.eval_ctx.clone());
    norm_ctx.allow_eval = false;
    let a = crate::type_normalize::normalize(&a_substituted, &state.type_vars, &mut norm_ctx).await;
    let b = crate::type_normalize::normalize(&b_substituted, &state.type_vars, &mut norm_ctx).await;
    drop(norm_ctx);

    if a == b {
        return Ok(());
    }

    // Robinson (1965) invariant: after unifying X and Y, `state.type_vars` is extended with at
    // most one new binding (the TypeVar arm inserts exactly one entry). Subsequent calls to
    // `unify` operate on the extended state via the `apply_with_visited` calls at the top of
    // each recursive invocation -- those calls chase the binding chain and return fully-walked
    // types before the match. We do NOT re-apply the substitution to already-unified terms
    // between match arms because (a) the occurs check prevents cycles, so there are no
    // self-referential chains to chase, and (b) each arm receives pre-applied operands (a, b)
    // that are already walk-complete with respect to the bindings at entry time.
    match (&a, &b) {
        // Error absorption: unify(Error, T) = Ok(()) for all T.
        // Error is a sentinel for failed sub-expression inference; absorbing it silently
        // prevents cascade errors in parent expressions. No binding is modified --
        // Error carries no information that should propagate to type variables.
        (Type::Error(_), _) | (_, Type::Error(_)) => Ok(()),

        // Unknown-consistency with level zeroing: prevent generalization of Unknown-touched vars.
        // Unknown relates to other types via consistency, not unification. When Unknown meets
        // a type variable, we zero the variable's level to prevent generalization (Siek & Taha 2006).
        (Type::Unknown, Type::TypeVar(name, _)) => {
            state.set_level(name.clone(), 0);
            Ok(())
        }
        (Type::TypeVar(name, _), Type::Unknown) => {
            state.set_level(name.clone(), 0);
            Ok(())
        }
        (Type::Unknown, other) | (other, Type::Unknown) => {
            // Zero levels of all type/row vars in the non-Unknown side to prevent
            // over-generalization. E.g., unify(Unknown, Fn(TypeVar("b",3) -> Int))
            // must zero b's level so it won't be generalized.
            let mut type_vars = HashSet::new();
            other.collect_all_vars(&mut type_vars);
            for var in &type_vars {
                state.set_level(var.clone(), 0);
            }
            Ok(())
        }

        // Top unification: Top should not appear in unification positions (it's for checking only).
        // If it does appear, treat it like Unknown for now (accepting unification with anything).
        (Type::Any, Type::TypeVar(name, _)) => {
            state.set_level(name.clone(), 0);
            Ok(())
        }
        (Type::TypeVar(name, _), Type::Any) => {
            state.set_level(name.clone(), 0);
            Ok(())
        }
        (Type::Any, other) | (other, Type::Any) => {
            let mut type_vars = HashSet::new();
            other.collect_all_vars(&mut type_vars);
            for var in &type_vars {
                state.set_level(var.clone(), 0);
            }
            Ok(())
        }

        // TypeVar-to-TypeVar unification: bind higher-level var to lower-level var
        // (Kiselyov 2013 L3 invariant — reduces substitution chain length).
        // Skipping `lower_levels_check_occurs` is safe: by binding high→low, the surviving
        // variable already holds the minimum level, so the level-lowering step would be a
        // no-op. No occurs-check is needed because the two variables are distinct — the
        // same-name case (`a == b`) is caught by the early return above.
        (Type::TypeVar(name_a, _), Type::TypeVar(name_b, _)) => {
            let level_a = state.get_level(name_a).unwrap_or(0);
            let level_b = state.get_level(name_b).unwrap_or(0);

            // Bind the higher-level variable to the lower-level one.
            // If levels are equal, bind left-to-right for determinism.
            // Bind the higher-level variable to the lower-level one for determinism.
            if level_a >= level_b {
                // Bind name_a → TypeVar(name_b)
                transfer_class_constraints(name_a, name_b, constraints);
                state.bind_type_var(name_a.clone(), Type::TypeVar(name_b.clone(), level_b));
            } else {
                // Bind name_b → TypeVar(name_a)
                transfer_class_constraints(name_b, name_a, constraints);
                state.bind_type_var(name_b.clone(), Type::TypeVar(name_a.clone(), level_a));
            }
            state.check_type_vars_size(span)?;
            Ok(())
        }

        // U-VAR-LEVEL: bind α to τ, lower levels of all β ∈ FTV(τ) and all ρ ∈ FRV(τ)
        (Type::TypeVar(name, _), _) => {
            // Fused occurs check + level lowering: one tree walk, zero HashSet allocations.
            // lower_levels_check_occurs returns true if `name` appears in the type tree
            // (infinite-type guard), and simultaneously lowers all var levels to cap_level.
            let alpha_level = state.get_level(name).unwrap_or(0);
            if lower_levels_check_occurs(&b, name, alpha_level, state) {
                return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!("infinite type: {name} occurs in {b}"),
                    span,
                    notes: vec![],
                    call_stack: vec![],
                })));
            }
            // Promote literal types when binding a constrained type variable.
            // Without this, `[+ 1 2]` would bind _t0 to IntLiteral(1) and then fail
            // to unify IntLiteral(1) with IntLiteral(2) for the second argument.
            let b = promote_literal_for_constrained_var(name, b, constraints, state);

            // CONSTRAINT TRANSFER: when binding α to β (both TypeVars or Operator), transfer Class
            // constraints from α to β instead of checking. β inherits α's obligations and will be
            // checked when β is bound to a concrete type. HasField constraints are NOT transferred
            // (they reference the dict variable, not the param).
            // bind_at_level routes the binding to the frame matching the TypeVar's creation level.
            if let Type::TypeVar(beta_name, _) = &b {
                transfer_class_constraints(name, beta_name, constraints);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                state.bind_type_var(name.clone(), b);
            } else if let Type::Operator(beta_name) = &b {
                transfer_class_constraints(name, beta_name, constraints);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                state.bind_type_var(name.clone(), b);
            } else {
                // Binding α to a concrete type — check constraints normally
                check_constraints_on_var(name, &b, state, constraints, span.clone()).await?;
                state.bind_type_var(name.clone(), b);
            }
            state.check_type_vars_size(span)?;
            Ok(())
        }
        // U-VAR-LEVEL-SYM: bind α to τ, lower levels of all β ∈ FTV(τ) and all ρ ∈ FRV(τ)
        (_, Type::TypeVar(name, _)) => {
            // Fused occurs check + level lowering: one tree walk, zero HashSet allocations.
            let alpha_level = state.get_level(name).unwrap_or(0);
            if lower_levels_check_occurs(&a, name, alpha_level, state) {
                return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!("infinite type: {name} occurs in {a}"),
                    span,
                    notes: vec![],
                    call_stack: vec![],
                })));
            }
            // Promote literal types when binding a constrained type variable.
            let a = promote_literal_for_constrained_var(name, a, constraints, state);

            // CONSTRAINT TRANSFER: when binding α to β (both TypeVars or Operator), transfer Class
            // constraints from α to β instead of checking. β inherits α's obligations and will be
            // checked when β is bound to a concrete type. HasField constraints are NOT transferred
            // (they reference the dict variable, not the param).
            // bind_at_level routes the binding to the frame matching the TypeVar's creation level.
            if let Type::TypeVar(beta_name, _) = &a {
                transfer_class_constraints(name, beta_name, constraints);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                state.bind_type_var(name.clone(), a);
            } else if let Type::Operator(beta_name) = &a {
                transfer_class_constraints(name, beta_name, constraints);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                state.bind_type_var(name.clone(), a);
            } else {
                // Binding α to a concrete type — check constraints normally
                check_constraints_on_var(name, &a, state, constraints, span.clone()).await?;
                state.bind_type_var(name.clone(), a);
            }
            state.check_type_vars_size(span)?;
            Ok(())
        }

        // S-861: equirecursive-checker — Recursive type unification arms.
        //
        // ORDERING IS CRITICAL: these three arms must appear AFTER the TypeVar arms above
        // (U-VAR-LEVEL and U-VAR-LEVEL-SYM) and BEFORE the structural arms below.
        //
        // Without this ordering, `unify(Recursive{..}, TypeVar)` would hit Arm 4
        // (open-left), unfold the Recursive, and bind the TypeVar to the opened body —
        // losing the recursive structure. With the correct ordering, the TypeVar arm above
        // fires first and binds the TypeVar to the full `Type::Recursive` value.
        //
        // Termination argument (Pierce 2002 §21.8 simultaneous-opening):
        //  - Arm 3 (Recursive+Recursive): after substitute_recvar, the binder positions
        //    in both opened bodies hold `fresh` (a TypeVar), not another Recursive. When
        //    unification descends to those positions, a TypeVar arm fires and binds —
        //    no further Recursive arm fires on that branch.
        //  - Arms 4 and 5 (asymmetric): substitute_recvar replaces `TypeVar(var, 0)` with
        //    `fresh`. All former recursive positions now hold a TypeVar. When unification
        //    descends and encounters `fresh` paired against any type, a TypeVar arm fires
        //    and binds — no further Recursive arm fires on that branch.
        //  - `other` may contain Recursive sub-terms (e.g., `Union([Recursive{v3,...}, Int])`).
        //    Those are handled with fresh TypeVars for their own binder (v3), not the current
        //    one. Structural induction: each arm firing eliminates one Recursive from the top
        //    of one side without re-introducing Recursive at the top of either side.

        // Arm 3 (Recursive+Recursive, symmetric): open BOTH sides with ONE shared fresh TypeVar.
        //
        // Why one shared var, not two sequential opens? Two sequential opens produce
        // `fresh_a → fresh_b` in the substitution — an extra indirection that surfaces
        // in error messages and type display. The shared fresh var produces a direct result:
        // `μ_t0.T[_t0]` where `_t0` is immediately the representative. Both approaches
        // produce the same principal type; the shared fresh var is more direct.
        (Type::Recursive { var: va, body: ba }, Type::Recursive { var: vb, body: bb }) => {
            let fresh = state.fresh_type_var(&span);
            let opened_a = substitute_recvar(ba, va, &fresh);
            let opened_b = substitute_recvar(bb, vb, &fresh);
            Box::pin(unify(&opened_a, &opened_b, state, constraints, span)).await
        }

        // Arm 4 (open-left): left is Recursive, right is a concrete type (not TypeVar — that
        // was caught by the U-VAR-LEVEL-SYM arm above; not Recursive — caught by Arm 3 above).
        // Open the left side with a fresh TypeVar and unify the opened body with the right.
        (Type::Recursive { var: va, body: ba }, _) => {
            let fresh = state.fresh_type_var(&span);
            let opened_a = substitute_recvar(ba, va, &fresh);
            Box::pin(unify(&opened_a, &b, state, constraints, span)).await
        }

        // Arm 5 (open-right): right is Recursive, left is a concrete type (not TypeVar — caught
        // above; not Recursive — caught by Arm 3 above).
        // Open the right side with a fresh TypeVar and unify with the left.
        (_, Type::Recursive { var: vb, body: bb }) => {
            let fresh = state.fresh_type_var(&span);
            let opened_b = substitute_recvar(bb, vb, &fresh);
            Box::pin(unify(&a, &opened_b, state, constraints, span)).await
        }

        // Literal type promotion shortcuts (performance optimization over U-SUBSUME).
        //
        // These arms are logically redundant with [U-SUBSUME] at the bottom of this match:
        //   - IntLiteral(_) <: Int <: Number via S-NEVER/is_subtype, so U-SUBSUME passes both ways
        //   - Float <: Number via is_subtype, so U-SUBSUME passes
        //   - StringLiteral(_) <: Str via is_subtype, so U-SUBSUME passes
        //
        // They are retained as explicit fast-paths to avoid the has_inference_vars() guards
        // on [U-SUBSUME], which would be a minor overhead for very common unification patterns
        // (e.g., integer literals in arithmetic expressions).
        //
        // SEMANTIC NOTE: unify(Int, IntLiteral(42)) succeeds here (and via U-SUBSUME bidirectional
        // check). This is correct: is_subtype(IntLiteral(42), Int) = true, so U-SUBSUME's
        // `is_subtype(&b, &a)` arm fires. The "bidirectionality" is from the symmetric U-SUBSUME
        // check, not from asserting Int = IntLiteral(42) — Int is the wider type, IntLiteral(42)
        // is the narrower, and subtyping allows the narrower to satisfy the wider.
        (Type::IntLiteral(_), Type::Int) | (Type::Int, Type::IntLiteral(_)) => Ok(()),
        (Type::StringLiteral(_), Type::Str) | (Type::Str, Type::StringLiteral(_)) => Ok(()),
        // Same-value literals: covered by the `a == b` early-return above.
        // Different-value INTEGER literals: allow unification without rebinding.
        //
        // DESIGN NOTE (B-384): This arm is intentionally a no-op (no TypeVar rebinding)
        // rather than a widening to Int. The reason: by the time two distinct IntLiterals
        // reach this arm, the TypeVar that introduced the binding (e.g., `_t42` bound to
        // IntLiteral(1) from the first argument) has already been committed in the
        // substitution. Widening it here would require knowing which TypeVar introduced
        // IntLiteral(n1) — that information is not available at this match site.
        //
        // What this arm DOES provide: it prevents false UnificationFailure warnings for
        // call sites like `[= [x: 1] [x: 2]]` where the same polymorphic TypeVar is
        // unified against two different integer literals. The arm succeeds (Ok(()))
        // without changing the substitution, which is correct for the single-arg-call
        // scenario (the TypeVar stays at IntLiteral(1), the narrower type). For the
        // two-different-literal case, the arm prevents a spurious warning at the cost of
        // not widening to Int — acceptable since the subsequent call-site check at
        // check_constraints_on_var uses widen_literal_types to recover Int precision.
        //
        // StringLiteral values are NOT handled here because they serve as field labels in
        // Indexable FD resolution and must remain distinct across unification sites.
        (Type::IntLiteral(n1), Type::IntLiteral(n2)) if n1 != n2 => Ok(()),
        (
            Type::Function {
                params: p1,
                ret: r1,
                variadic: v1,
                required_count: _,
            },
            Type::Function {
                params: p2,
                ret: r2,
                variadic: v2,
                required_count: _,
            },
        ) => {
            // Special case: zero-param variadic is the "any function" type.
            // Function{params:[], ret:Unknown, variadic:true} unifies with any function that
            // has at least one parameter (concrete arity). It does NOT unify with zero-param
            // non-variadic (different semantics: zero-param variadic accepts any args, zero-param
            // non-variadic accepts exactly zero args).
            // This enables precise function type predicate narrowing (fn-narrowing-variadic sprint).
            let is_any_function_1 = p1.is_empty() && *v1;
            let is_any_function_2 = p2.is_empty() && *v2;

            // Apply special case when one side is zero-param variadic and the other has params.
            if is_any_function_1 && !p2.is_empty() {
                // Zero-param variadic unifies with any concrete-arity function.
                return Box::pin(unify(r1, r2, state, constraints, span)).await;
            }
            if is_any_function_2 && !p1.is_empty() {
                // Zero-param variadic unifies with any concrete-arity function (symmetric).
                return Box::pin(unify(r1, r2, state, constraints, span)).await;
            }

            if p1.len() != p2.len() {
                return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!(
                        "arity mismatch: expected {} arguments, got {}",
                        p1.len(),
                        p2.len()
                    ),
                    span,
                    notes: vec![],
                    call_stack: vec![],
                })));
            }
            if v1 != v2 {
                return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!(
                        "variadic mismatch: {} vs {}",
                        if *v1 { "variadic" } else { "non-variadic" },
                        if *v2 { "variadic" } else { "non-variadic" }
                    ),
                    span,
                    notes: vec![],
                    call_stack: vec![],
                })));
            }
            // Robinson invariant: sub-terms are passed without explicit apply() because
            // every recursive unify() call re-applies the accumulated substitution at its
            // own entry (via apply_with_visited at the top of this function). Bindings
            // from earlier parameter unifications are therefore visible to later ones via
            // the shared `subst` -- this is correct Robinson (1965) unification.
            for ((_name_a, ty_a), (_name_b, ty_b)) in p1.iter().zip(p2.iter()) {
                Box::pin(unify(ty_a, ty_b, state, constraints, span.clone())).await?;
            }
            Box::pin(unify(r1, r2, state, constraints, span)).await
        }

        (Type::Proxy, Type::Proxy) => Ok(()),

        // Never unification: Never (⊥) unifies with any type — sound because Never is the
        // bottom type and no value can inhabit it. Any constraint involving Never is vacuously
        // satisfiable: if a branch is unreachable (type Never), it cannot produce a value that
        // would violate a type constraint. This is dual to Top (⊤) which absorbs all via subtyping.
        //
        // This is correct unification behavior, NOT a soundness hole:
        //   - S-NEVER: Never <: T for all T (Never is a subtype of everything)
        //   - U-SUBSUME would also allow this (is_subtype(Never, T) = true)
        //   - TypeVar arms above (lines 925, 947) handle Never vs TypeVar correctly
        //
        // KNOWN LIMITATION: unify(Never, Int) succeeds silently, meaning a Never-typed value
        // "appears" to have type Int through unification. In practice this is fine because Never
        // values cannot be constructed — any code that produces Never is dead code. If you want
        // to detect dead-code branches at the type level, use is_subtype(scrutinee, Never) instead.
        (Type::Never, _) | (_, Type::Never) => Ok(()),

        // Negation unification: structural (for now, basic support)
        (Type::Negation(t1), Type::Negation(t2)) => {
            Box::pin(unify(t1, t2, state, constraints, span)).await
        }

        // Negation disjointness: if T <: A, then T & ~A = Never (provably empty intersection).
        // We can statically reject this case without full RDNF normalization — if is_subtype(T, A)
        // holds, the intersection is provably Never. For all other cases (uncertain overlap), we
        // remain conservative and allow unification to succeed. Runtime value_matches_type handles
        // the residual constraint for `[@[[without T]] expr]` TypeAsserts.
        (concrete, Type::Negation(inner))
            if !matches!(concrete, Type::TypeVar(..) | Type::Unknown) =>
        {
            if Type::is_subtype(concrete, inner, Some(&state.tycon_env)) {
                Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!(
                        "cannot unify {} with ~{}: intersection is Never (T <: A implies T & ~A = \u{2205})",
                        concrete, inner
                    ),
                    span,
                    notes: vec![], call_stack: vec![],
                })))
            } else {
                Ok(()) // conservative: may still be empty but can't prove it statically
            }
        }
        (Type::Negation(inner), concrete)
            if !matches!(concrete, Type::TypeVar(..) | Type::Unknown) =>
        {
            if Type::is_subtype(concrete, inner, Some(&state.tycon_env)) {
                Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!(
                        "cannot unify ~{} with {}: intersection is Never (T <: A implies T & ~A = \u{2205})",
                        inner, concrete
                    ),
                    span,
                    notes: vec![], call_stack: vec![],
                })))
            } else {
                Ok(()) // conservative: may still be empty but can't prove it statically
            }
        }
        // Fallback: TypeVar or Unknown against Negation — defer to runtime
        (_, Type::Negation(_)) | (Type::Negation(_), _) => Ok(()),

        // Capability types: reflexive unification only
        (Type::DirCap, Type::DirCap) => Ok(()),
        (Type::NetCap, Type::NetCap) => Ok(()),
        (Type::DatagramHandle, Type::DatagramHandle) => Ok(()),
        (Type::QuicDatagramHandle, Type::QuicDatagramHandle) => Ok(()),
        // UNIFY-TYCON: two TyCons unify iff they refer to the same type constructor definition.
        // Distinct named constructors are distinct types regardless of arity or structure.
        //
        // Name equality is checked first. When names match, Arc::ptr_eq checks pointer identity:
        // if two `[type Foo ...]` declarations in different scopes both registered under "Foo",
        // they produce distinct Arc<TyConDef> values. If the current tycon_env has already been
        // overwritten by a shadowing declaration (inner scope wins), both lookups return the same
        // Arc — so they correctly unify. If the two TyCons came from different module contexts
        // with disjoint tycon_envs, Arc::ptr_eq correctly rejects the unification.
        //
        // NOTE: `Type::TyCon` carries only a name string. A flat tycon_env (single entry per name)
        // means that two lookups for the same name always return the same Arc in the current env,
        // so Arc::ptr_eq is always true for name-equal TyCons in a single type-checking pass.
        // The ptr_eq check becomes meaningful in future work where TyCon carries Arc identity
        // directly (e.g., `Type::TyCon(Arc<TyConDef>)`), eliminating name-based lookup ambiguity.
        (Type::TyCon(n1), Type::TyCon(n2)) => {
            if n1 != n2 {
                return Err(TypeError::from(TypeErrorTyped::UnificationFailure(
                    UnificationFailure {
                        expected: a.clone(),
                        got: b.clone(),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    },
                )));
            }
            let def1 = state.tycon_env.get(n1.as_str());
            let def2 = state.tycon_env.get(n2.as_str());
            match (def1, def2) {
                (Some(d1), Some(d2)) if !Arc::ptr_eq(d1, d2) => {
                    // Same name but different TyConDef objects — cross-scope shadowing.
                    let loc1 = d1
                        .definition_span
                        .as_ref()
                        .map(|s| {
                            format!(
                                " (defined at {}:{}:{})",
                                s.file.path, s.start.line, s.start.column
                            )
                        })
                        .unwrap_or_default();
                    let loc2 = d2
                        .definition_span
                        .as_ref()
                        .map(|s| {
                            format!(
                                " (defined at {}:{}:{})",
                                s.file.path, s.start.line, s.start.column
                            )
                        })
                        .unwrap_or_default();
                    Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                        message: format!(
                            "type constructor '{n1}' refers to two distinct definitions: \
                             {n1}{loc1} vs {n1}{loc2}"
                        ),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    })))
                }
                _ => {
                    // Both None (unknown TyCon), or same Arc (same definition) — unify.
                    Ok(())
                }
            }
        }

        // UNIFY-TYCON-EXPAND: TyCon unified with a NominalVariant.
        //
        // When a zero-arity TyCon (e.g., `@Color`) is unified with a NominalVariant value
        // (e.g., a value of type `NominalVariant{tag:"Color.Red", fields:{}}`), check that the
        // NominalVariant is a subtype of the TyCon's body (the union of NominalVariants).
        //
        // We use `is_subtype` rather than a recursive `unify` call here because:
        // 1. Both the body (Union of NominalVariants) and the NominalVariant are concrete (no
        //    inference variables) — no variable binding is needed.
        // 2. The recursive `unify` path would hit the C-Var1 arm for `(Union(concrete_members),
        //    concrete)` which requires exactly 1 TypeVar in the union; without TypeVars it errors.
        //    `is_subtype` correctly checks `NominalVariant <: Union([...])` via [UNION-INJ] rules.
        //
        // If the TyConDef has no registered body (unknown or builtin TyCon), the TyCon is opaque
        // and cannot unify with NominalVariant — produce a type mismatch.
        (Type::TyCon(n), Type::NominalVariant { .. }) => {
            if let Some(def) = state.tycon_env.get(n.as_str()) {
                let body = def.body.clone();
                let tycon_env = Some(&state.tycon_env);
                // Check that the NominalVariant (b) is a subtype of the body (the union).
                // Unidirectional: NominalVariant <: body only — the reverse is unsound.
                // is_subtype(body, NominalVariant) would accept any variant matching one constructor
                // of the union even from a different TyCon family (no tag-equality check in that path).
                if Type::is_subtype(&b, &body, tycon_env) {
                    Ok(())
                } else {
                    Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                        message: format!(
                            "cannot unify nominal variant with type '{}': variant is not a member of '{}'",
                            n, n
                        ),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    })))
                }
            } else {
                Err(TypeError::from(TypeErrorTyped::UnificationFailure(
                    UnificationFailure {
                        expected: a.clone(),
                        got: b.clone(),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    },
                )))
            }
        }
        (Type::NominalVariant { .. }, Type::TyCon(n)) => {
            if let Some(def) = state.tycon_env.get(n.as_str()) {
                let body = def.body.clone();
                let tycon_env = Some(&state.tycon_env);
                if Type::is_subtype(&a, &body, tycon_env) {
                    Ok(())
                } else {
                    Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                        message: format!(
                            "cannot unify nominal variant with type '{}': variant is not a member of '{}'",
                            n, n
                        ),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    })))
                }
            } else {
                Err(TypeError::from(TypeErrorTyped::UnificationFailure(
                    UnificationFailure {
                        expected: a.clone(),
                        got: b.clone(),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    },
                )))
            }
        }

        // UNIFY-TYCON-UNION: TyCon unified with a Union of NominalVariants.
        //
        // `@Boolean ~ Union([Boolean.True, Boolean.False])`: succeeds when every union member
        // is a subtype of the TyCon's declared body. This is the subset direction — the union
        // must be covered entirely by the TyCon's constructor family.
        //
        // Symmetric: both (TyCon, Union) and (Union, TyCon) are handled here.
        // Guard: only fire when the union has no TypeVars. If it has TypeVars, fall through
        // to C-Var1 which handles constraint rewriting for inference variables.
        (Type::TyCon(n), Type::Union(members))
            if !members.iter().any(|m| matches!(m, Type::TypeVar(_, _))) =>
        {
            if let Some(def) = state.tycon_env.get(n.as_str()) {
                let body = def.body.clone();
                let tycon_env = Some(&state.tycon_env);
                if members
                    .iter()
                    .all(|m| Type::is_subtype(m, &body, tycon_env))
                {
                    Ok(())
                } else {
                    Err(TypeError::from(TypeErrorTyped::UnificationFailure(
                        UnificationFailure {
                            expected: a.clone(),
                            got: b.clone(),
                            span,
                            notes: vec![],
                            call_stack: vec![],
                        },
                    )))
                }
            } else {
                Err(TypeError::from(TypeErrorTyped::UnificationFailure(
                    UnificationFailure {
                        expected: a.clone(),
                        got: b.clone(),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    },
                )))
            }
        }
        (Type::Union(members), Type::TyCon(n))
            if !members.iter().any(|m| matches!(m, Type::TypeVar(_, _))) =>
        {
            if let Some(def) = state.tycon_env.get(n.as_str()) {
                let body = def.body.clone();
                let tycon_env = Some(&state.tycon_env);
                if members
                    .iter()
                    .all(|m| Type::is_subtype(m, &body, tycon_env))
                {
                    Ok(())
                } else {
                    Err(TypeError::from(TypeErrorTyped::UnificationFailure(
                        UnificationFailure {
                            expected: a.clone(),
                            got: b.clone(),
                            span,
                            notes: vec![],
                            call_stack: vec![],
                        },
                    )))
                }
            } else {
                Err(TypeError::from(TypeErrorTyped::UnificationFailure(
                    UnificationFailure {
                        expected: a.clone(),
                        got: b.clone(),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    },
                )))
            }
        }

        // UNIFY-OPERATOR-TO-OPERATOR: bind higher-level Operator to lower-level Operator.
        // Follows Kiselyov L3 invariant (same as TypeVar-to-TypeVar at lines 1837-1860).
        (Type::Operator(m), Type::Operator(n)) if m != n => {
            let level_m = state.get_level(m).unwrap_or(0);
            let level_n = state.get_level(n).unwrap_or(0);

            // Bind the higher-level operator to the lower-level one.
            // If levels are equal, bind left-to-right for determinism.
            if level_m >= level_n {
                // Bind m → Operator(n)
                transfer_class_constraints(m, n, constraints);
                state.bind_type_var(m.clone(), Type::Operator(n.clone()));
            } else {
                // Bind n → Operator(m)
                transfer_class_constraints(n, m, constraints);
                state.bind_type_var(n.clone(), Type::Operator(m.clone()));
            }
            state.check_type_vars_size(span)?;
            Ok(())
        }

        // UNIFY-OPERATOR: bind type constructor variable m to a type T.
        // Occurs check prevents infinite kinds (m ∉ ftv(T)).
        // Kind check premise is deferred to hkt-kind-inference.
        (Type::Operator(m), _) => {
            // Fused occurs check + level lowering (Kiselyov L3 invariant for Operator variables)
            let alpha_level = state.get_level(m).unwrap_or(0);
            if lower_levels_check_occurs(&b, m, alpha_level, state) {
                return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!("infinite type: operator variable {} occurs in {}", m, b),
                    span,
                    notes: vec![],
                    call_stack: vec![],
                })));
            }
            // CONSTRAINT TRANSFER: when binding m to TypeVar, transfer constraints
            // instead of checking. When binding to a concrete type, check constraints normally.
            // bind_at_level routes to the frame matching the Operator variable's creation level.
            if let Type::TypeVar(beta_name, _) = &b {
                transfer_class_constraints(m, beta_name, constraints);
                state.bind_type_var(m.clone(), b.clone());
            } else {
                // Binding to concrete type — check constraints
                check_constraints_on_var(m, &b, state, constraints, span.clone()).await?;
                state.bind_type_var(m.clone(), b.clone());
            }
            state.check_type_vars_size(span)?;
            Ok(())
        }
        // UNIFY-OPERATOR-SYM: symmetric case
        (_, Type::Operator(m)) => {
            // Fused occurs check + level lowering (Kiselyov L3 invariant for Operator variables)
            let alpha_level = state.get_level(m).unwrap_or(0);
            if lower_levels_check_occurs(&a, m, alpha_level, state) {
                return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!("infinite type: operator variable {} occurs in {}", m, a),
                    span,
                    notes: vec![],
                    call_stack: vec![],
                })));
            }
            // CONSTRAINT TRANSFER: when binding m to TypeVar, transfer constraints
            // instead of checking. When binding to a concrete type, check constraints normally.
            // bind_at_level routes to the frame matching the Operator variable's creation level.
            if let Type::TypeVar(beta_name, _) = &a {
                transfer_class_constraints(m, beta_name, constraints);
                state.bind_type_var(m.clone(), a.clone());
            } else {
                // Binding to concrete type — check constraints
                check_constraints_on_var(m, &a, state, constraints, span.clone()).await?;
                state.bind_type_var(m.clone(), a.clone());
            }
            state.check_type_vars_size(span)?;
            Ok(())
        }

        // UNIFY-APP: decompose App(f₁, a₁) vs App(f₂, a₂).
        //
        // When both sides are TyCon spines of the same constructor, applies variance-directed
        // unification per parameter position declared in TyConDef.variance:
        //   - Covariant:     standard unify (U-SUBSUME allowed for ground types)
        //   - Invariant:     strict — TypeVar binding allowed, but no subsumption for ground types
        //   - Contravariant: standard unify with arguments swapped
        //   - Phantom:       always succeeds (argument doesn't affect type safety)
        //
        // This eliminates the need for constructor-specific arms (formerly UNIFY-MAP for Map)
        // and makes the runtime agnostic to which constructors exist.
        (Type::App(f1, a1), Type::App(f2, a2)) => {
            // Attempt variance-directed unification for TyCon spine forms.
            if let (Some((name1, args1)), Some((name2, args2))) =
                (extract_tycon_spine(&a), extract_tycon_spine(&b))
            {
                if name1 == name2 && args1.len() == args2.len() {
                    if let Some(def) = state.tycon_env.get(name1).cloned() {
                        for (i, (arg_a, arg_b)) in args1.iter().zip(args2.iter()).enumerate() {
                            let var = def.variance.get(i).copied().unwrap_or(Variance::Invariant);
                            match var {
                                Variance::Covariant => {
                                    Box::pin(unify(arg_a, arg_b, state, constraints, span.clone()))
                                        .await?;
                                }
                                Variance::Contravariant => {
                                    Box::pin(unify(arg_b, arg_a, state, constraints, span.clone()))
                                        .await?;
                                }
                                Variance::Invariant => {
                                    // Invariant: bind TypeVars, but reject ground-type subsumption.
                                    // U-SUBSUME must not fire here — Int and Number are distinct
                                    // invariant positions and must not unify via subtyping.
                                    let ra = state.apply(arg_a);
                                    let rb = state.apply(arg_b);
                                    if ra.has_inference_vars() || rb.has_inference_vars() {
                                        Box::pin(unify(&ra, &rb, state, constraints, span.clone()))
                                            .await?;
                                    } else if ra != rb {
                                        return Err(TypeError::from(
                                            TypeErrorTyped::UnificationFailure(
                                                UnificationFailure {
                                                    expected: ra,
                                                    got: rb,
                                                    span: span.clone(),
                                                    notes: vec![format!(
                                                    "type argument {} of {} must match exactly \
                                                     (invariant position)",
                                                    i + 1,
                                                    name1
                                                )],
                                                    call_stack: vec![],
                                                },
                                            ),
                                        ));
                                    }
                                }
                                Variance::Phantom => {}
                            }
                        }
                        return Ok(());
                    }
                }
            }
            // Fallback for non-TyCon App forms or unknown constructors: structural recursion.
            Box::pin(unify(f1, f2, state, constraints, span.clone())).await?;
            Box::pin(unify(a1, a2, state, constraints, span)).await
        }

        // Record unification: delegate to row unification
        (Type::Dict(row1), Type::Dict(row2)) => {
            unify_rows(row1, row2, state, constraints, span).await
        }

        (
            Type::NominalVariant {
                tycon: tycon1,
                ctor: ctor1,
                fields: fields1,
            },
            Type::NominalVariant {
                tycon: tycon2,
                ctor: ctor2,
                fields: fields2,
            },
        ) => {
            if tycon1 != tycon2 || ctor1 != ctor2 {
                return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!(
                        "cannot unify nominal variants with different tags: {}.{} and {}.{}",
                        tycon1, ctor1, tycon2, ctor2
                    ),
                    span,
                    notes: vec![],
                    call_stack: vec![],
                })));
            }
            unify_rows(fields1, fields2, state, constraints, span).await
        }

        (Type::NominalVariant { tycon, ctor, .. }, Type::Dict(_)) => {
            Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                message: format!(
                    "cannot unify nominal variant {}.{} with structural record",
                    tycon, ctor
                ),
                span,
                notes: vec![],
                call_stack: vec![],
            })))
        }
        (Type::Dict(_), Type::NominalVariant { tycon, ctor, .. }) => {
            Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                message: format!(
                    "cannot unify structural record with nominal variant {}.{}",
                    tycon, ctor
                ),
                span,
                notes: vec![],
                call_stack: vec![],
            })))
        }

        // Record ↔ Intersection-of-Records unification.
        //
        // When a concrete Record is unified against an Intersection whose members are all
        // Records (the shape produced by multi-field `@[x: Int  y: String]` annotations),
        // unify the record against EACH member in turn.  Row unification will bind each
        // member's open row variable to absorb the extra fields present in the concrete
        // record, satisfying the width-subtyping requirement for open rows.
        //
        // This arm is placed BEFORE the C-Var2 patterns because those patterns require
        // `!concrete.has_inference_vars()`, which would not fire for intersections whose
        // members contain RowVar tails (which are inference variables).  Additionally,
        // C-Var2 only handles intersections with exactly one TypeVar — not the all-Record
        // intersection shape we are handling here.
        (Type::Dict(_), Type::Intersection(members))
            if members.iter().all(|m| matches!(m, Type::Dict(_))) =>
        {
            let members = members.clone();
            for member in &members {
                Box::pin(unify(&a, member, state, constraints, span.clone())).await?;
            }
            Ok(())
        }
        (Type::Intersection(members), Type::Dict(_))
            if members.iter().all(|m| matches!(m, Type::Dict(_))) =>
        {
            let members = members.clone();
            for member in &members {
                Box::pin(unify(member, &b, state, constraints, span.clone())).await?;
            }
            Ok(())
        }

        // Union vs Union: defer when both sides have inference vars (TypeVars, row vars, TypeStageApp).
        // This prevents hard errors from Union([Int, TypeVar(a)]) ~ Union([Str, TypeVar(b)]).
        // Conservative approximation: the constraint is dropped if TypeVars get bound elsewhere
        // through other unification paths. process_deferred_equalities in typecheck_dict.rs
        // retries these after each SCC's substitution merge.
        (Type::Union(m1), Type::Union(m2))
            if m1.iter().any(|ty| ty.has_inference_vars())
                || m2.iter().any(|ty| ty.has_inference_vars()) =>
        {
            state.deferred_equalities.push((a.clone(), b.clone()));
            Ok(())
        }

        // [C-VAR1] (BAS constraint rewriting — Parreaux & Chau 2022, §3.2.1):
        //
        //   τ₁ ≤ τ₂ ∨ α  →  τ₁ & ~τ₂ ≤ α
        //
        // When unifying `concrete ~ Union(members)` where the union contains TypeVars:
        // 1. Partition members into TypeVars and concrete members.
        // 2. Compute the "residual" bound: `concrete & ~(union of concrete members)`.
        //    This is the part of `concrete` not already covered by the union's non-var members.
        // 3. If the residual simplifies to Never (all of concrete is covered), done — no binding.
        // 4. If exactly one TypeVar, bind it to the residual (or concrete if residual = concrete).
        // 5. If multiple TypeVars, add the residual as a lower bound of each.
        //
        // Pattern: Union on one side containing at least one TypeVar
        (concrete, Type::Union(members))
            if !concrete.has_inference_vars()
                && members.iter().any(|m| matches!(m, Type::TypeVar(_, _))) =>
        {
            let members = members.clone();
            bas_cvar1_rewrite(&members, concrete, state, constraints, span).await
        }

        // Symmetric C-Var1: Union on the left, concrete on the right
        (Type::Union(members), concrete)
            if !concrete.has_inference_vars()
                && members.iter().any(|m| matches!(m, Type::TypeVar(_, _))) =>
        {
            let members = members.clone();
            bas_cvar1_rewrite(&members, concrete, state, constraints, span).await
        }

        // [C-VAR2] (BAS constraint rewriting — Parreaux & Chau 2022, §3.2.1):
        //
        //   α ∧ τ₁ ≤ τ₂  →  α ≤ ~τ₁ ∨ τ₂
        //
        // When unifying `concrete ~ Intersection(members)` where the intersection has TypeVars:
        // 1. Partition members into TypeVars and concrete members.
        // 2. Compute the bound: `concrete | ~(intersection of concrete members)`.
        // 3. If exactly one TypeVar, bind it to concrete (simplified case when concrete members
        //    already imply the constraint).
        // 4. Otherwise add as upper bound.
        //
        // Pattern: Intersection on one side containing at least one TypeVar
        (Type::Intersection(members), concrete)
            if !concrete.has_inference_vars()
                && members.iter().any(|m| matches!(m, Type::TypeVar(_, _))) =>
        {
            let members = members.clone();
            bas_cvar2_rewrite(&members, concrete, state, constraints, span).await
        }

        // Symmetric C-Var2: concrete on the left, Intersection on the right
        (concrete, Type::Intersection(members))
            if !concrete.has_inference_vars()
                && members.iter().any(|m| matches!(m, Type::TypeVar(_, _))) =>
        {
            let members = members.clone();
            bas_cvar2_rewrite(&members, concrete, state, constraints, span).await
        }

        // TypeStageApp unification cases (after normalization in chr-normalization sprint).
        // Case 1: same function name -> pairwise unify args
        (
            Type::TypeStageApp {
                fn_name: f1,
                args: a1,
            },
            Type::TypeStageApp {
                fn_name: f2,
                args: a2,
            },
        ) if f1 == f2 => {
            if a1.len() != a2.len() {
                return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!(
                        "TypeStageApp arity mismatch: {} expects {} args, got {}",
                        f1,
                        a1.len(),
                        a2.len()
                    ),
                    span,
                    notes: vec![],
                    call_stack: vec![],
                })));
            }
            for (arg1, arg2) in a1.iter().zip(a2.iter()) {
                Box::pin(unify(arg1, arg2, state, constraints, span.clone())).await?;
            }
            Ok(())
        }
        // Case 2: different function names -> error
        (Type::TypeStageApp { fn_name: f1, .. }, Type::TypeStageApp { fn_name: f2, .. }) => {
            Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                message: format!(
                    "cannot unify TypeStageApp with different resolvers: {} vs {}",
                    f1, f2
                ),
                span,
                notes: vec![],
                call_stack: vec![],
            })))
        }
        // Case 3: TypeStageApp vs concrete (non-TypeVar, non-Unknown, non-Top)
        // Defer to process_deferred_equalities (no resolvers available yet in chr-normalization)
        // In chr-prelude, this will attempt resolver evaluation before deferring
        (Type::TypeStageApp { .. }, concrete) | (concrete, Type::TypeStageApp { .. })
            if !matches!(concrete, Type::TypeVar(..) | Type::Unknown | Type::Any) =>
        {
            state.deferred_equalities.push((a.clone(), b.clone()));
            Ok(())
        }

        // [U-SUBSUME]: concrete type subsumption fallback (Pierce & Turner 2000)
        // When both sides are ground types (no type variables), check directed subtyping:
        // a (actual) must be a subtype of b (expected). Unidirectional — the caller is
        // responsible for swapping arguments in contravariant positions.
        // The substitution is not modified (no variables to bind).
        _ if !a.has_inference_vars() && !b.has_inference_vars() => {
            if Type::is_subtype(&a, &b, Some(&state.tycon_env)) {
                Ok(())
            } else {
                Err(TypeError::from(TypeErrorTyped::UnificationFailure(
                    UnificationFailure {
                        expected: b.clone(),
                        got: a.clone(),
                        span,
                        notes: vec![],
                        call_stack: vec![],
                    },
                )))
            }
        }

        _ => Err(TypeError::from(TypeErrorTyped::UnificationFailure(
            UnificationFailure {
                expected: a.clone(),
                got: b.clone(),
                span,
                notes: vec![],
                call_stack: vec![],
            },
        ))),
    }
}

/// Directional subtype constraint: `sub <: sup`.
///
/// Unlike `unify()` (symmetric equality), `constrain()` is directional:
/// `sub` is the inferred type (on the left) and `sup` is the expected/annotated type (on the right).
/// This distinction is critical for principal type inference:
///
/// - **C-Var1** fires when `sup` is a Union containing a TypeVar (`τ₁ ≤ τ₂ ∨ α`):
///   rewrites to `τ₁ & ~τ₂ ≤ α`, accumulating a lower bound on the TypeVar.
///
/// - **C-Var2** fires when `sub` is an Intersection containing a TypeVar (`α ∧ τ₁ ≤ τ₂`):
///   rewrites to `α ≤ ~τ₁ ∨ τ₂`, accumulating an upper bound on the TypeVar.
///
/// - When `sup` is a bare TypeVar `α`, `sub` is accumulated as a lower bound of `α`.
///
/// - When `sub` is a bare TypeVar `α`, `sup` is accumulated as an upper bound of `α`.
///
/// - All other cases fall through to `unify()`. This is correct because unification
///   handles structural decomposition (record fields, function params/return) and the
///   U-SUBSUME ground-type fallback. TypeVars encountered during structural decomposition
///   recursively invoke `constrain()` via `unify()`'s arms, which will eventually
///   reach the TypeVar accumulation arms here.
///
/// **Why separate from `unify()`:**
/// Argument-passing is a subtype relationship (`arg_ty <: param_ty`), not an equality.
/// When the param type is `Union([Int, TypeVar(α)])`, `constrain(arg_ty, param_ty)` correctly
/// applies C-Var1 and accumulates `arg_ty & ~Int` as a lower bound of `α`. With symmetric
/// `unify()`, both orderings apply C-Var1 identically, losing the directionality needed for
/// principal type inference (Parreaux & Chau 2022, §3.2.1).
///
/// Parreaux & Chau (2022), OOPSLA '22, §3.2.1 — C-Var1/2 in constrain(), not unify().
#[allow(dead_code)]
pub async fn constrain(
    sub: &Type,
    sup: &Type,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) -> Result<(), TypeError> {
    // Apply current substitution to both sides.
    let mut visited_types = HashSet::new();
    let mut visited_rows = HashSet::new();
    let sub_substituted = state.apply_with_visited(sub, &mut visited_types, &mut visited_rows);
    visited_types.clear();
    let sup_substituted = state.apply_with_visited(sup, &mut visited_types, &mut visited_rows);

    // Normalize both types.
    let mut norm_ctx =
        crate::type_normalize::NormCtxt::new(state.type_stage_env.clone(), state.eval_ctx.clone());
    norm_ctx.allow_eval = false;
    let sub =
        crate::type_normalize::normalize(&sub_substituted, &state.type_vars, &mut norm_ctx).await;
    let sup =
        crate::type_normalize::normalize(&sup_substituted, &state.type_vars, &mut norm_ctx).await;
    drop(norm_ctx);

    if sub == sup {
        return Ok(());
    }

    match (&sub, &sup) {
        // Error absorption: absorb silently to prevent cascade errors.
        (Type::Error(_), _) | (_, Type::Error(_)) => return Ok(()),

        // Unknown: directional — zero levels of affected vars, accept the constraint.
        (Type::Unknown, Type::TypeVar(name, _)) => {
            state.set_level(name.clone(), 0);
            return Ok(());
        }
        (Type::TypeVar(name, _), Type::Unknown) => {
            state.set_level(name.clone(), 0);
            return Ok(());
        }
        (Type::Unknown, other) | (other, Type::Unknown) => {
            let mut type_vars = HashSet::new();
            other.collect_all_vars(&mut type_vars);
            for var in &type_vars {
                state.set_level(var.clone(), 0);
            }
            return Ok(());
        }

        // [C-VAR1] (Parreaux & Chau 2022, §3.2.1): τ₁ ≤ τ₂ ∨ α → τ₁ & ~τ₂ ≤ α
        //
        // Directional: fires only when Union is on the RIGHT (sup position).
        // This is the key distinction from unify() where both orderings call bas_cvar1_rewrite.
        // In constrain(sub, sup), sup=Union means we are constraining sub to fit into the union,
        // so the TypeVar in the union must absorb whatever sub is not covered by the other members.
        (_, Type::Union(members))
            if !sub.has_inference_vars()
                && members.iter().any(|m| matches!(m, Type::TypeVar(_, _))) =>
        {
            let members = members.clone();
            return bas_cvar1_rewrite(&members, &sub, state, constraints, span).await;
        }

        // [C-VAR2] (Parreaux & Chau 2022, §3.2.1): α ∧ τ₁ ≤ τ₂ → α ≤ ~τ₁ ∨ τ₂
        //
        // Directional: fires only when Intersection is on the LEFT (sub position).
        // In constrain(sub, sup), sub=Intersection means we are constraining the intersection
        // to fit into sup. The TypeVar in the intersection contributes an upper bound.
        (Type::Intersection(members), _)
            if !sup.has_inference_vars()
                && members.iter().any(|m| matches!(m, Type::TypeVar(_, _))) =>
        {
            let members = members.clone();
            return bas_cvar2_rewrite(&members, &sup, state, constraints, span).await;
        }

        // TypeVar accumulation: sub <: TypeVar(α) → α has lower bound sub.
        // Collect sub as a lower bound of α rather than binding α = sub.
        // This preserves principal types: α can still be instantiated to any supertype of sub.
        // Guard: sub must be a ground type (no inference vars) to avoid pushing unsolved TypeVars
        // as bounds. If sub contains TypeVars, fall through to unify() which handles U-VAR-LEVEL.
        (_, Type::TypeVar(var_name, _)) if !sub.has_inference_vars() => {
            let alpha_level = state.get_level(var_name).unwrap_or(0);
            if lower_levels_check_occurs(&sub, var_name, alpha_level, state) {
                return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!("infinite type: {var_name} occurs in {sub}"),
                    span,
                    notes: vec![],
                    call_stack: vec![],
                })));
            }
            let normalized = crate::bas::to_rdnf(&sub);
            let flat = crate::bas::flatten_rdnf_to_type(normalized);
            if !matches!(flat, Type::Never) {
                state
                    .bounds
                    .entry(var_name.clone())
                    .or_insert_with(crate::bas::TypeVarBounds::new)
                    .add_lower(flat);
            }
            return Ok(());
        }

        // TypeVar accumulation: TypeVar(α) <: sup → α has upper bound sup.
        // Collect sup as an upper bound of α rather than binding α = sup.
        // Guard: sup must be a ground type (no inference vars) to avoid pushing unsolved TypeVars
        // as bounds. If sup contains TypeVars, fall through to unify() which handles U-VAR-LEVEL.
        (Type::TypeVar(var_name, _), _) if !sup.has_inference_vars() => {
            let alpha_level = state.get_level(var_name).unwrap_or(0);
            if lower_levels_check_occurs(&sup, var_name, alpha_level, state) {
                return Err(TypeError::from(TypeErrorTyped::Generic(GenericTypeError {
                    message: format!("infinite type: {var_name} occurs in {sup}"),
                    span,
                    notes: vec![],
                    call_stack: vec![],
                })));
            }
            let normalized = crate::bas::to_rdnf(&sup);
            let flat = crate::bas::flatten_rdnf_to_type(normalized);
            if !matches!(flat, Type::Any) {
                state
                    .bounds
                    .entry(var_name.clone())
                    .or_insert_with(crate::bas::TypeVarBounds::new)
                    .add_upper(flat);
            }
            return Ok(());
        }

        // All other cases: fall through to unify().
        // This handles: structural decomposition (records, functions, apps),
        // TypeVar-to-TypeVar, U-SUBSUME for ground types, etc.
        _ => {}
    }

    unify(&sub, &sup, state, constraints, span).await
}

/// Process deferred equality constraints for stuck TypeStageApp applications.
///
/// After a round of unification, try to resolve deferred equalities using a fixed-point loop:
/// - Take all deferred equalities
/// - Normalize both sides of each
/// - If both sides are fully reduced (no TypeStageApp nodes), attempt unification
/// - Otherwise, keep them deferred for the next round
/// - Repeat until a full iteration produces no progress
///
/// Unification failures during an iteration are silently dropped (not propagated with `?`):
/// if unification of a fully-reduced pair fails, that equality is discarded and the error
/// will surface later when the affected type variable is used in a context that requires it.
/// This preserves the fixed-point invariant — a single failure mid-iteration must not
/// abort processing of remaining equalities that might still make progress.
///
/// Called after each SCC's substitution merge in `infer_dict` (typecheck_dict.rs).
/// Union-vs-Union deferred equalities (from the arm above) also land here.
/// See doc/06-type-inference.md:884.
#[allow(dead_code)]
pub async fn process_deferred_equalities(
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) {
    let max_iterations = 100;
    let mut iteration = 0;
    let mut progress = true;
    while progress && iteration < max_iterations {
        iteration += 1;
        progress = false;
        let deferred = std::mem::take(&mut state.deferred_equalities);
        if deferred.is_empty() {
            break;
        }
        // One NormCtxt per outer iteration: the resolver cache is shared across all
        // equality pairs in this pass, amortizing the HashMap allocation cost.
        let mut norm_ctx = crate::type_normalize::NormCtxt::new(
            state.type_stage_env.clone(),
            state.eval_ctx.clone(),
        );
        for (a, b) in deferred {
            // Normalize both sides
            let a_norm =
                crate::type_normalize::normalize(&a, &state.type_vars, &mut norm_ctx).await;
            let b_norm =
                crate::type_normalize::normalize(&b, &state.type_vars, &mut norm_ctx).await;

            if !a_norm.has_type_stage_app() && !b_norm.has_type_stage_app() {
                // Both sides fully reduced — attempt unification.
                // F10 FIX: Emit diagnostic on unification failure instead of silently dropping.
                match Box::pin(unify(&a_norm, &b_norm, state, constraints, span.clone())).await {
                    Ok(()) => {
                        progress = true;
                    }
                    Err(err) => {
                        // Deferred equality failed — emit T013-style diagnostic
                        state.diagnostics.push(crate::error::TypeDiagnostic {
                            message: format!(
                                "deferred type equality failed: cannot unify {} with {} — {}",
                                a_norm, b_norm, err.message
                            ),
                            span: span.clone(),
                            code: crate::typecheck::typecheck_diag::T013_AMBIGUOUS_CONSTRAINT,
                            level: crate::error::DiagnosticLevel::Warn,
                        });
                    }
                }
                // Don't re-defer either way (fully reduced)
            } else {
                // Still stuck — keep deferred for the next iteration
                state.deferred_equalities.push((a_norm, b_norm));
            }
        }
    }
}

#[cfg(test)]
#[path = "type_unify_tests.rs"]
mod type_unify_tests;
