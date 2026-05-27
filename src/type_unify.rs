//! Substitution, unification, and constraint solving for Hindley-Milner polymorphism
//! with Boolean-Algebraic Subtyping (BAS) and structural record types.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::ast::Span;

use super::*;

/// Maximum recursion depth for constraint satisfaction checking.
/// Prevents infinite loops when checking constraints on recursive types.
const MAX_CONSTRAINT_DEPTH: usize = 256;

/// Check if a type satisfies a type class constraint.
/// Returns true if the type is an instance of the class.
///
/// `Numeric`, `Comparable`, `Equatable`, and `Showable` are handled here via fixed
/// instance sets for primitives, plus structural propagation for Record types.
/// All other classes (Mappable, Appendable) are resolved dynamically via
/// `InstanceEnv::resolve_instance` in `check_constraints_on_var`.
/// This requires prelude.llt instances to be propagated into the `InferState`
/// (done by `imports::seed_infer_state_from_prelude_cache`).
///
/// KNOWN ISSUE (F5): Hardcoded instance sets may diverge from InstanceEnv if prelude
/// instances are added/modified without updating this function. The correct long-term
/// solution is to make ALL constraint satisfaction go through InstanceEnv (eliminating
/// hardcoded sets entirely), but this requires seeding the InstanceEnv before early-stage
/// operator type inference runs. Deferred to chr-eliminate-hardcoded-instances sprint.
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
            _ => {} // Fall through to instance resolution
        }
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

    match class_name {
        // Equatable: base class for equality ([= $a $b]).
        // Hardcoded because prelude instance declarations for primitives are commented
        // out (primitives use Rust fallback dispatch). Without this, [= x 42] triggers
        // "type Int does not satisfy constraint Equatable" in narrowing tests.
        // Variant added for Group E fix (transport_typed.llt-eval).
        "Equatable" => matches!(
            ty,
            Type::Int
                | Type::IntLiteral(_)
                | Type::Float
                | Type::Str
                | Type::StringLiteral(_)
                | Type::Bool
                | Type::Number
                | Type::NominalVariant { .. }
        ),
        // Showable: base class for string conversion. Hardcoded for primitives that
        // have built-in str conversion. Combined with structural propagation above,
        // this means Record([x: Int, y: Str]) satisfies Showable because both Int
        // and Str satisfy Showable and the constraint propagates through all fields.
        // Seq, Map, and Record are also showable (they have runtime str conversion).
        "Showable" => matches!(
            ty,
            Type::Int
                | Type::IntLiteral(_)
                | Type::Float
                | Type::Str
                | Type::StringLiteral(_)
                | Type::Bool
                | Type::Number
                | Type::Seq(_)
                | Type::Map(_, _)
                | Type::Record(_)
        ),
        // Comparable subsumes Equatable via superclass relationship.
        // These are kept hardcoded because they are used in the early stages of type
        // checking (before prelude instances are loaded) and during operator type
        // inference ([< $a $b], [> $a $b], etc.).
        "Comparable" => matches!(
            ty,
            Type::Int
                | Type::IntLiteral(_)
                | Type::Float
                | Type::Str
                | Type::StringLiteral(_)
                | Type::Number
        ),
        // Numeric is kept hardcoded: arithmetic operators ([+ $a $b], [* $a $b], etc.)
        // require Numeric constraint checking during core type inference, before prelude
        // instances are loaded. Removing this would break basic arithmetic type checking.
        "Numeric" => matches!(
            ty,
            Type::Int | Type::IntLiteral(_) | Type::Float | Type::Number
        ),
        _ => false, // All other classes resolved via InstanceEnv::resolve_instance
    }
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
                })
            }
            _ => None,
        })
        .collect();

    for constraint in applicable {
        match constraint {
            ApplicableConstraint::SingleParam { class } => {
                // Single-parameter type class constraint (e.g., Numeric a)
                // First, check the fixed instance sets (B4 constrained type variables)
                if satisfies_constraint(concrete_ty, &class) {
                    continue;
                }

                // If not in fixed instance set, try instance resolution
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
                        span,
                    ));
                }
                state.instance_resolution_depth += 1;
                let inst_env = state.instance_env.clone();
                let instance_found = inst_env
                    .resolve_instance(&class, concrete_ty, state)
                    .is_some();
                state.instance_resolution_depth -= 1;

                if instance_found {
                    // Instance found - constraint satisfied
                    continue;
                }

                // No instance found - constraint violated
                return Err(TypeError::new(
                    format!("type {} does not satisfy constraint {}", concrete_ty, class),
                    span,
                ));
            }
            ApplicableConstraint::MultiParam {
                class,
                vars,
                fundeps,
            } => {
                // Multi-parameter type class constraint with functional dependencies.
                // Check if this variable binding triggers FD improvement.
                improve_functional_dependency(
                    &class,
                    &vars,
                    &fundeps,
                    var_name,
                    concrete_ty,
                    subst,
                    state,
                    span,
                )?;
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
        class, vars, fundeps, bound_var, bound_type, subst, state, span,
    );
    state.fd_depth -= 1;
    result
}

