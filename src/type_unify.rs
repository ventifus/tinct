//! Substitution, unification, and constraint solving for Hindley-Milner polymorphism
//! with Boolean-Algebraic Subtyping (BAS) and structural record types.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::ast::Span;
use crate::type_def::substitute_recvar;

use super::*;

/// Maximum recursion depth for constraint satisfaction checking.
/// Prevents infinite loops when checking constraints on recursive types.
const MAX_CONSTRAINT_DEPTH: usize = 256;

/// Check if a type satisfies a type class constraint.
/// Returns true if the type is an instance of the class.
///
/// This function handles three kinds of constraint satisfaction:
///
/// 1. **Gradual/lattice meta-rules**: Unknown satisfies all constraints vacuously; Never
///    vacuously (uninhabited); Top only satisfies Showable (total function policy).
///
/// 2. **Structural propagation**: Record, NominalVariant, Union, and Intersection types
///    are checked by recursing into their fields/members. A compound type satisfies a
///    structural class iff all of its components do.
///
/// 3. **Primitive leaf membership**: Delegated to `primitive_satisfies_constraint` (defined
///    in `type_def.rs`). That function is the **single authoritative source** of which
///    concrete types belong to which classes. `InferState::new()` also calls it to pre-seed
///    `InstanceEnv` — so primitive membership is defined once and used in both paths.
///
/// The separation means: to add a new primitive type to a class, update ONLY
/// `primitive_satisfies_constraint` in `type_def.rs`. The InstanceEnv pre-seeding in
/// `InferState::new()` will automatically reflect the change.
///
/// Structural types (Seq, Map) that require InstanceDecl patterns with TypeVars
/// (e.g., `Showable Seq[T]`) are handled via `InstanceEnv::resolve_instance` in
/// `check_constraints_on_var` — those instances are pre-seeded in `InferState::new()`.
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

    // [CONSTRAIN-TOP]: Showable(⊤) satisfied, all other classes ⊢ error.
    // Top concretizes only to itself (γ(⊤) = {⊤}), so class membership requires
    // a literal Top instance. Showable is the sole exception (total function policy).
    if matches!(ty, Type::Top) {
        return class_name == "Showable";
    }

    // [CONSTRAIN-FIELD]: C(Record({f: τ})) ⊢ satisfied iff C(τ) for all fields.
    // Applies to built-in STRUCTURAL/COMPOSITIONAL classes where constraint satisfaction
    // is determined by field types: Numeric, Comparable, Equatable, Showable.
    // Mappable and Appendable are NOT structural (they depend on collection semantics).
    if let Type::Record(row) = ty {
        match class_name {
            "Numeric" | "Comparable" | "Equatable" | "Showable" => {
                if row.fields.is_empty() {
                    return matches!(class_name, "Equatable" | "Showable");
                }
                return row
                    .fields
                    .values()
                    .all(|field_ty| satisfies_constraint_inner(field_ty, class_name, depth + 1));
            }
            _ => {} // Fall through to primitive check / instance resolution
        }
    }

    // [CONSTRAIN-NOMINAL]: C(NominalVariant{tag, fields}) ⊢ satisfied iff C(τ) for all fields.
    // NominalVariants are structurally Equatable/Showable if all their fields are.
    // Comparable and Numeric do NOT apply to NominalVariants (they are not ordered scalars).
    if let Type::NominalVariant { fields, .. } = ty {
        match class_name {
            "Equatable" | "Showable" => {
                if fields.fields.is_empty() {
                    return true;
                }
                return fields
                    .fields
                    .values()
                    .all(|field_ty| satisfies_constraint_inner(field_ty, class_name, depth + 1));
            }
            _ => {} // Fall through to instance resolution
        }
    }

    // [CONSTRAIN-CONTAINER]: Seq[T] and Map[K V] are always Showable and Appendable,
    // regardless of their element types. The runtime str() and ++ operations work for any
    // collection at the semantic level. This rule is needed to handle structural propagation
    // through Record fields that contain Seq or Map types.
    //
    // Seq and Map are now App(TyCon("Seq"), ...) and App(App(TyCon("Map"), ...), ...).
    // Use the as_seq() / as_map() helpers for detection.
    if (ty.as_seq().is_some() || ty.as_map().is_some()) && class_name == "Showable" {
        return true;
    }
    // Appendable for Seq (any Seq can be appended via concat semantics).
    // Appendable for Map: maps are not appendable (no Appendable Map instance in prelude).
    if ty.as_seq().is_some() && class_name == "Appendable" {
        return true;
    }
    // Record for Appendable: any record satisfies Appendable (dict merge semantics).
    if matches!(ty, Type::Record(_)) && class_name == "Appendable" {
        return true;
    }

    // [CONSTRAIN-EQUATABLE-CONTAINER]: Seq[T], Map[K V], and Record are structurally
    // equatable — the runtime equality check works for any sequence, map, or record.
    if class_name == "Equatable"
        && (matches!(ty, Type::Record(_)) || ty.as_seq().is_some() || ty.as_map().is_some())
    {
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

    // Primitive leaf check: delegate to the canonical membership table in type_def.rs.
    // This is the single source of truth for which concrete primitive types belong to which
    // classes. InferState::new() pre-seeds InstanceEnv from the same function via
    // primitive_satisfies_constraint, so InstanceEnv and satisfies_constraint are always
    // in sync for primitive leaf types.
    primitive_satisfies_constraint(ty, class_name)
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
            let target_var = &target_vars[0];
            for constraint in context {
                if let Constraint::Class { class, vars, .. } = constraint {
                    if vars.len() == 1
                        && vars[0] == *target_var
                        && is_superclass_of(class_env, &class.name, &target_class.name)
                    {
                        return true;
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
fn check_constraints_on_var(
    var_name: &str,
    concrete_ty: &Type,
    subst: &Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // Collect only the constraints that apply to var_name (immutable scan first).
    // This avoids cloning the entire Vec<Constraint> — we clone only the constraints
    // that match, which is typically 0–2 per variable binding even in constraint-heavy
    // programs. HasField constraints are skipped here (handled in resolve_has_field).
    #[derive(Clone)]
    enum ApplicableConstraint {
        SingleParam {
            class: String,
        },
        MultiParam {
            class: String,
            vars: Vec<String>,
            fundeps: Vec<(Vec<usize>, Vec<usize>)>,
            resolver_injective: bool,
        },
    }

    let applicable: Vec<ApplicableConstraint> = state
        .constraints
        .iter()
        .filter_map(|c| match c {
            Constraint::Class { class, vars, .. } if vars.len() == 1 && vars[0] == var_name => {
                Some(ApplicableConstraint::SingleParam {
                    class: class.name.clone(),
                })
            }
            Constraint::Class { class, vars, .. }
                if vars.len() > 1 && vars.iter().any(|v| v == var_name) =>
            {
                Some(ApplicableConstraint::MultiParam {
                    class: class.name.clone(),
                    vars: vars.clone(),
                    fundeps: class.determines.clone(),
                    resolver_injective: class.resolver_injective,
                })
            }
            _ => None,
        })
        .collect();

    for constraint in applicable {
        match constraint {
            ApplicableConstraint::SingleParam { class } => {
                // Single-parameter type class constraint (e.g., Numeric a)
                // First, check via satisfies_constraint (structural meta-rules + primitive
                // leaf membership from primitive_satisfies_constraint). This is the fast path
                // that avoids instance resolution for the common case.
                if satisfies_constraint(concrete_ty, &class) {
                    continue;
                }

                // Fast path returned false: try instance resolution for parametric structural
                // types (Seq[T], Map[K V]) and user-defined instances. Pre-seeded InstanceDecl
                // entries in InferState::new() cover the parametric cases.
                // This enables user-defined instances (future work: dictionary construction)
                // Clone instance_env to avoid borrowing state both immutably (for the
                // field access) and mutably (as the unify parameter) at the same time.
                const MAX_INSTANCE_RESOLUTION_DEPTH: u32 = 64;
                if state.instance_resolution_depth >= MAX_INSTANCE_RESOLUTION_DEPTH {
                    // Too deep — return a type error instead of silently skipping.
                    // The recursion cycle is: check_constraints_on_var → resolve_instance →
                    // unify → check_constraints_on_var. This matches GHC's -freduction-depth
                    // semantics (Sulzmann et al. 2007 §3.2).
                    return Err(TypeError::new(
                        format!(
                            "instance resolution depth limit exceeded (max {}) — possible recursive instance definitions for constraint {}",
                            MAX_INSTANCE_RESOLUTION_DEPTH,
                            class
                        ),
                        span.clone(),
                    ));
                }
                state.instance_resolution_depth += 1;
                let inst_env = state.instance_env.clone();
                let resolve_result = inst_env.resolve_instance(&class, concrete_ty, state);
                state.instance_resolution_depth -= 1;

                match resolve_result {
                    Ok(Some(_)) => {
                        // Instance found - constraint satisfied
                        continue;
                    }
                    Ok(None) => {
                        // No instance found - constraint violated
                        return Err(TypeError::new(
                            format!("type {} does not satisfy constraint {}", concrete_ty, class),
                            span.clone(),
                        ));
                    }
                    Err(ambig_msg) => {
                        // Ambiguous instances — equally specific matches, coherence violation
                        return Err(TypeError::new(ambig_msg, span.clone()));
                    }
                }
            }
            ApplicableConstraint::MultiParam {
                class,
                vars,
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
                    &vars,
                    &fundeps,
                    resolver_injective,
                    var_name,
                    concrete_ty,
                    subst,
                    state,
                    span.clone(),
                );
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
fn improve_functional_dependency(
    class: &str,
    vars: &[String],
    fundeps: &[(Vec<usize>, Vec<usize>)],
    resolver_injective: bool,
    bound_var: &str,
    bound_type: &Type,
    subst: &Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // Depth guard: prevent infinite recursion through the FD improvement cycle.
    if state.fd_depth >= MAX_FD_DEPTH {
        // F7 FIX: Return error instead of silently succeeding when depth limit is reached
        return Err(TypeError::new(
            format!(
                "functional dependency improvement depth limit exceeded (max {}) — possible recursive FD chain for class {}",
                MAX_FD_DEPTH, class
            ),
            span,
        ));
    }
    state.fd_depth += 1;
    let result = improve_functional_dependency_inner(
        class,
        vars,
        fundeps,
        resolver_injective,
        bound_var,
        bound_type,
        subst,
        state,
        span,
    );
    state.fd_depth -= 1;
    result
}

#[allow(clippy::too_many_arguments)] // FD improvement requires all constraint components
fn improve_functional_dependency_inner(
    class: &str,
    vars: &[String],
    fundeps: &[(Vec<usize>, Vec<usize>)],
    resolver_injective: bool,
    bound_var: &str,
    bound_type: &Type,
    subst: &Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // For each functional dependency (determining → determined)
    for (det_positions, ded_positions) in fundeps {
        // Compute the positions of bound_var in the constraint var list
        let bound_var_positions: Vec<usize> = vars
            .iter()
            .enumerate()
            .filter(|(_, v)| *v == bound_var)
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
        //    for the in-flight binding, subst for already-bound vars).
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
                if pos >= vars.len() {
                    continue;
                }
                let var = &vars[pos];
                let ty = if var == bound_var {
                    subst.apply(bound_type)
                } else {
                    subst.apply(&Type::TypeVar(var.clone(), 0))
                };
                ded_types.push((pos, var, ty));
            }

            // Only attempt reverse improvement when all determined positions are ground.
            let all_ded_ground = ded_types.iter().all(|(_, _, ty)| !ty.has_inference_vars());
            if all_ded_ground {
                // Scan InstanceEnv for an instance whose determined-position type
                // unifies with the ground determined types we have.
                let instance_env = state.instance_env.clone();
                if let Some((determining_types, det_pos_list)) = instance_env.reverse_lookup_mptc(
                    class,
                    ded_positions,
                    &ded_types
                        .iter()
                        .map(|(_, _, ty)| ty.clone())
                        .collect::<Vec<_>>(),
                    state,
                ) {
                    // Unify each determining-position variable with the back-propagated type.
                    //
                    // Guard: skip variables already being processed by an outer FD improvement
                    // call (in fd_in_progress). This prevents the mutual-recursion cycle:
                    //   reverse(t1=Str) → bind(t0=Int) → forward(t0=Int) → bind(t1=Str)
                    //   → reverse(t1=Str) → … (hits MAX_FD_DEPTH).
                    // The guard makes the forward FD's re-binding of t1 a no-op: t1 is already
                    // being bound in the outer check_constraints_on_var("t1", Str, …) call.
                    for (det_pos, det_ty) in det_pos_list.iter().zip(determining_types.iter()) {
                        if *det_pos >= vars.len() {
                            continue;
                        }
                        let det_var = &vars[*det_pos];
                        // Skip if this var is already being processed by an outer FD step.
                        if state.fd_in_progress.contains(det_var.as_str()) {
                            continue;
                        }
                        let det_type_var = Type::TypeVar(det_var.clone(), 0);
                        state.fd_in_progress.insert(det_var.clone());
                        let mut local_subst = std::mem::take(&mut state.subst);
                        let result =
                            unify(&det_type_var, det_ty, &mut local_subst, state, span.clone());
                        state.subst = local_subst;
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
        // 1. `subst` — the active substitution being threaded through unification right now.
        //    This contains bindings made during the current unify() call tree, including
        //    bindings from earlier argument unifications. It is often separate from state.subst
        //    because callers use mem::take to avoid borrow-checker conflicts.
        //
        // 2. `bound_type` — the concrete type being bound to `bound_var` RIGHT NOW.
        //    check_constraints_on_var is called BEFORE the binding is written to subst
        //    (see U-VAR arm: check_constraints_on_var → insert). So looking up `bound_var`
        //    from subst would return the unbound TypeVar — not the value being bound.
        //    We must use `bound_type` directly for the variable currently being bound.
        //
        // Lookup order: for `bound_var` → use `bound_type` (in-flight, not yet in subst).
        //               for all other vars → apply subst first (has recent bindings from
        //               this call tree), then fall back to state.subst (global accumulated).
        let mut det_types = Vec::new();
        for &pos in det_positions {
            if pos >= vars.len() {
                continue;
            }
            let var = &vars[pos];
            let ty = if var == bound_var {
                // In-flight binding — not yet written to subst
                subst.apply(bound_type)
            } else {
                // Look up from the active subst (most up-to-date within this call tree).
                // apply() chains through bound TypeVars, so this handles the case where
                // _t0 was bound in a prior argument unification in this call tree.
                subst.apply(&Type::TypeVar(var.clone(), 0))
            };
            det_types.push((pos, var, ty));
        }

        // Check if ALL determining positions are ground
        let all_det_ground = det_types.iter().all(|(_, _, ty)| !ty.has_inference_vars());

        if !all_det_ground {
            // Not all determining positions are ground yet - can't improve
            continue;
        }

        // All determining positions are ground - look up the instance.
        // Multiple paths:
        // 1. Indexable + Record: special case for HasField-style resolution
        // 2. Arithmetic classes: hardcoded lookup_arithmetic_instance
        // 3. Resolver classes: type-stage function normalization
        // 4. General MPTC: InstanceEnv lookup

        // Special case: Indexable on Record/Union/Intersection/Top types uses
        // resolve_has_field for field lookup instead of instance registration.
        // Records are structural (not nominal), so they don't register instances —
        // instead, resolve_has_field applies [HAS-FIELD-REC], [HAS-FIELD-UNION],
        // [HAS-FIELD-INTER], and [HAS-FIELD-TOP] rules from type_unify.rs.
        let indexable_record_result = if class == "Indexable" && det_types.len() == 2 {
            let container_ty = &det_types[0].2;
            let key_ty = &det_types[1].2;

            match (container_ty, key_ty) {
                (
                    Type::Record(_) | Type::Intersection(_) | Type::Top,
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
                (
                    Type::Record(_) | Type::Union(_) | Type::Intersection(_) | Type::Top,
                    Type::Str,
                ) => {
                    // Str key (from promoted StringLiteral) — can't resolve statically
                    Some(Type::Unknown)
                }
                _ => None, // Not a Record/Union/Intersection/Top case — fall through to general logic
            }
        } else {
            None
        };

        let result_type = if let Some(ty) = indexable_record_result {
            ty
        } else if matches!(
            class,
            "Addable" | "Subtractable" | "Multipliable" | "Divisible"
        ) {
            // Hardcoded arithmetic path — propagate errors
            lookup_arithmetic_instance(
                class,
                &det_types
                    .iter()
                    .map(|(_, _, ty)| ty.clone())
                    .collect::<Vec<_>>(),
                state,
                span.clone(),
            )?
        } else if let Some(class_decl) = state.class_env.get(class) {
            // Not an arithmetic class — check for resolver
            if let Some(ref resolver_name) = class_decl.resolver.clone() {
                // Resolver-based path: construct a TypeStageApp and normalize it.
                // Normalization calls evaluate_resolver() which invokes the type-stage function.
                let det_arg_types: Vec<Type> =
                    det_types.iter().map(|(_, _, ty)| ty.clone()).collect();
                let stage_app = Type::TypeStageApp {
                    fn_name: resolver_name.clone(),
                    args: det_arg_types,
                };
                let mut norm_ctx = crate::type_normalize::NormCtxt::new();
                let resolved = crate::type_normalize::normalize(&stage_app, subst, &mut norm_ctx);

                // If normalization returned a stuck TypeStageApp, we can't improve yet.
                // Defer: the deferred_equalities mechanism will retry when more types are ground.
                if matches!(resolved, Type::TypeStageApp { .. }) {
                    continue;
                }
                resolved
            } else {
                // No resolver — fall back to general MPTC instance lookup via InstanceEnv
                let det_arg_types: Vec<Type> =
                    det_types.iter().map(|(_, _, ty)| ty.clone()).collect();

                // Clone instance_env to avoid borrow checker conflict
                let instance_env = state.instance_env.clone();
                match instance_env.lookup_mptc(class, &det_arg_types, state) {
                    Some(inst) => {
                        // Extract the determined type from the instance.
                        // For a multi-param MPTC instance, instance_type is a Record with
                        // numbered fields (0, 1, 2, …). Determined positions are those NOT in det_positions.
                        let det_position_set: std::collections::HashSet<usize> =
                            inst.det_positions.iter().copied().collect();

                        match &inst.instance_type {
                            Type::Record(row) => {
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
                                            TypeError::new(
                                                format!(
                                                    "no instance for {} (determined field {} missing)",
                                                    class, pos
                                                ),
                                                span.clone(),
                                            )
                                        })?,
                                    None => {
                                        return Err(TypeError::new(
                                            format!(
                                                "no instance for {} (no determined position found)",
                                                class
                                            ),
                                            span.clone(),
                                        ));
                                    }
                                }
                            }
                            _ => {
                                return Err(TypeError::new(
                                    format!(
                                        "no instance for {} (unexpected instance_type shape)",
                                        class
                                    ),
                                    span.clone(),
                                ));
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
                            .any(|(_, _, ty)| !is_definitely_no_instance_for(class, ty));
                        if should_defer {
                            continue;
                        }
                        return Err(TypeError::new(
                            format!("no instance for {}", class),
                            span.clone(),
                        ));
                    }
                }
            }
        } else {
            // Class not found in class_env — should not happen
            return Err(TypeError::new(
                format!("unknown class {}", class),
                span.clone(),
            ));
        };

        // Unify each determined position with the result type
        for &ded_pos in ded_positions {
            if ded_pos >= vars.len() {
                continue;
            }
            let ded_var = &vars[ded_pos];

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

            // F2 FIX: Use state.subst for determined unification.
            // We can't use the `subst` parameter directly because it's immutable (&Substitution).
            // The correct approach is to take state.subst, unify, and restore it.
            // This ensures the determined binding is written to the same substitution that
            // the outer unify call will eventually merge back to state.subst.
            //
            // Note: The determining lookup (lines 488-520) uses the passed `subst` parameter
            // to read bindings from the active call tree. The determined unification writes
            // to state.subst, which is correct because state.subst is the authoritative store
            // for completed bindings. The caller's `subst` parameter is a view/snapshot used
            // for reading, not writing.
            state.fd_in_progress.insert(ded_var.clone());
            let mut local_subst = std::mem::take(&mut state.subst);
            let result = unify(
                &ded_type_var,
                &result_type,
                &mut local_subst,
                state,
                span.clone(),
            );
            state.subst = local_subst;
            state.fd_in_progress.remove(ded_var.as_str());
            result?;
        }
    }

    Ok(())
}

/// Instance lookup for multi-parameter type classes with functional dependencies.
///
/// Given the class name and the ground determining types, returns the determined type.
///
/// Two-tier lookup:
///
/// 1. FAST PATH — hardcoded table for the builtin Addable/Subtractable/Multipliable/Divisible
///    instances (all share the FD shape `(a,b) → c`).  This avoids `instance_env` iteration
///    for the common arithmetic case.
///
/// 2. GENERAL PATH — for any MPTC class not in the hardcoded list, delegates to
///    `instance_env.lookup_mptc(class, det_types, state)`. `lookup_mptc` uses structural
///    unification to match instances, which correctly handles HKT instance heads with type
///    variables (e.g., `[Channel t]` can match query types like `[Channel Int]`).
///    On a successful lookup the determined type is extracted from the numbered-field Record
///    that encodes the multi-param instance head. On a miss a `no instance for …` error is
///    returned.
fn lookup_arithmetic_instance(
    class: &str,
    det_types: &[Type],
    state: &mut InferState,
    span: Span,
) -> Result<Type, TypeError> {
    if det_types.len() != 2 {
        return Err(TypeError::new(
            format!(
                "arithmetic class {} expects 2 determining types, got {}",
                class,
                det_types.len()
            ),
            span,
        ));
    }

    let a = &det_types[0];
    let b = &det_types[1];

    // Normalize types for comparison
    let key = (type_key(a), type_key(b));

    // FAST PATH: hardcoded instances for Addable/Subtractable/Multipliable/Divisible (performance)
    match class {
        "Addable" | "Subtractable" | "Multipliable" => match key {
            ("Int", "Int") => Ok(Type::Int),
            ("Float", "Float") => Ok(Type::Float),
            ("Int", "Float") | ("Float", "Int") => Ok(Type::Float),
            // Number op Number — result is Number (could be Int or Float at runtime)
            ("Number", "Number") | ("Number", "Int") | ("Int", "Number") => Ok(Type::Number),
            // Number op Float or Float op Number — result is always Float.
            // Reasoning: Int op Float = Float and Float op Float = Float, so either
            // way the result is Float. This matches the Divisible rule below.
            ("Number", "Float") | ("Float", "Number") => Ok(Type::Float),
            // T1 FIX: Never ∨ τ = Never (⊥ absorbs all operations)
            ("Never", _) | (_, "Never") => Ok(Type::Never),
            _ => Err(TypeError::new(
                format!("no instance for {} {} {}", class, a, b),
                span,
            )),
        },
        "Divisible" => match key {
            ("Int", "Int") | ("Float", "Float") | ("Int", "Float") | ("Float", "Int") => {
                Ok(Type::Float)
            }
            // Number / Number or Number / Int or Int / Number — result is Number
            // (could be Float or Int depending on runtime values; Int/Int→Float though)
            ("Number", "Number") | ("Number", "Int") | ("Int", "Number") => Ok(Type::Number),
            // Number / Float or Float / Number — result is always Float.
            // Int / Float = Float and Float / Float = Float.
            ("Number", "Float") | ("Float", "Number") => Ok(Type::Float),
            // T1 FIX: Never ∨ τ = Never (⊥ absorbs all operations)
            ("Never", _) | (_, "Never") => Ok(Type::Never),
            _ => Err(TypeError::new(
                format!("no instance for Divisible {} {}", a, b),
                span,
            )),
        },
        _ => {
            // GENERAL PATH: query InstanceEnv for user-defined MPTC classes.
            // `lookup_mptc` now uses structural unification instead of string-key lookup,
            // which correctly handles HKT instance heads with type variables.
            // Clone instance_env to avoid borrow checker conflict
            let instance_env = state.instance_env.clone();
            match instance_env.lookup_mptc(class, det_types, state) {
                Some(inst) => {
                    // Extract the determined type from the instance.
                    // For a multi-param MPTC instance, instance_type is a Record with
                    // numbered fields (0, 1, 2, …).  The determined position is the first
                    // index not listed in inst.det_positions.
                    let det_position_set: HashSet<usize> =
                        inst.det_positions.iter().copied().collect();

                    match &inst.instance_type {
                        Type::Record(row) => {
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
                                        TypeError::new(
                                            format!(
                                                "no instance for {} {} {} (determined field {} missing)",
                                                class, a, b, pos
                                            ),
                                            span,
                                        )
                                    }),
                                None => Err(TypeError::new(
                                    format!(
                                        "no instance for {} {} {} (no determined position found)",
                                        class, a, b
                                    ),
                                    span,
                                )),
                            }
                        }
                        _ => Err(TypeError::new(
                            format!(
                                "no instance for {} {} {} (unexpected instance_type shape)",
                                class, a, b
                            ),
                            span,
                        )),
                    }
                }
                None => Err(TypeError::new(
                    format!("no instance for {} {} {}", class, a, b),
                    span,
                )),
            }
        }
    }
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
/// structurally incompatible with the class.  Structural types (Record, Union,
/// Intersection, Top, Seq, Map) and the gradual `Unknown` can always
/// potentially satisfy a class at runtime, so we return `false` for them.
fn is_definitely_no_instance_for(class: &str, ty: &Type) -> bool {
    match class {
        "Indexable" => {
            // A type is definitively non-Indexable only if it is a scalar/primitive
            // that cannot possibly be a container at runtime.
            // Record, Union, Intersection, Top, Seq, Map, Unknown, NominalVariant — might work.
            // Scalars (Int/Float/Bool/Str/Number and their literals), Function, Never — cannot.
            matches!(
                ty,
                Type::Int
                    | Type::Float
                    | Type::Number
                    | Type::Bool
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

/// Helper to extract a type key for instance lookup.
fn type_key(ty: &Type) -> &'static str {
    match ty {
        Type::Int => "Int",
        Type::Float => "Float",
        Type::Number => "Number",
        Type::IntLiteral(_) => "Int", // Promoted
        Type::Never => "Never",       // T1 FIX: Never gets its own key (not "Unknown")
        _ => "_other", // F6 FIX: renamed from "Unknown" (clearer sentinel for unhandled types)
    }
}

/// When binding a constrained type variable, promote literal types to their parent types.
/// This prevents `[+ 1 2]` from failing: without promotion, `_t0` (Numeric) would bind
/// to `IntLiteral(1)`, then unification of `IntLiteral(1)` with `IntLiteral(2)` would fail.
/// With promotion, `_t0` binds to `Int`, and both `IntLiteral(1)` and `IntLiteral(2)` unify
/// with `Int` via the literal-to-parent promotion rules.
///
/// Promotion is now restricted to known primitive classes where literal instances entail
/// parent instances (Numeric, Comparable, Equatable, Showable, Add, Sub, Mul, Div).
/// For other classes, preserve the literal type and let instance resolution handle it.
fn promote_literal_for_constrained_var(var_name: &str, ty: Type, state: &InferState) -> Type {
    // Label-kinded TypeVars must not be promoted regardless of constraint presence
    // (preserves StringLiteral identity for field access)
    if state.kind_env.get(var_name) == Some(&Kind::Label) {
        return ty;
    }

    // Only promote for known primitive classes where literal instances entail parent instances.
    // These classes have the property that if IntLiteral(42) satisfies the class, then Int
    // also satisfies it (and similarly for StringLiteral/Str).
    const PROMOTABLE_CLASSES: &[&str] = &[
        "Numeric",
        "Comparable",
        "Equatable",
        "Showable",
        "Addable",
        "Subtractable",
        "Multipliable",
        "Divisible",
    ];

    let has_promotable_constraint = state.constraints.iter().any(|c| match c {
        Constraint::Class { class, vars, .. } => {
            vars.iter().any(|v| v == var_name) && PROMOTABLE_CLASSES.contains(&class.name.as_str())
        }
        _ => false,
    });

    if !has_promotable_constraint {
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
        return Err(TypeError::new("HasField recursion depth exceeded", span));
    }

    // Resolve label to concrete string
    let label_str = match label {
        Label::Concrete(s) => s.clone(),
        Label::Var(var_name) => {
            // Look up the label var in substitution
            match state.subst.type_map.borrow().get(var_name) {
                Some(Type::StringLiteral(s)) => s.clone(),
                _ => {
                    return Err(TypeError::new(
                        format!("label variable {} not bound to a string literal", var_name),
                        span,
                    ))
                }
            }
        }
    };

    // Apply substitution to dict_type to dereference any already-bound TypeVars
    let dict_type = state.subst.apply(dict_type);

    match &dict_type {
        // [HAS-FIELD-REC]: Record with matching field → return field type
        Type::Record(row) => {
            if let Some(field_ty) = row.fields.get(&label_str) {
                Ok(field_ty.clone())
            } else {
                Err(TypeError::new(
                    format!("record has no field '{}'", label_str),
                    span,
                ))
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
        Type::Top => Ok(Type::Top),

        // [HAS-FIELD-UNKNOWN]: Unknown → Unknown
        Type::Unknown => Ok(Type::Unknown),

        // [HAS-FIELD-NEVER]: Never → Never (vacuous)
        Type::Never => Ok(Type::Never),

        // TypeVar: defer constraint (handled by caller)
        Type::TypeVar(_, _) => Err(TypeError::new(
            "cannot resolve HasField constraint on unbound type variable (expected caller to defer)".to_string(),
            span,
        )),

        // All other types don't support field access
        _ => Err(TypeError::new(
            format!("type {} does not support field access", dict_type),
            span,
        )),
    }
}

/// Per-scope substitution frame with parent-chain lookup.
///
/// Each dict inference scope gets its own child frame. TypeVar bindings are routed
/// to the frame whose `creation_level` matches the TypeVar's creation-time level,
/// preventing TypeVars from one dict scope from escaping into sibling or ancestor scopes.
///
/// The parent chain is traversed by `apply_type` for lookup and by `bind_at_level`
/// for writes. Interior mutability (`RefCell`) allows writes through `Arc` references.
pub struct Substitution {
    pub type_map: std::cell::RefCell<HashMap<String, Type>>, // α → τ  (kind: Type)
    /// Parent frame in the scope chain. `None` for the root substitution.
    pub parent: Option<Arc<Substitution>>,
    /// The InferState level at which this substitution frame was created.
    /// TypeVars whose creation-time level equals this value are bound here.
    /// TypeVars with a lower level are routed to the parent chain.
    pub creation_level: u32,
    /// Per-frame monotonic counter for fresh TypeVar names within this substitution scope.
    ///
    /// Lives here (rather than on InferState) so that child substitution frames (T-926/T-927)
    /// inherit the parent's counter value and continue from it. This ensures globally unique
    /// TypeVar names across all active frames (Barendregt convention) — sibling dicts do NOT
    /// reuse names; each continues from where the parent's counter left off.
    ///
    /// Using `Cell<u32>` provides interior mutability: `fresh_type_var` can advance the
    /// counter through a shared reference to the Substitution without requiring the caller
    /// to hold `&mut Substitution`. The `Clone` implementation copies the current counter
    /// value, so probe save/restore patterns (which clone+restore the Substitution) correctly
    /// capture and reset the counter as part of the probe's state rollback.
    pub name_counter: std::cell::Cell<u32>,
}

const MAX_APPLY_DEPTH: usize = 256;

/// Maximum size of the substitution map (type_map entries).
/// Prevents resource exhaustion from quadratic growth in pathological cases.
/// Raised from 10K to 50K to accommodate real-world K8s-style configs with
/// hundreds of open-record dot-accesses that each bind a fresh type variable.
pub const MAX_SUBST_SIZE: usize = 50_000;

// T-991: Thread-local visited set for `Substitution::apply`.
// Declared at module level (not inside the method) so the static-initialization semantics are
// clear: the HashSet is allocated once per thread and reused across all `apply()` calls in that
// thread. Moving this declaration inside the method body was syntactically valid but obscured
// the amortization intent and risked accidental duplication during future refactors.
thread_local! {
    static VISITED_TYPES: std::cell::RefCell<HashSet<String>> = std::cell::RefCell::new(HashSet::new());
}

impl Substitution {
    /// Create a new root substitution frame (no parent).
    ///
    /// Performance note: `HashMap::new()` creates a map with zero capacity
    /// and performs no heap allocation until the first insert. This is optimal
    /// for fully-concrete dicts that generate no unification constraints.
    pub fn new() -> Self {
        Self {
            type_map: std::cell::RefCell::new(HashMap::new()),
            parent: None,
            creation_level: 0,
            name_counter: std::cell::Cell::new(0),
        }
    }

    /// Create a child substitution frame for a nested dict scope.
    ///
    /// TypeVars at `level` are bound in this frame. TypeVars at lower levels
    /// are routed up the parent chain by `bind_at_level`.
    ///
    /// The child's `name_counter` continues from the parent's current value to preserve
    /// globally unique TypeVar names across all frames in the chain (Barendregt convention).
    /// This is required for `lookup_in_chain` to be sound: it matches by name, so names
    /// must be unique across all active frames.
    ///
    /// DESIGN NOTE (T-1002): The child's incremented name_counter is NOT propagated back
    /// to the parent when the child frame is dropped. This is technically a Barendregt
    /// violation — names generated in a child could collide with names in a later sibling
    /// scope at the same level. However, this is safe due to LEVEL ROUTING: each level
    /// gets its own namespace slice via the level-indexed `bind_at_level` routing, so
    /// variables created at different levels never interact even if their numeric suffixes
    /// collide. The parent only sees TypeVars at its own level or lower; child-level vars
    /// are inaccessible after the child frame is dropped, preventing cross-contamination.
    pub fn child(parent: &Arc<Substitution>, level: u32) -> Self {
        Self {
            type_map: std::cell::RefCell::new(HashMap::new()),
            parent: Some(Arc::clone(parent)),
            creation_level: level,
            name_counter: std::cell::Cell::new(parent.name_counter.get()),
        }
    }

    /// Bind a TypeVar to a type in the frame whose `creation_level` matches `var_level`.
    ///
    /// If `var_level == self.creation_level`, the binding goes in this frame's `type_map`.
    /// Otherwise the call is routed to the parent chain. If no frame matches (root reached
    /// without a level match), the root frame absorbs the binding — this handles TypeVars
    /// created at a level that no longer has an active frame (e.g., after scope exit).
    pub fn bind_at_level(&self, name: String, var_level: u32, ty: Type) {
        if self.creation_level == var_level || self.parent.is_none() {
            // Bind here: either we match the level, or we're the root and must absorb it.
            self.type_map.borrow_mut().insert(name, ty);
        } else if let Some(ref p) = self.parent {
            p.bind_at_level(name, var_level, ty);
        }
    }

    /// Look up a variable name in the local frame first, then in the parent chain.
    ///
    /// Returns `Some(bound_type)` from the first frame that has a binding, or `None`
    /// if no frame in the chain has bound this variable.
    fn lookup_in_chain(&self, name: &str) -> Option<Type> {
        // Check local frame first (most recent bindings override parent bindings)
        if let Some(ty) = self.type_map.borrow().get(name).cloned() {
            return Some(ty);
        }
        // Walk up the parent chain
        if let Some(ref p) = self.parent {
            return p.lookup_in_chain(name);
        }
        None
    }

    /// Check if this frame (and its parent chain) is empty (no bindings).
    /// Used to guard against unnecessary allocation in apply() operations.
    pub fn is_empty(&self) -> bool {
        if !self.type_map.borrow().is_empty() {
            return false;
        }
        match &self.parent {
            Some(p) => p.is_empty(),
            None => true,
        }
    }

    /// Check if the LOCAL frame's type_map has exceeded the maximum allowed size.
    /// Only counts local entries — parent chain entries are counted when their own
    /// frame calls check_size. This prevents double-counting shared parent bindings.
    pub(crate) fn check_size(&self, span: Span) -> Result<(), TypeError> {
        let len = self.type_map.borrow().len();
        if len > MAX_SUBST_SIZE {
            Err(TypeError::new(
                format!(
                    "type inference resource limit exceeded (substitution size {} > {}) — use fewer chained dot-accesses or add explicit type annotations to break constraint chains",
                    len, MAX_SUBST_SIZE
                ),
                span,
            ))
        } else {
            Ok(())
        }
    }

    pub fn apply(&self, ty: &Type) -> Type {
        if self.is_empty() {
            return ty.clone();
        }
        // Fast path: if no parent, use the existing single-frame logic below.
        // With a parent chain, apply_type walks the chain for each TypeVar lookup.
        // Fast-path for concrete types: no type variables, so return clone immediately.
        // Avoids allocating visited_types HashSet for the common case.
        match ty {
            Type::Int
            | Type::IntLiteral(_)
            | Type::Float
            | Type::Bool
            | Type::Str
            | Type::StringLiteral(_)
            | Type::Bytes
            | Type::Number
            | Type::Unknown
            | Type::Top
            | Type::Never
            | Type::Proxy
            | Type::Error
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
        // Covers e.g. Function{params: [Int], ret: Str} — no TypeVars, nothing to substitute.
        if !ty.has_inference_vars() {
            return ty.clone();
        }
        // Reuse thread-local visited set (declared at module level) to avoid per-call
        // HashSet allocation. The set is cleared before use to ensure correctness.
        VISITED_TYPES.with(|visited_cell| {
            let mut visited = visited_cell.borrow_mut();
            visited.clear();
            self.apply_type(ty, 0, &mut visited).into_owned()
        })
    }

    /// Apply substitution with an externally-supplied visited set.
    /// Allows sharing the visited set across multiple apply() calls to avoid repeated allocation.
    /// The caller must clear the visited set between uses.
    pub fn apply_with_visited(
        &self,
        ty: &Type,
        visited_types: &mut HashSet<String>,
        _visited_rows: &mut HashSet<String>, // kept for call-site compatibility; unused under BAS
    ) -> Type {
        if self.is_empty() {
            return ty.clone();
        }
        self.apply_type(ty, 0, visited_types).into_owned()
    }

    fn apply_type<'a>(
        &self,
        ty: &'a Type,
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
                // Look up the binding for this TypeVar: check local frame first,
                // then walk up the parent chain. This is O(depth) per lookup;
                // path compression below flattens chains after first resolution.
                let bound_opt = self.lookup_in_chain(name);
                match bound_opt {
                    Some(bound) => {
                        visited_types.insert(name.clone());
                        // Reset depth to 0 when following a TypeVar binding: chain-following
                        // is cycle-protected by visited_types; depth guards structural
                        // recursion only. Resetting prevents premature truncation of
                        // long-but-shallow substitution chains (items 5/6).
                        let result = self.apply_type(&bound, 0, visited_types).into_owned();
                        visited_types.remove(name);

                        // PATH COMPRESSION: if the resolved type differs from the immediate binding,
                        // cache the concrete type in the local frame to avoid repeated parent-chain
                        // traversal. This collapses chains like t0 → t1 → t2 → Int into
                        // local[t0 → Int] after first traversal.
                        // Only compress when result is not a TypeVar to avoid premature compression
                        // of still-growing chains.
                        if !matches!(result, Type::TypeVar(..)) && result != bound {
                            self.type_map
                                .borrow_mut()
                                .insert(name.clone(), result.clone());
                        }

                        Cow::Owned(result)
                    }
                    None => Cow::Owned(Type::TypeVar(name.clone(), *level)),
                }
            }
            Type::Record(row) => {
                let applied_row = self.apply_row(row, depth + 1, visited_types);
                Cow::Owned(Type::Record(applied_row))
            }
            Type::Function {
                params,
                ret,
                variadic,
            } => Cow::Owned(Type::Function {
                params: params
                    .iter()
                    .map(|(name, p_ty)| {
                        (
                            name.clone(),
                            self.apply_type(p_ty, depth + 1, visited_types).into_owned(),
                        )
                    })
                    .collect(),
                ret: Box::new(self.apply_type(ret, depth + 1, visited_types).into_owned()),
                variadic: *variadic,
            }),
            Type::Union(members) => {
                let applied_members: Vec<Type> = members
                    .iter()
                    .map(|m| self.apply_type(m, depth + 1, visited_types).into_owned())
                    .collect();
                // Re-normalize after substitution to maintain invariants
                Cow::Owned(Type::normalize_union(applied_members))
            }
            Type::Intersection(members) => {
                let applied_members: Vec<Type> = members
                    .iter()
                    .map(|m| self.apply_type(m, depth + 1, visited_types).into_owned())
                    .collect();
                // Re-normalize after substitution to maintain invariants
                Cow::Owned(Type::normalize_intersection(applied_members))
            }
            Type::Negation(inner) => Cow::Owned(Type::Negation(Box::new(
                self.apply_type(inner, depth + 1, visited_types)
                    .into_owned(),
            ))),
            Type::App(f, a) => {
                let f_applied = self.apply_type(f, depth + 1, visited_types).into_owned();
                let a_applied = self.apply_type(a, depth + 1, visited_types).into_owned();

                // Normalize App(Operator("Seq"), T) → App(TyCon("Seq"), T) (bind Operator → TyCon)
                // When an Operator TypeVar resolves to a concrete constructor name, update to TyCon.
                if let Type::Operator(ctor_name) = &f_applied {
                    if ctor_name.as_str() == "Seq" {
                        return Cow::Owned(Type::seq(a_applied));
                    }
                    if ctor_name.as_str() == "Map" {
                        // App(Operator("Map"), K) → App(TyCon("Map"), K) (partial application)
                        return Cow::Owned(Type::App(
                            Box::new(Type::TyCon("Map".into())),
                            Box::new(a_applied),
                        ));
                    }
                    if ctor_name.as_str() == "Handle" {
                        return Cow::Owned(Type::handle(a_applied));
                    }
                }

                Cow::Owned(Type::App(Box::new(f_applied), Box::new(a_applied)))
            }
            Type::TypeStageApp { fn_name, args } => Cow::Owned(Type::TypeStageApp {
                fn_name: fn_name.clone(),
                args: args
                    .iter()
                    .map(|arg| self.apply_type(arg, depth + 1, visited_types).into_owned())
                    .collect(),
            }),
            Type::NominalVariant { tag, fields } => {
                let applied_fields = self.apply_row(fields, depth + 1, visited_types);
                Cow::Owned(Type::NominalVariant {
                    tag: tag.clone(),
                    fields: applied_fields,
                })
            }
            Type::TyCon(_) => Cow::Borrowed(ty), // TyCon is always concrete, no substitution needed
            // S-860 / T-1077 (S-861): apply_type for Recursive — recurse into the body.
            // The `var` binder name is a gensym'd μ-binder, not a unification variable, and
            // must NOT be looked up in the substitution. The body may contain TypeVar sentinels
            // placed by expand_named cycle detection (Step 4) that need substitution applied.
            // T-1077 specifies: var is NOT in the substitution namespace; recurse into body only.
            Type::Recursive { var, body } => {
                let applied_body = self.apply_type(body, depth + 1, visited_types);
                match applied_body {
                    Cow::Borrowed(_) => Cow::Borrowed(ty), // body unchanged — no clone needed
                    Cow::Owned(new_body) => Cow::Owned(Type::Recursive {
                        var: var.clone(),
                        body: Box::new(new_body),
                    }),
                }
            }
            Type::Operator(name) => {
                // Look up Operator variable in substitution map (local frame + parent chain)
                if visited_types.contains(name) {
                    return Cow::Borrowed(ty);
                }
                let bound_opt = self.lookup_in_chain(name);
                match bound_opt {
                    Some(bound) => {
                        visited_types.insert(name.clone());
                        let result = self.apply_type(&bound, 0, visited_types).into_owned();
                        visited_types.remove(name);

                        // PATH COMPRESSION for Operator chains: cache in local frame
                        if result != bound {
                            self.type_map
                                .borrow_mut()
                                .insert(name.clone(), result.clone());
                        }

                        Cow::Owned(result)
                    }
                    None => Cow::Owned(Type::Operator(name.clone())),
                }
            }
            // Primitive types (Int, Float, Bool, Str, etc.) have no type variables;
            // return a borrow to avoid cloning the whole type tree when substitution
            // does not apply. Cow::Borrowed eliminates the clone on the hot path.
            _ => Cow::Borrowed(ty),
        }
    }

    pub(crate) fn apply_row(
        &self,
        row: &Row,
        depth: usize,
        visited_types: &mut HashSet<String>,
    ) -> Row {
        if depth >= MAX_APPLY_DEPTH {
            return row.clone();
        }

        // Apply substitution to field types and to RowTail::Uniform key/value types.
        let new_fields: HashMap<String, Type> = row
            .fields
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    self.apply_type(v, depth + 1, visited_types).into_owned(),
                )
            })
            .collect();

        let new_tail = match &row.tail {
            crate::type_def::RowTail::Empty => crate::type_def::RowTail::Empty,
            crate::type_def::RowTail::Uniform { key, value } => {
                let new_key = key
                    .as_ref()
                    .map(|k| Box::new(self.apply_type(k, depth + 1, visited_types).into_owned()));
                let new_value = Box::new(
                    self.apply_type(value, depth + 1, visited_types)
                        .into_owned(),
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

    /// Test-only introspection: lookup a type variable binding in the type_map.
    /// Used in type checker tests for asserting substitution contents; not called from production code.
    /// For production access to substitution results, use `apply()` instead.
    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<Type> {
        self.type_map.borrow().get(name).cloned()
    }
}

impl Default for Substitution {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Substitution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Substitution")
            .field("type_map", &self.type_map)
            .field("creation_level", &self.creation_level)
            .field("has_parent", &self.parent.is_some())
            .field("name_counter", &self.name_counter.get())
            .finish()
    }
}

/// Clone a substitution frame.
///
/// The parent chain is shared via `Arc::clone` (not deep-copied). This is correct:
/// parent frames are shared across multiple child frames and must not be duplicated.
/// The local `type_map` is deep-cloned so that the cloned frame has independent bindings.
impl Clone for Substitution {
    fn clone(&self) -> Self {
        Self {
            type_map: std::cell::RefCell::new(self.type_map.borrow().clone()),
            parent: self.parent.clone(), // Arc::clone — shared parent chain
            creation_level: self.creation_level,
            // Copy the current counter value so that probe save/restore patterns
            // (which clone+restore the Substitution) correctly reset the counter
            // along with the rest of the frame's state.
            name_counter: std::cell::Cell::new(self.name_counter.get()),
        }
    }
}

/// PartialEq compares only `type_map` contents (matching prior semantics).
///
/// `parent` and `creation_level` are structural metadata, not semantic state.
/// Two substitutions are equal if they bind the same variables to the same types
/// in their local frames, regardless of their position in the scope chain.
/// This matches the prior derived `PartialEq` behavior (which had no parent field).
impl PartialEq for Substitution {
    fn eq(&self, other: &Self) -> bool {
        *self.type_map.borrow() == *other.type_map.borrow()
    }
}

// Row variable occurs check functions removed — BAS Step 4: no RowVar tails exist.
// row_var_occurs, row_var_occurs_in_type, row_var_occurs_pub, lower_row_var_levels_pub
// were all removed. Tests in types.rs that used these functions have been updated.

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
fn unify_rows(
    row1: &Row,
    row2: &Row,
    subst: &mut Substitution,
    state: &mut InferState,
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
            unify(ty1, ty2, subst, state, span.clone())?;
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
                unify(ty1, ty2, subst, state, span.clone())?;
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
                    if let Some(current_level) = state.levels.get_mut(var_name) {
                        *current_level = 0;
                    }
                }
            } else {
                // Both rows have concrete field types and no shared fields: structurally incompatible.
                return Err(TypeError::type_mismatch(
                    &Type::Record(row1.clone()),
                    &Type::Record(row2.clone()),
                    span,
                ));
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
            unify(v1, v2, subst, state, span.clone())?;

            // B-327: Unify key type constraints when both sides specify them.
            // When only one side specifies a key type (asymmetric), the unconstrained
            // side is implicitly compatible with any key type (Unknown semantics) and
            // no error is emitted — the keyed side's constraint is preserved in its row.
            if let (Some(k1_ty), Some(k2_ty)) = (k1, k2) {
                unify(k1_ty, k2_ty, subst, state, span.clone())?;
            }

            // UNIFY-UNIFORM steps 2-3: after unifying the value types, apply the
            // substitution to fixpoint and validate all named fields from both rows.
            //
            // After unify(v1, v2), v1 and v2 are the same type (one may be bound to the
            // other). Apply substitution to v1 to get the resolved value type.
            let v_fixed = subst.apply(v1);

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
                    unify(
                        &Type::TypeVar(alpha.clone(), 0),
                        &join,
                        subst,
                        state,
                        span.clone(),
                    )?;
                } else if !v_fixed.has_inference_vars() {
                    // Step 3: V is concrete — each named field Ti must be a subtype of V.
                    for field_ty in &all_fields {
                        let field_fixed = subst.apply(field_ty);
                        if !Type::is_subtype(&field_fixed, &v_fixed, Some(&state.tycon_env)) {
                            return Err(TypeError::new(
                                format!(
                                    "field type {field_fixed} does not conform to Uniform constraint {v_fixed}"
                                ),
                                span.clone(),
                            ));
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
            let v_fixed = subst.apply(value);

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
                unify(
                    &Type::TypeVar(alpha.clone(), 0),
                    &join,
                    subst,
                    state,
                    span.clone(),
                )?;
            } else if !v_fixed.has_inference_vars() {
                // V is concrete: each named field Ti must be a subtype of V.
                for field_ty in &field_types {
                    let field_fixed = subst.apply(field_ty);
                    if !Type::is_subtype(&field_fixed, &v_fixed, Some(&state.tycon_env)) {
                        return Err(TypeError::new(
                            format!(
                                "field type {field_fixed} does not conform to Uniform constraint {v_fixed}"
                            ),
                            span.clone(),
                        ));
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
            let current_level = state.levels.get(name).copied().unwrap_or(0);
            state
                .levels
                .insert(name.clone(), current_level.min(cap_level));
            found
        }
        Type::Record(row) => {
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
            let current_level = state.levels.get(name).copied().unwrap_or(0);
            state
                .levels
                .insert(name.clone(), current_level.min(cap_level));
            found
        }
        // Leaf types — no type variables to lower, no occurs check needed.
        // Exhaustive match ensures new compound types are not silently missed.
        Type::Int
        | Type::IntLiteral(_)
        | Type::Float
        | Type::Str
        | Type::StringLiteral(_)
        | Type::Bool
        | Type::Bytes
        | Type::Number
        | Type::Proxy
        | Type::Unknown
        | Type::Top
        | Type::Error
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
        Type::NominalVariant { tag: _, fields } => {
            let mut found = false;
            for ty in fields.fields.values() {
                found |= lower_levels_check_occurs(ty, occurs_name, cap_level, state);
            }
            // Lower levels through RowTail::Uniform key and value types
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
fn transfer_class_constraints(alpha: &str, beta: &str, state: &mut InferState) {
    // Collect all Class constraints on α (both single-param and MPTC).
    let alpha_constraints: Vec<(Arc<ClassDecl>, Vec<String>)> = state
        .constraints
        .iter()
        .filter_map(|c| match c {
            Constraint::Class { class, vars, .. } if vars.contains(&alpha.to_string()) => {
                Some((Arc::clone(class), vars.clone()))
            }
            _ => None,
        })
        .collect();
    if alpha_constraints.is_empty() {
        return;
    }

    // Transfer to β (deduplicated: only add if not already present).
    // F3 FIX: For MPTC constraints, substitute alpha→beta in the vars list.
    let beta_existing: HashSet<Vec<String>> = state
        .constraints
        .iter()
        .filter_map(|c| match c {
            Constraint::Class { class: _, vars, .. } if vars.contains(&beta.to_string()) => {
                Some(vars.clone())
            }
            _ => None,
        })
        .collect();

    for (class, vars) in alpha_constraints {
        // Substitute alpha → beta in vars list
        let renamed_vars: Vec<String> = vars
            .iter()
            .map(|v| {
                if v == alpha {
                    beta.to_string()
                } else {
                    v.clone()
                }
            })
            .collect();

        // F3 FIX: Check if the renamed constraint already exists (avoid duplicates)
        if !beta_existing.contains(&renamed_vars) {
            state.constraints.push(Constraint::Class {
                class,
                vars: renamed_vars,
                origin_name: None,
                origin_span: None,
            });
        }
    }
}

/// Shared binding logic for C-Var1 (Union) and C-Var2 (Intersection) unification arms.
///
/// Both arms have the same structure:
/// 1. Partition compound members into TypeVars and concrete members.
/// 2. Require exactly one TypeVar in the compound.
/// 3. Check whether the concrete side is already covered/satisfied by the non-var members.
/// 4. If not covered, bind the TypeVar to the concrete type.
///
/// The only difference between C-Var1 and C-Var2 is the coverage check direction:
/// - C-Var1 (Union, `is_union = true`):  covered iff `concrete <: member`
///   (the concrete type is already a member of the union — no binding needed).
/// - C-Var2 (Intersection, `is_union = false`): satisfied iff `member <: concrete`
///   (the intersection already implies the target — the TypeVar is unconstrained).
///
/// Returns `Ok(())` when the TypeVar is bound or the compound already covers the concrete type.
/// Returns `Err(TypeError::type_mismatch)` when the compound has != 1 TypeVar.
fn bind_single_type_var_from_compound(
    compound_members: &[Type],
    concrete: &Type,
    is_union: bool,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // Partition into TypeVars and non-TypeVar (concrete) members
    let type_vars: Vec<_> = compound_members
        .iter()
        .filter(|m| matches!(m, Type::TypeVar(_, _)))
        .collect();
    let concrete_members: Vec<_> = compound_members
        .iter()
        .filter(|m| !matches!(m, Type::TypeVar(_, _)))
        .collect();

    if type_vars.len() != 1 {
        // Zero TypeVars: no binding target; >1 TypeVars: ambiguous binding.
        // Neither case is handled conservatively — fall through to mismatch.
        // Reconstruct a representative compound for the error message.
        let representative = if is_union {
            Type::Union(compound_members.to_vec())
        } else {
            Type::Intersection(compound_members.to_vec())
        };
        return Err(TypeError::type_mismatch(concrete, &representative, span));
    }

    // Check whether the concrete side is already handled by the non-var members.
    let already_handled = if is_union {
        // C-Var1: concrete is subsumed by an existing non-var union member.
        concrete_members
            .iter()
            .any(|m| Type::is_subtype(concrete, m, Some(&state.tycon_env)))
    } else {
        // C-Var2: an existing non-var intersection member already implies the concrete target.
        concrete_members
            .iter()
            .any(|m| Type::is_subtype(m, concrete, Some(&state.tycon_env)))
    };

    if already_handled {
        return Ok(());
    }

    // Extract the single TypeVar name (the `!= 1` guard above ensures this unwrap is safe).
    let Type::TypeVar(var_name, _) = type_vars[0] else {
        unreachable!()
    };

    let alpha_level = state.levels.get(var_name).copied().unwrap_or(0);
    if lower_levels_check_occurs(concrete, var_name, alpha_level, state) {
        return Err(TypeError::new(
            format!("infinite type: {var_name} occurs in {concrete}"),
            span,
        ));
    }

    let concrete_promoted = promote_literal_for_constrained_var(var_name, concrete.clone(), state);
    check_constraints_on_var(var_name, &concrete_promoted, subst, state, span.clone())?;
    let var_level = state.levels.get(var_name).copied().unwrap_or(0);
    subst.bind_at_level(var_name.clone(), var_level, concrete_promoted);
    subst.check_size(span)
}

pub fn unify(
    a: &Type,
    b: &Type,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // Apply current substitution to both sides (Robinson step: chase bound vars).
    // Shared visited set avoids redundant allocation across both apply() calls.
    let mut visited_types = HashSet::new();
    let mut visited_rows = HashSet::new(); // kept for apply_with_visited API compatibility
    let a_substituted = subst.apply_with_visited(a, &mut visited_types, &mut visited_rows);
    visited_types.clear();
    let b_substituted = subst.apply_with_visited(b, &mut visited_types, &mut visited_rows);

    // Normalize both types (for TypeStageApp reduction)
    // subst is passed explicitly — NormCtxt no longer holds a reference to it, so there is
    // no immutable borrow conflict with the mutable reference used in the match arms below.
    // allow_eval is set to false inside unify to prevent runtime errors from propagating
    // into type inference (e.g., a failing resolver should produce a stuck TypeStageApp, not
    // a type error).
    let mut norm_ctx = crate::type_normalize::NormCtxt::new();
    norm_ctx.allow_eval = false;
    let a = crate::type_normalize::normalize(&a_substituted, subst, &mut norm_ctx);
    let b = crate::type_normalize::normalize(&b_substituted, subst, &mut norm_ctx);
    drop(norm_ctx);

    if a == b {
        return Ok(());
    }

    // Robinson (1965) invariant: after unifying X and Y, `subst` is extended with at most one
    // new binding (the TypeVar arm inserts exactly one entry into subst.type_map). Subsequent
    // calls to `unify` operate on the extended substitution via the `apply_with_visited` calls
    // at the top of each recursive invocation -- those calls chase the substitution chain and
    // return fully-walked types before the match. We do NOT re-apply `subst` to already-unified
    // terms between match arms because (a) the occurs check prevents cycles, so there are no
    // self-referential chains to chase, and (b) each arm receives pre-applied operands (a, b)
    // that are already walk-complete with respect to the substitution at entry time.
    match (&a, &b) {
        // Error absorption: unify(Error, T) = Ok(()) for all T.
        // Error is a sentinel for failed sub-expression inference; absorbing it silently
        // prevents cascade errors in parent expressions. No substitution is modified --
        // Error carries no information that should propagate to type variables.
        (Type::Error, _) | (_, Type::Error) => Ok(()),

        // Unknown-consistency with level zeroing: prevent generalization of Unknown-touched vars.
        // Unknown relates to other types via consistency, not unification. When Unknown meets
        // a type variable, we zero the variable's level to prevent generalization (Siek & Taha 2006).
        (Type::Unknown, Type::TypeVar(name, _)) => {
            state.levels.insert(name.clone(), 0);
            Ok(())
        }
        (Type::TypeVar(name, _), Type::Unknown) => {
            state.levels.insert(name.clone(), 0);
            Ok(())
        }
        (Type::Unknown, other) | (other, Type::Unknown) => {
            // Zero levels of all type/row vars in the non-Unknown side to prevent
            // over-generalization. E.g., unify(Unknown, Fn(TypeVar("b",3) -> Int))
            // must zero b's level so it won't be generalized.
            let mut type_vars = HashSet::new();
            other.collect_all_vars(&mut type_vars);
            for var in &type_vars {
                state.levels.insert(var.clone(), 0);
            }
            Ok(())
        }

        // Top unification: Top should not appear in unification positions (it's for checking only).
        // If it does appear, treat it like Unknown for now (accepting unification with anything).
        (Type::Top, Type::TypeVar(name, _)) => {
            state.levels.insert(name.clone(), 0);
            Ok(())
        }
        (Type::TypeVar(name, _), Type::Top) => {
            state.levels.insert(name.clone(), 0);
            Ok(())
        }
        (Type::Top, other) | (other, Type::Top) => {
            let mut type_vars = HashSet::new();
            other.collect_all_vars(&mut type_vars);
            for var in &type_vars {
                state.levels.insert(var.clone(), 0);
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
            let level_a = state.levels.get(name_a).copied().unwrap_or(0);
            let level_b = state.levels.get(name_b).copied().unwrap_or(0);

            // Bind the higher-level variable to the lower-level one.
            // If levels are equal, bind left-to-right for determinism.
            // bind_at_level routes the binding to the substitution frame whose
            // creation_level matches the TypeVar's level, keeping per-dict bindings local.
            if level_a >= level_b {
                // Bind name_a → TypeVar(name_b)
                transfer_class_constraints(name_a, name_b, state);
                subst.bind_at_level(
                    name_a.clone(),
                    level_a,
                    Type::TypeVar(name_b.clone(), level_b),
                );
            } else {
                // Bind name_b → TypeVar(name_a)
                transfer_class_constraints(name_b, name_a, state);
                subst.bind_at_level(
                    name_b.clone(),
                    level_b,
                    Type::TypeVar(name_a.clone(), level_a),
                );
            }
            subst.check_size(span)?;
            Ok(())
        }

        // U-VAR-LEVEL: bind α to τ, lower levels of all β ∈ FTV(τ) and all ρ ∈ FRV(τ)
        (Type::TypeVar(name, _), _) => {
            // Fused occurs check + level lowering: one tree walk, zero HashSet allocations.
            // lower_levels_check_occurs returns true if `name` appears in the type tree
            // (infinite-type guard), and simultaneously lowers all var levels to cap_level.
            let alpha_level = state.levels.get(name).copied().unwrap_or(0);
            if lower_levels_check_occurs(&b, name, alpha_level, state) {
                return Err(TypeError::new(
                    format!("infinite type: {name} occurs in {b}"),
                    span,
                ));
            }
            // Promote literal types when binding a constrained type variable.
            // Without this, `[+ 1 2]` would bind _t0 to IntLiteral(1) and then fail
            // to unify IntLiteral(1) with IntLiteral(2) for the second argument.
            let b = promote_literal_for_constrained_var(name, b, state);

            // CONSTRAINT TRANSFER: when binding α to β (both TypeVars or Operator), transfer Class
            // constraints from α to β instead of checking. β inherits α's obligations and will be
            // checked when β is bound to a concrete type. HasField constraints are NOT transferred
            // (they reference the dict variable, not the param).
            // bind_at_level routes the binding to the frame matching the TypeVar's creation level.
            if let Type::TypeVar(beta_name, _) = &b {
                transfer_class_constraints(name, beta_name, state);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                subst.bind_at_level(name.clone(), alpha_level, b);
            } else if let Type::Operator(beta_name) = &b {
                transfer_class_constraints(name, beta_name, state);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                subst.bind_at_level(name.clone(), alpha_level, b);
            } else {
                // Binding α to a concrete type — check constraints normally
                check_constraints_on_var(name, &b, subst, state, span.clone())?;
                subst.bind_at_level(name.clone(), alpha_level, b);
            }
            subst.check_size(span)?;
            Ok(())
        }
        // U-VAR-LEVEL-SYM: bind α to τ, lower levels of all β ∈ FTV(τ) and all ρ ∈ FRV(τ)
        (_, Type::TypeVar(name, _)) => {
            // Fused occurs check + level lowering: one tree walk, zero HashSet allocations.
            let alpha_level = state.levels.get(name).copied().unwrap_or(0);
            if lower_levels_check_occurs(&a, name, alpha_level, state) {
                return Err(TypeError::new(
                    format!("infinite type: {name} occurs in {a}"),
                    span,
                ));
            }
            // Promote literal types when binding a constrained type variable.
            let a = promote_literal_for_constrained_var(name, a, state);

            // CONSTRAINT TRANSFER: when binding α to β (both TypeVars or Operator), transfer Class
            // constraints from α to β instead of checking. β inherits α's obligations and will be
            // checked when β is bound to a concrete type. HasField constraints are NOT transferred
            // (they reference the dict variable, not the param).
            // bind_at_level routes the binding to the frame matching the TypeVar's creation level.
            if let Type::TypeVar(beta_name, _) = &a {
                transfer_class_constraints(name, beta_name, state);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                subst.bind_at_level(name.clone(), alpha_level, a);
            } else if let Type::Operator(beta_name) = &a {
                transfer_class_constraints(name, beta_name, state);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                subst.bind_at_level(name.clone(), alpha_level, a);
            } else {
                // Binding α to a concrete type — check constraints normally
                check_constraints_on_var(name, &a, subst, state, span.clone())?;
                subst.bind_at_level(name.clone(), alpha_level, a);
            }
            subst.check_size(span)?;
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
            let fresh = state.fresh_type_var();
            let opened_a = substitute_recvar(ba, va, &fresh);
            let opened_b = substitute_recvar(bb, vb, &fresh);
            unify(&opened_a, &opened_b, subst, state, span)
        }

        // Arm 4 (open-left): left is Recursive, right is a concrete type (not TypeVar — that
        // was caught by the U-VAR-LEVEL-SYM arm above; not Recursive — caught by Arm 3 above).
        // Open the left side with a fresh TypeVar and unify the opened body with the right.
        (Type::Recursive { var: va, body: ba }, _) => {
            let fresh = state.fresh_type_var();
            let opened_a = substitute_recvar(ba, va, &fresh);
            unify(&opened_a, &b, subst, state, span)
        }

        // Arm 5 (open-right): right is Recursive, left is a concrete type (not TypeVar — caught
        // above; not Recursive — caught by Arm 3 above).
        // Open the right side with a fresh TypeVar and unify with the left.
        (_, Type::Recursive { var: vb, body: bb }) => {
            let fresh = state.fresh_type_var();
            let opened_b = substitute_recvar(bb, vb, &fresh);
            unify(&a, &opened_b, subst, state, span)
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
        (Type::IntLiteral(_), Type::Int | Type::Number) | (Type::Int, Type::Number) => Ok(()),
        (Type::Int | Type::Number, Type::IntLiteral(_)) | (Type::Number, Type::Int) => Ok(()),
        (Type::Float, Type::Number) | (Type::Number, Type::Float) => Ok(()),
        (Type::StringLiteral(_), Type::Str) | (Type::Str, Type::StringLiteral(_)) => Ok(()),
        // Same-value literals: covered by the `a == b` early-return above.
        // Different-value literals of the same base type are NOT unifiable — they are distinct
        // singleton types with no subtype relationship between them. U-SUBSUME (the
        // is_subtype(a,b) || is_subtype(b,a) fallback) also fails because IntLiteral(n1)
        // is not a subtype of IntLiteral(n2) when n1≠n2, so U-SUBSUME produces a type mismatch
        // as well. Callers that need to accept either value must widen to Int/Str first (e.g.
        // via constrained type variables with promote_literal_for_constrained_var, or dict
        // field promotion in typecheck_dict).
        (
            Type::Function {
                params: p1,
                ret: r1,
                variadic: v1,
            },
            Type::Function {
                params: p2,
                ret: r2,
                variadic: v2,
            },
        ) => {
            // Special case: zero-param variadic is the "any function" type.
            // Function{params:[], ret:Unknown, variadic:true} unifies with any function that
            // has at least one parameter (concrete arity). It does NOT unify with zero-param
            // non-variadic (different semantics: zero-param variadic accepts any args, zero-param
            // non-variadic accepts exactly zero args).
            // This enables precise `fn?` type predicate narrowing (fn-narrowing-variadic sprint).
            let is_any_function_1 = p1.is_empty() && *v1;
            let is_any_function_2 = p2.is_empty() && *v2;

            // Apply special case when one side is zero-param variadic and the other has params.
            if is_any_function_1 && !p2.is_empty() {
                // Zero-param variadic unifies with any concrete-arity function.
                return unify(r1, r2, subst, state, span);
            }
            if is_any_function_2 && !p1.is_empty() {
                // Zero-param variadic unifies with any concrete-arity function (symmetric).
                return unify(r1, r2, subst, state, span);
            }

            if p1.len() != p2.len() {
                return Err(TypeError::new(
                    format!(
                        "arity mismatch: expected {} arguments, got {}",
                        p1.len(),
                        p2.len()
                    ),
                    span,
                ));
            }
            if v1 != v2 {
                return Err(TypeError::new(
                    format!(
                        "variadic mismatch: {} vs {}",
                        if *v1 { "variadic" } else { "non-variadic" },
                        if *v2 { "variadic" } else { "non-variadic" }
                    ),
                    span,
                ));
            }
            // Robinson invariant: sub-terms are passed without explicit apply() because
            // every recursive unify() call re-applies the accumulated substitution at its
            // own entry (via apply_with_visited at the top of this function). Bindings
            // from earlier parameter unifications are therefore visible to later ones via
            // the shared `subst` -- this is correct Robinson (1965) unification.
            for ((_name_a, ty_a), (_name_b, ty_b)) in p1.iter().zip(p2.iter()) {
                unify(ty_a, ty_b, subst, state, span.clone())?;
            }
            unify(r1, r2, subst, state, span)
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
        (Type::Negation(t1), Type::Negation(t2)) => unify(t1, t2, subst, state, span),

        // Negation disjointness: if T <: A, then T & ~A = Never (provably empty intersection).
        // We can statically reject this case without full RDNF normalization — if is_subtype(T, A)
        // holds, the intersection is provably Never. For all other cases (uncertain overlap), we
        // remain conservative and allow unification to succeed. Runtime value_matches_type handles
        // the residual constraint for `[@[[without T]] expr]` TypeAsserts.
        (concrete, Type::Negation(inner))
            if !matches!(concrete, Type::TypeVar(..) | Type::Unknown) =>
        {
            if Type::is_subtype(concrete, inner, Some(&state.tycon_env)) {
                Err(TypeError::new(
                    format!(
                        "cannot unify {} with ~{}: intersection is Never (T <: A implies T & ~A = \u{2205})",
                        concrete, inner
                    ),
                    span,
                ))
            } else {
                Ok(()) // conservative: may still be empty but can't prove it statically
            }
        }
        (Type::Negation(inner), concrete)
            if !matches!(concrete, Type::TypeVar(..) | Type::Unknown) =>
        {
            if Type::is_subtype(concrete, inner, Some(&state.tycon_env)) {
                Err(TypeError::new(
                    format!(
                        "cannot unify ~{} with {}: intersection is Never (T <: A implies T & ~A = \u{2205})",
                        inner, concrete
                    ),
                    span,
                ))
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
        // TyCon("Seq") does not unify with TyCon("Map") — distinct named constructors are
        // distinct types regardless of arity or structure.
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
                return Err(TypeError::type_mismatch(&a, &b, span));
            }
            // Names are equal. Verify Arc identity. NOTE: In the current architecture,
            // Type::TyCon carries a name string, so both lookups (n1 == n2) access the same
            // HashMap slot and always return the same Arc — the error branch below is currently
            // dead code. It becomes meaningful when Type::TyCon is changed to carry Arc<TyConDef>
            // directly (future migration), at which point two same-name TyCons from different
            // scope registrations (i.e., distinct TypeEnv frames once TyConEnv is threaded via
            // TypeEnv's parent chain) could have distinct Arcs. The check is scaffolding for
            // that migration (B-343 groundwork).
            let def1 = state.tycon_env.get(n1.as_str());
            let def2 = state.tycon_env.get(n2.as_str());
            match (def1, def2) {
                (Some(d1), Some(d2)) if !Arc::ptr_eq(d1, d2) => {
                    // Same name but different TyConDef objects — cross-scope shadowing.
                    Err(TypeError::new(
                        format!(
                            "type constructor '{n1}' refers to two distinct definitions \
                             (cross-scope shadowing): cannot unify"
                        ),
                        span,
                    ))
                }
                _ => {
                    // Both None (unknown TyCon), or same Arc (same definition) — unify.
                    Ok(())
                }
            }
        }

        // UNIFY-OPERATOR-TO-OPERATOR: bind higher-level Operator to lower-level Operator.
        // Follows Kiselyov L3 invariant (same as TypeVar-to-TypeVar at lines 1837-1860).
        (Type::Operator(m), Type::Operator(n)) if m != n => {
            let level_m = state.levels.get(m).copied().unwrap_or(0);
            let level_n = state.levels.get(n).copied().unwrap_or(0);

            // Bind the higher-level operator to the lower-level one.
            // If levels are equal, bind left-to-right for determinism.
            if level_m >= level_n {
                // Bind m → Operator(n)
                transfer_class_constraints(m, n, state);
                subst.bind_at_level(m.clone(), level_m, Type::Operator(n.clone()));
            } else {
                // Bind n → Operator(m)
                transfer_class_constraints(n, m, state);
                subst.bind_at_level(n.clone(), level_n, Type::Operator(m.clone()));
            }
            subst.check_size(span)?;
            Ok(())
        }

        // UNIFY-OPERATOR: bind type constructor variable m to a type T.
        // Occurs check prevents infinite kinds (m ∉ ftv(T)).
        // Kind check premise is deferred to hkt-kind-inference.
        (Type::Operator(m), _) => {
            // Fused occurs check + level lowering (Kiselyov L3 invariant for Operator variables)
            let alpha_level = state.levels.get(m).copied().unwrap_or(0);
            if lower_levels_check_occurs(&b, m, alpha_level, state) {
                return Err(TypeError::new(
                    format!("infinite type: operator variable {} occurs in {}", m, b),
                    span,
                ));
            }
            // CONSTRAINT TRANSFER: when binding m to TypeVar, transfer constraints
            // instead of checking. When binding to a concrete type, check constraints normally.
            // bind_at_level routes to the frame matching the Operator variable's creation level.
            if let Type::TypeVar(beta_name, _) = &b {
                transfer_class_constraints(m, beta_name, state);
                subst.bind_at_level(m.clone(), alpha_level, b.clone());
            } else {
                // Binding to concrete type — check constraints
                check_constraints_on_var(m, &b, subst, state, span.clone())?;
                subst.bind_at_level(m.clone(), alpha_level, b.clone());
            }
            subst.check_size(span)?;
            Ok(())
        }
        // UNIFY-OPERATOR-SYM: symmetric case
        (_, Type::Operator(m)) => {
            // Fused occurs check + level lowering (Kiselyov L3 invariant for Operator variables)
            let alpha_level = state.levels.get(m).copied().unwrap_or(0);
            if lower_levels_check_occurs(&a, m, alpha_level, state) {
                return Err(TypeError::new(
                    format!("infinite type: operator variable {} occurs in {}", m, a),
                    span,
                ));
            }
            // CONSTRAINT TRANSFER: when binding m to TypeVar, transfer constraints
            // instead of checking. When binding to a concrete type, check constraints normally.
            // bind_at_level routes to the frame matching the Operator variable's creation level.
            if let Type::TypeVar(beta_name, _) = &a {
                transfer_class_constraints(m, beta_name, state);
                subst.bind_at_level(m.clone(), alpha_level, a.clone());
            } else {
                // Binding to concrete type — check constraints
                check_constraints_on_var(m, &a, subst, state, span.clone())?;
                subst.bind_at_level(m.clone(), alpha_level, a.clone());
            }
            subst.check_size(span)?;
            Ok(())
        }

        // UNIFY-MAP: Map[K1, V1] ~ Map[K2, V2] — keys must be invariant, values covariant.
        // Map is represented as App(App(TyCon("Map"), K), V).
        // This arm intercepts before the general UNIFY-APP to enforce key invariance.
        (Type::App(_, _), Type::App(_, _)) if a.as_map().is_some() && b.as_map().is_some() => {
            let (map_k1, map_v1) = a.as_map().unwrap();
            let (map_k2, map_v2) = b.as_map().unwrap();
            let k1_resolved = subst.apply(map_k1);
            let k2_resolved = subst.apply(map_k2);

            match (&k1_resolved, &k2_resolved) {
                (Type::TypeVar(_, _), _) | (_, Type::TypeVar(_, _)) => {
                    unify(&k1_resolved, &k2_resolved, subst, state, span.clone())?;
                }
                (Type::Int, Type::Number)
                | (Type::Number, Type::Int)
                | (Type::Float, Type::Number)
                | (Type::Number, Type::Float) => {
                    return Err(TypeError::new(
                        format!(
                            "Map key types must be invariant: {} vs {}",
                            k1_resolved, k2_resolved
                        ),
                        span,
                    ));
                }
                _ if k1_resolved != k2_resolved => {
                    return Err(TypeError::new(
                        format!(
                            "Map key types must be invariant: {} vs {}",
                            k1_resolved, k2_resolved
                        ),
                        span,
                    ));
                }
                _ => {}
            }
            // Values are covariant (unify normally)
            let val1 = map_v1.clone();
            let val2 = map_v2.clone();
            unify(&val1, &val2, subst, state, span)
        }

        // UNIFY-APP: decompose App(f₁, a₁) vs App(f₂, a₂).
        // Unify constructors first, then apply resulting substitution and unify arguments.
        (Type::App(f1, a1), Type::App(f2, a2)) => {
            // Unify constructors
            unify(f1, f2, subst, state, span.clone())?;
            // Substitution from constructor unification is already in subst and will be
            // applied by the recursive unify() call (via apply_with_visited at the top).
            unify(a1, a2, subst, state, span)
        }

        // Record unification: delegate to row unification
        (Type::Record(row1), Type::Record(row2)) => unify_rows(row1, row2, subst, state, span),

        // NominalVariant unification: tags must match (nominal identity), then unify fields structurally
        (
            Type::NominalVariant {
                tag: tag1,
                fields: fields1,
            },
            Type::NominalVariant {
                tag: tag2,
                fields: fields2,
            },
        ) => {
            if tag1 != tag2 {
                return Err(TypeError::new(
                    format!(
                        "cannot unify nominal variants with different tags: {} and {}",
                        tag1, tag2
                    ),
                    span,
                ));
            }
            // Tags match — unify fields structurally
            unify_rows(fields1, fields2, subst, state, span)
        }

        // NominalVariant vs Record: never unifiable (nominal vs structural distinction)
        (Type::NominalVariant { tag, .. }, Type::Record(_)) => Err(TypeError::new(
            format!(
                "cannot unify nominal variant {} with structural record",
                tag
            ),
            span,
        )),
        (Type::Record(_), Type::NominalVariant { tag, .. }) => Err(TypeError::new(
            format!(
                "cannot unify structural record with nominal variant {}",
                tag
            ),
            span,
        )),

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
        (Type::Record(_), Type::Intersection(members))
            if members.iter().all(|m| matches!(m, Type::Record(_))) =>
        {
            let members = members.clone();
            for member in &members {
                unify(&a, member, subst, state, span.clone())?;
            }
            Ok(())
        }
        (Type::Intersection(members), Type::Record(_))
            if members.iter().all(|m| matches!(m, Type::Record(_))) =>
        {
            let members = members.clone();
            for member in &members {
                unify(member, &b, subst, state, span.clone())?;
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

        // [C-VAR1] (BAS constraint rewriting, conservative):
        // τ₁ ≤ τ₂ ∨ α  →  bind α to the concrete type when the non-var members
        // don't already cover τ₁.
        //
        // Full BAS would rewrite to: τ₁ & ~τ₂ ≤ α, binding α to Negation(τ₂) ∩ τ₁.
        // Conservative approximation: bind α directly to τ₁ when the non-var members
        // of the union are disjoint from τ₁ (i.e., τ₁ is not a subtype of any non-var member).
        // This handles the common case: `(Int, Union([Str, TypeVar(a)]))` → bind a = Int.
        //
        // Pattern: concrete type a, Union on side b with exactly one TypeVar
        (concrete, Type::Union(members)) if !concrete.has_inference_vars() => {
            let members = members.clone();
            bind_single_type_var_from_compound(&members, concrete, true, subst, state, span)
        }

        // Symmetric C-Var1: Union on the left, concrete on the right
        (Type::Union(members), concrete) if !concrete.has_inference_vars() => {
            let members = members.clone();
            bind_single_type_var_from_compound(&members, concrete, true, subst, state, span)
        }

        // [C-VAR2] (BAS constraint rewriting, conservative):
        // α & τ₁ ≤ τ₂  →  bind α to τ₂ when τ₁ doesn't already satisfy τ₂.
        //
        // Full BAS would rewrite to: α ≤ τ₂ | ~τ₁.
        // Conservative approximation: bind α directly to τ₂ when the non-var members
        // of the intersection don't already imply τ₂.
        //
        // Pattern: Intersection with exactly one TypeVar on one side, concrete on the other
        (Type::Intersection(members), concrete) if !concrete.has_inference_vars() => {
            let members = members.clone();
            bind_single_type_var_from_compound(&members, concrete, false, subst, state, span)
        }

        // Symmetric C-Var2: concrete on the left, Intersection on the right
        (concrete, Type::Intersection(members)) if !concrete.has_inference_vars() => {
            let members = members.clone();
            bind_single_type_var_from_compound(&members, concrete, false, subst, state, span)
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
                return Err(TypeError::new(
                    format!(
                        "TypeStageApp arity mismatch: {} expects {} args, got {}",
                        f1,
                        a1.len(),
                        a2.len()
                    ),
                    span,
                ));
            }
            for (arg1, arg2) in a1.iter().zip(a2.iter()) {
                unify(arg1, arg2, subst, state, span.clone())?;
            }
            Ok(())
        }
        // Case 2: different function names -> error
        (Type::TypeStageApp { fn_name: f1, .. }, Type::TypeStageApp { fn_name: f2, .. }) => {
            Err(TypeError::new(
                format!(
                    "cannot unify TypeStageApp with different resolvers: {} vs {}",
                    f1, f2
                ),
                span,
            ))
        }
        // Case 3: TypeStageApp vs concrete (non-TypeVar, non-Unknown, non-Top)
        // Defer to process_deferred_equalities (no resolvers available yet in chr-normalization)
        // In chr-prelude, this will attempt resolver evaluation before deferring
        (Type::TypeStageApp { .. }, concrete) | (concrete, Type::TypeStageApp { .. })
            if !matches!(concrete, Type::TypeVar(..) | Type::Unknown | Type::Top) =>
        {
            state.deferred_equalities.push((a.clone(), b.clone()));
            Ok(())
        }

        // [U-SUBSUME]: concrete type subsumption fallback (Pierce & Turner 2000)
        // When both sides are ground types (no type variables), check the subtype
        // relation in both directions. Bidirectional because unification is symmetric --
        // the original actual/expected roles are lost after structural decomposition.
        // The substitution is not modified (no variables to bind).
        _ if !a.has_inference_vars() && !b.has_inference_vars() => {
            if Type::is_subtype(&a, &b, Some(&state.tycon_env))
                || Type::is_subtype(&b, &a, Some(&state.tycon_env))
            {
                Ok(())
            } else {
                Err(TypeError::type_mismatch(&a, &b, span))
            }
        }

        _ => Err(TypeError::type_mismatch(&a, &b, span)),
    }
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
pub fn process_deferred_equalities(state: &mut InferState, subst: &mut Substitution, span: Span) {
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
        let mut norm_ctx = crate::type_normalize::NormCtxt::new();
        for (a, b) in deferred {
            // Normalize both sides
            let a_norm = crate::type_normalize::normalize(&a, subst, &mut norm_ctx);
            let b_norm = crate::type_normalize::normalize(&b, subst, &mut norm_ctx);

            if !a_norm.has_type_stage_app() && !b_norm.has_type_stage_app() {
                // Both sides fully reduced — attempt unification.
                // F10 FIX: Emit diagnostic on unification failure instead of silently dropping.
                match unify(&a_norm, &b_norm, subst, state, span.clone()) {
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