#[allow(clippy::too_many_arguments)] // FD improvement requires all constraint components
fn improve_functional_dependency_inner(
    class: &str,
    vars: &[String],
    fundeps: &[(Vec<usize>, Vec<usize>)],
    bound_var: &str,
    bound_type: &Type,
    subst: &Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // For each functional dependency (determining → determined)
    for (det_positions, ded_positions) in fundeps {
        // Check if the bound variable is in a determining position
        let bound_var_positions: Vec<usize> = vars
            .iter()
            .enumerate()
            .filter(|(_, v)| *v == bound_var)
            .map(|(i, _)| i)
            .collect();

        if !bound_var_positions
            .iter()
            .any(|p| det_positions.contains(p))
        {
            // This binding doesn't affect this FD
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

        // Special case: Indexable on Record/Union types uses direct field lookup
        // instead of instance registration (records are structural, not nominal).
        let indexable_record_result = if class == "Indexable" && det_types.len() == 2 {
            let container_ty = &det_types[0].2;
            let key_ty = &det_types[1].2;

            match (container_ty, key_ty) {
                (Type::Record(row), Type::StringLiteral(field_name)) => {
                    // Direct field lookup for Record + StringLiteral key
                    Some(row.fields.get(field_name).cloned().unwrap_or(Type::Unknown))
                }
                (Type::Record(_), Type::Str) => {
                    // Str key (from promoted StringLiteral) — can't resolve statically
                    Some(Type::Unknown)
                }
                (Type::Union(members), Type::StringLiteral(field_name)) => {
                    // [HAS-FIELD-UNION]: distribute field lookup across union members.
                    // [get key (A | B)] → get(key, A) | get(key, B)
                    // Each member that has the field contributes its field type.
                    // Members without the field contribute Unknown (graceful degradation).
                    let field_types: Vec<Type> = members
                        .iter()
                        .map(|member| match member {
                            Type::Record(row) => {
                                row.fields.get(field_name).cloned().unwrap_or(Type::Unknown)
                            }
                            _ => Type::Unknown,
                        })
                        .collect();
                    Some(Type::normalize_union(field_types))
                }
                _ => None, // Not a Record/Union case — fall through to general logic
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
                span,
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
                                                span,
                                            )
                                        })?,
                                    None => {
                                        return Err(TypeError::new(
                                            format!(
                                                "no instance for {} (no determined position found)",
                                                class
                                            ),
                                            span,
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
                                    span,
                                ));
                            }
                        }
                    }
                    None => {
                        // KNOWN ISSUE: Indexable falls back to Unknown when no instance matches
                        // (e.g., unknown container type or TypeVar container). The old check_get
                        // returned Unknown in these cases; restoring that behavior here prevents
                        // false-positive "no instance for Indexable" errors for unresolvable
                        // containers. Determined TypeVar remains unbound → resolves to Unknown.
                        // Non-Indexable MPTC classes still report errors on lookup failure.
                        if class == "Indexable" {
                            continue;
                        }
                        return Err(TypeError::new(format!("no instance for {}", class), span));
                    }
                }
            }
        } else {
            // Class not found in class_env — should not happen
            return Err(TypeError::new(format!("unknown class {}", class), span));
        };

        // Unify each determined position with the result type
        for &ded_pos in ded_positions {
            if ded_pos >= vars.len() {
                continue;
            }
            let ded_var = &vars[ded_pos];
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
            let mut local_subst = std::mem::take(&mut state.subst);
            let result = unify(&ded_type_var, &result_type, &mut local_subst, state, span);
            state.subst = local_subst;
            result?;
        }
    }

    Ok(())
}

/// Returns true if a type is provably non-arithmetic — i.e., it cannot possibly be a
/// numeric type at runtime and therefore cannot satisfy any arithmetic type class instance
/// (Addable, Subtractable, Multipliable, Divisible).
///
/// Conservative: the following types are NOT flagged (they pass the check):
/// - `Unknown`: gradual typing escape hatch — may be numeric at runtime.
/// - `TypeVar`: unconstrained polymorphic variable — may unify to a numeric type.
/// - `Top`: ⊤ is the lattice ceiling; not a concrete value, leave to runtime.
/// - `Error`: cascade sentinel — the sub-expression already failed; don't double-report.
/// - `Never`: ⊥ is uninhabited; constraint is vacuously satisfied.
/// - `Number`, `Int`, `Float`, `IntLiteral`: these ARE arithmetic types.
///
/// Everything else (Str, StringLiteral, Bool, Record, Seq, Handle, etc.) is provably
/// non-arithmetic and returns `true`.
///
/// Mirrors `is_definitely_non_numeric` from `typecheck.rs` but lives here so it can
/// be called from `check_constraints_on_var` during unification (type_unify.rs is a
/// submodule of types.rs and cannot import from typecheck.rs).
#[allow(dead_code)]
fn is_definitely_non_arithmetic(ty: &Type) -> bool {
    match ty {
        // These types cannot be proven non-arithmetic — conservative pass
        Type::Unknown
        | Type::TypeVar(_, _)
        | Type::Top
        | Type::Error
        | Type::Never
        | Type::Number
        | Type::Int
        | Type::Float
        | Type::IntLiteral(_) => false,
        // Everything else is provably non-arithmetic
        _ => true,
    }
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
            ("Number", "Number")
            | ("Number", "Int")
            | ("Int", "Number")
            | ("Number", "Float")
            | ("Float", "Number") => Ok(Type::Number),
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
            ("Number", "Number")
            | ("Number", "Int")
            | ("Int", "Number")
            | ("Number", "Float")
            | ("Float", "Number") => Ok(Type::Number),
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
                let field_ty = resolve_has_field(label, member, state, span, depth + 1)?;
                field_types.push(field_ty);
            }
            Ok(Type::normalize_union(field_types))
        }

        // [HAS-FIELD-INTER]: Intersection → all members must have field, return Intersection of field types
        Type::Intersection(members) => {
            let mut field_types = Vec::new();
            for member in members {
                let field_ty = resolve_has_field(label, member, state, span, depth + 1)?;
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

#[derive(Debug, Clone, PartialEq)]
pub struct Substitution {
    pub type_map: std::cell::RefCell<HashMap<String, Type>>, // α → τ  (kind: Type)
}

const MAX_APPLY_DEPTH: usize = 256;

/// Maximum size of the substitution map (type_map entries).
/// Prevents resource exhaustion from quadratic growth in pathological cases.
/// Raised from 10K to 50K to accommodate real-world K8s-style configs with
/// hundreds of open-record dot-accesses that each bind a fresh type variable.
pub const MAX_SUBST_SIZE: usize = 50_000;

impl Substitution {
    /// Create a new empty substitution.
    ///
    /// Performance note: `HashMap::new()` creates a map with zero capacity
    /// and performs no heap allocation until the first insert. This is optimal
    /// for fully-concrete dicts that generate no unification constraints.
    pub fn new() -> Self {
        Self {
            type_map: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// Check if the substitution is empty (no bindings).
    /// Used to guard against unnecessary allocation in apply() operations.
    pub fn is_empty(&self) -> bool {
        self.type_map.borrow().is_empty()
    }

    /// Check if the substitution has exceeded the maximum allowed size.
    /// Returns an error if type_map exceeds MAX_SUBST_SIZE.
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
        let mut visited_types = HashSet::new();
        self.apply_type(ty, 0, &mut visited_types).into_owned()
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
                // Look up the binding for this TypeVar
                let bound_opt = self.type_map.borrow().get(name).cloned();
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
                        // update the map to point directly to the final result. This collapses chains
                        // like t0 → t1 → t2 → Int into t0 → Int, t1 → Int after first traversal.
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
            Type::Seq(elem) => Cow::Owned(Type::Seq(Box::new(
                self.apply_type(elem, depth + 1, visited_types).into_owned(),
            ))),
            Type::Map(key, val) => Cow::Owned(Type::Map(
                Box::new(self.apply_type(key, depth + 1, visited_types).into_owned()),
                Box::new(self.apply_type(val, depth + 1, visited_types).into_owned()),
            )),
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

                // Normalize App(concrete_constructor, T) to builtin forms (hkt-kind-inference Task 5)
                // When an Operator TypeVar resolves to a concrete constructor name, normalize to
                // the corresponding builtin type variant to maintain type system invariants.
                if let Type::Operator(ctor_name) = &f_applied {
                    if ctor_name.as_str() == "Seq" {
                        return Cow::Owned(Type::Seq(Box::new(a_applied)));
                    }
                }

                // Normalize App(App(Operator("Map"), K), V) → Type::Map(K, V) (Task 6)
                // Map has kind (* → * → *), so full application is App(App(Map, K), V)
                if let Type::App(inner_f, k) = &f_applied {
                    if let Type::Operator(ctor_name) = inner_f.as_ref() {
                        if ctor_name == "Map" {
                            return Cow::Owned(Type::Map(k.clone(), Box::new(a_applied)));
                        }
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
            Type::Handle(cap) => Cow::Owned(Type::Handle(Box::new(
                self.apply_type(cap, depth + 1, visited_types).into_owned(),
            ))),
            Type::Operator(name) => {
                // Look up Operator variable in substitution map
                if visited_types.contains(name) {
                    return Cow::Borrowed(ty);
                }
                let bound_opt = self.type_map.borrow().get(name).cloned();
                match bound_opt {
                    Some(bound) => {
                        visited_types.insert(name.clone());
                        let result = self.apply_type(&bound, 0, visited_types).into_owned();
                        visited_types.remove(name);

                        // PATH COMPRESSION for Operator chains as well
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

        // Apply substitution to field types only (no row variable tails under BAS).
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

        Row { fields: new_fields }
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

// Row variable occurs check functions removed — BAS Step 4: no RowVar tails exist.
// row_var_occurs, row_var_occurs_in_type, row_var_occurs_pub, lower_row_var_levels_pub
// were all removed. Tests in types.rs that used these functions have been updated.

/// BAS record unification: unify only the fields shared between both rows.
/// Fields unique to one row are ignored — BAS width subtyping handles openness
/// via is_subtype (a record with MORE fields satisfies an annotation with FEWER fields).
///
/// This replaces the full Rémy-style Wand 4-case algorithm. Under BAS there are no
/// RowVar tails to bind, so unification is simply: for each field in both rows, unify types.
fn unify_rows(
    row1: &Row,
    row2: &Row,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // Fast-path: identical field sets — avoid collecting intersection.
    if row1.fields.len() == row2.fields.len()
        && row1.fields.keys().all(|k| row2.fields.contains_key(k))
    {
        for (key, ty1) in &row1.fields {
            let ty2 = &row2.fields[key];
            unify(ty1, ty2, subst, state, span)?;
        }
        return Ok(());
    }

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
            unify(ty1, ty2, subst, state, span)?;
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
            // Lower all TypeVars to level 0 to prevent unsound generalization.
            let mut vars = HashSet::new();
            for ty in row1.fields.values() {
                ty.collect_type_vars(&mut vars);
            }
            for ty in row2.fields.values() {
                ty.collect_type_vars(&mut vars);
            }
            for var_name in vars {
                if let Some(current_level) = state.levels.get_mut(&var_name) {
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

    Ok(())
}

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
            // BAS: RowTail::Empty — no row var to lower
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
        Type::Seq(elem) => lower_levels_check_occurs(elem, occurs_name, cap_level, state),
        Type::Map(key, val) => {
            let mut found = false;
            found |= lower_levels_check_occurs(key, occurs_name, cap_level, state);
            found |= lower_levels_check_occurs(val, occurs_name, cap_level, state);
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
            found
        }
        Type::Handle(cap) => lower_levels_check_occurs(cap, occurs_name, cap_level, state),
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
            .any(|m| Type::is_subtype(concrete, m))
    } else {
        // C-Var2: an existing non-var intersection member already implies the concrete target.
        concrete_members
            .iter()
            .any(|m| Type::is_subtype(m, concrete))
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
    check_constraints_on_var(var_name, &concrete_promoted, subst, state, span)?;
    subst
        .type_map
        .borrow_mut()
        .insert(var_name.clone(), concrete_promoted);
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
            if level_a >= level_b {
                // Bind name_a → TypeVar(name_b)
                transfer_class_constraints(name_a, name_b, state);
                subst
                    .type_map
                    .borrow_mut()
                    .insert(name_a.clone(), Type::TypeVar(name_b.clone(), level_b));
            } else {
                // Bind name_b → TypeVar(name_a)
                transfer_class_constraints(name_b, name_a, state);
                subst
                    .type_map
                    .borrow_mut()
                    .insert(name_b.clone(), Type::TypeVar(name_a.clone(), level_a));
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
            if let Type::TypeVar(beta_name, _) = &b {
                transfer_class_constraints(name, beta_name, state);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                subst.type_map.borrow_mut().insert(name.clone(), b);
            } else if let Type::Operator(beta_name) = &b {
                transfer_class_constraints(name, beta_name, state);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                subst.type_map.borrow_mut().insert(name.clone(), b);
            } else {
                // Binding α to a concrete type — check constraints normally
                check_constraints_on_var(name, &b, subst, state, span)?;
                subst.type_map.borrow_mut().insert(name.clone(), b);
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
            if let Type::TypeVar(beta_name, _) = &a {
                transfer_class_constraints(name, beta_name, state);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                subst.type_map.borrow_mut().insert(name.clone(), a);
            } else if let Type::Operator(beta_name) = &a {
                transfer_class_constraints(name, beta_name, state);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                subst.type_map.borrow_mut().insert(name.clone(), a);
            } else {
                // Binding α to a concrete type — check constraints normally
                check_constraints_on_var(name, &a, subst, state, span)?;
                subst.type_map.borrow_mut().insert(name.clone(), a);
            }
            subst.check_size(span)?;
            Ok(())
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
                unify(ty_a, ty_b, subst, state, span)?;
            }
            unify(r1, r2, subst, state, span)
        }

        (Type::Seq(elem1), Type::Seq(elem2)) => unify(elem1, elem2, subst, state, span),

        (Type::Map(k1, v1), Type::Map(k2, v2)) => {
            // Map keys must be invariant: Map[Int, Str] ≠ Map[Number, Str]
            // Apply substitution to resolve TypeVars, then check structural equality.
            // For TypeVars, unify them; for concrete types, enforce invariance.
            let k1_resolved = subst.apply(k1);
            let k2_resolved = subst.apply(k2);

            match (&k1_resolved, &k2_resolved) {
                // If either is still a TypeVar, unify them
                (Type::TypeVar(_, _), _) | (_, Type::TypeVar(_, _)) => {
                    unify(&k1_resolved, &k2_resolved, subst, state, span)?;
                }
                // For concrete types, enforce strict invariance (no Int <: Number subsumption)
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
                // For all other concrete types, check structural equality
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
            unify(v1, v2, subst, state, span)
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
            if Type::is_subtype(concrete, inner) {
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
            if Type::is_subtype(concrete, inner) {
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
        // Handle: unify capability rows
        (Type::Handle(cap_a), Type::Handle(cap_b)) => unify(cap_a, cap_b, subst, state, span),

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
            // CONSTRAINT TRANSFER: when binding m to another Operator or TypeVar, transfer constraints
            // instead of checking. When binding to a concrete type, check constraints normally.
            if let Type::Operator(n_name) = &b {
                transfer_class_constraints(m, n_name, state);
                subst.type_map.borrow_mut().insert(m.clone(), b.clone());
            } else if let Type::TypeVar(beta_name, _) = &b {
                transfer_class_constraints(m, beta_name, state);
                subst.type_map.borrow_mut().insert(m.clone(), b.clone());
            } else {
                // Binding to concrete type — check constraints
                check_constraints_on_var(m, &b, subst, state, span)?;
                subst.type_map.borrow_mut().insert(m.clone(), b.clone());
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
            // CONSTRAINT TRANSFER: when binding m to another Operator or TypeVar, transfer constraints
            // instead of checking. When binding to a concrete type, check constraints normally.
            if let Type::Operator(n_name) = &a {
                transfer_class_constraints(m, n_name, state);
                subst.type_map.borrow_mut().insert(m.clone(), a.clone());
            } else if let Type::TypeVar(beta_name, _) = &a {
                transfer_class_constraints(m, beta_name, state);
                subst.type_map.borrow_mut().insert(m.clone(), a.clone());
            } else {
                // Binding to concrete type — check constraints
                check_constraints_on_var(m, &a, subst, state, span)?;
                subst.type_map.borrow_mut().insert(m.clone(), a.clone());
            }
            subst.check_size(span)?;
            Ok(())
        }

        // UNIFY-APP: decompose App(f₁, a₁) vs App(f₂, a₂).
        // Unify constructors first, then apply resulting substitution and unify arguments.
        (Type::App(f1, a1), Type::App(f2, a2)) => {
            // Unify constructors
            unify(f1, f2, subst, state, span)?;
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
                unify(&a, member, subst, state, span)?;
            }
            Ok(())
        }
        (Type::Intersection(members), Type::Record(_))
            if members.iter().all(|m| matches!(m, Type::Record(_))) =>
        {
            let members = members.clone();
            for member in &members {
                unify(member, &b, subst, state, span)?;
            }
            Ok(())
        }

        // Union vs Union: defer when both sides have inference vars (TypeVars, row vars, TypeStageApp).
        // This prevents hard errors from Union([Int, TypeVar(a)]) ~ Union([Str, TypeVar(b)]).
        // Conservative approximation: the constraint is dropped if TypeVars get bound elsewhere
        // through other unification paths. See `process_deferred_equalities` for future improvement
        // (currently not called; enabling it requires a stable call site after dict-level inference).
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
                unify(arg1, arg2, subst, state, span)?;
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
            if Type::is_subtype(&a, &b) || Type::is_subtype(&b, &a) {
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
                match unify(&a_norm, &b_norm, subst, state, span) {
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
                            span,
                            code: "T013",
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
