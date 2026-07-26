//! Unification, constraint solving, and substitution application for Hindley-Milner
//! polymorphism with Boolean-Algebraic Subtyping (BAS) and structural record types.
//!
//! Type variable bindings are stored in `InferState.type_vars` (an `IndexMap<String, TypeVarEntry>`),
//! with each `TypeVarEntry` holding the variable's level, binding, and kind.
//! `Substitution` (in `type_infer.rs`) is a finite renaming map used by `instantiate_scheme`
//! to rename quantified variables to fresh names. `InferState.subst` is the global accumulated
//! substitution for constraint propagation during inference.

use indexmap::IndexMap;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

use crate::ast::Span;
use crate::error::TypeDiagnostic;
use crate::type_def::substitute_recvar;
use crate::type_infer::TypeVarEntry;

use super::*;

/// Maximum recursion depth for constraint satisfaction checking.
/// Prevents infinite loops when checking constraints on recursive types.
const MAX_CONSTRAINT_DEPTH: usize = 256;

/// Maximum recursion depth for unification.
/// Prevents stack overflow on deeply nested type unification.
const MAX_UNIFY_DEPTH: usize = 512;

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
    satisfies_constraint_inner(ty, class_name, &mut 0)
}

/// Internal implementation of constraint satisfaction with depth tracking.
/// Conservative: returns false if depth limit exceeded (treat as constraint not satisfied).
/// `depth` is a mutable counter incremented on each recursive call and checked against MAX_CONSTRAINT_DEPTH.
fn satisfies_constraint_inner(ty: &Type, class_name: &str, depth: &mut usize) -> bool {
    // Depth guard: prevent unbounded recursion on pathological recursive types
    if *depth >= MAX_CONSTRAINT_DEPTH {
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
        return members.iter().all(|member| {
            *depth += 1;
            let result = satisfies_constraint_inner(member, class_name, depth);
            *depth -= 1;
            result
        });
    }

    // [CONSTRAIN-INTER]: C(τ₁ & τ₂) ⊢ satisfied iff C(τ₁) ∧ C(τ₂) (ALL members).
    if let Type::Intersection(members) = ty {
        return members.iter().all(|member| {
            *depth += 1;
            let result = satisfies_constraint_inner(member, class_name, depth);
            *depth -= 1;
            result
        });
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
    depth: usize,
) -> Result<(), TypeDiagnostic> {
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
            /// hold concrete types that were resolved before generalization.
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
                        let field_var_ty = Type::Var(field_var.clone(), state.level);
                        let mut sub_constraints = Vec::new();
                        Box::pin(unify(
                            &field_var_ty,
                            &field_ty,
                            state,
                            &mut sub_constraints,
                            span.clone(),
                            depth + 1,
                        ))
                        .await?;
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
                                return Err(TypeDiagnostic::error("type-error",
                                    "open dict (Dict) does not satisfy Record — Record requires a closed dict with known fields; use @Dict to accept any dict".to_string(),
                                    span.clone(),
                                ));
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
                    return Err(TypeDiagnostic::error("type-error",
                        format!(
                            "instance resolution depth limit exceeded (max {}) — possible recursive instance definitions for constraint {}",
                            MAX_INSTANCE_RESOLUTION_DEPTH,
                            class
                        ),
                        span.clone(),
                    ));
                }
                state.instance_resolution_depth += 1;
                let inst_env = state.get_working_instance_env();
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
                            let inst_env2 = state.get_working_instance_env();
                            state.instance_resolution_depth += 1;
                            let retry_result =
                                Box::pin(inst_env2.resolve_instance(&class, &widened, state)).await;
                            state.instance_resolution_depth -= 1;
                            if let Ok(Some(_)) = retry_result {
                                continue;
                            }
                        }
                        // No instance found even after widening - constraint violated
                        return Err(TypeDiagnostic::error(
                            "type-error",
                            format!("type {} does not satisfy constraint {}", concrete_ty, class),
                            span.clone(),
                        ));
                    }
                    Err(ambig_msg) => {
                        // Ambiguous instances — equally specific matches, coherence violation
                        return Err(TypeDiagnostic::error("type-error", ambig_msg, span.clone()));
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
                let fd_ctx = FdImprovementCtx {
                    class: &class,
                    args: &args,
                    fundeps: &fundeps,
                    resolver_injective,
                    bound_var: var_name,
                    bound_type: concrete_ty,
                };
                let fd_result =
                    improve_functional_dependency(&fd_ctx, state, constraints, span.clone(), depth)
                        .await;
                if was_inserted {
                    state.fd_in_progress.remove(var_name);
                }
                fd_result?;
            }
        }
    }

    // Drain dispatch obligations keyed to this now-resolved TypeVar.
    let pending: Vec<crate::type_infer::DispatchObligation> = state.dispatch_obligations.iter()
        .filter(|o| {
            o.typevar_name == var_name
            && matches!(&o.varref_node.expr, crate::ast::SurfaceExpression::VarRef { call_dispatch, .. }
                if call_dispatch.get().is_none())
        })
        .cloned()
        .collect();

    for obligation in &pending {
        let resolved_vars: Vec<crate::type_def::Type> = obligation
            .constraint_vars
            .iter()
            .map(|arg| match arg {
                crate::type_class::ConstraintArg::Var(name) => state
                    .subst
                    .apply(&crate::type_def::Type::Var(name.clone(), state.level)),
                crate::type_class::ConstraintArg::Ground(ty) => ty.clone(),
            })
            .collect();

        let det_types: Vec<crate::type_def::Type> = obligation
            .det_positions
            .iter()
            .filter_map(|&i| resolved_vars.get(i).cloned())
            .collect();

        if det_types
            .iter()
            .any(|t| matches!(t, crate::type_def::Type::Var(..)))
        {
            continue;
        }

        let dispatch_tags: Vec<Option<String>> = det_types
            .iter()
            .map(|t| {
                let widened = crate::typecheck::typecheck_call::widen_literal_types(t.clone());
                crate::typecheck::type_to_dispatch_tag(&widened)
            })
            .collect();
        let type_args: Vec<&str> = dispatch_tags.iter().filter_map(|t| t.as_deref()).collect();
        let mangled = crate::type_def::instance_binding_name(
            &obligation.class_name,
            &obligation.method_name,
            &type_args,
        );

        if let Some(frames) = &state.scope_frames {
            if let Some((level, slot)) = crate::lower::resolve_name_in_frames(frames, &mangled) {
                if let crate::ast::SurfaceExpression::VarRef { call_dispatch, .. } =
                    &obligation.varref_node.expr
                {
                    call_dispatch.set(crate::lower::debruijn_to_var_addr(level, slot));
                }
            }
        }
    }

    state.dispatch_obligations.retain(|o| {
        matches!(&o.varref_node.expr, crate::ast::SurfaceExpression::VarRef { call_dispatch, .. }
            if call_dispatch.get().is_none())
    });

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

/// Bundled context for functional dependency improvement calls.
/// Groups the class-constraint data that is identical between the outer and inner function
/// to reduce the argument count and eliminate the `too_many_arguments` lint cause.
struct FdImprovementCtx<'a> {
    class: &'a str,
    args: &'a [ConstraintArg],
    fundeps: &'a [(Vec<usize>, Vec<usize>)],
    resolver_injective: bool,
    bound_var: &'a str,
    bound_type: &'a Type,
}

async fn improve_functional_dependency(
    ctx: &FdImprovementCtx<'_>,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
    depth: usize,
) -> Result<(), TypeDiagnostic> {
    // Depth guard: prevent infinite recursion through the FD improvement cycle.
    if state.fd_depth >= MAX_FD_DEPTH {
        // F7 FIX: Return error instead of silently succeeding when depth limit is reached
        return Err(TypeDiagnostic::error("type-error",
            format!(
                "functional dependency improvement depth limit exceeded (max {}) — possible recursive FD chain for class {}",
                MAX_FD_DEPTH, ctx.class
            ),
            span,
        ));
    }
    state.fd_depth += 1;
    let result = improve_functional_dependency_inner(ctx, state, constraints, span, depth).await;
    state.fd_depth -= 1;
    result
}

async fn improve_functional_dependency_inner(
    ctx: &FdImprovementCtx<'_>,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
    depth: usize,
) -> Result<(), TypeDiagnostic> {
    let class = ctx.class;
    let args = ctx.args;
    let fundeps = ctx.fundeps;
    let resolver_injective = ctx.resolver_injective;
    let bound_var = ctx.bound_var;
    let bound_type = ctx.bound_type;
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
                    ConstraintArg::Var(v) => state.apply(&Type::Var(v.clone(), 0)),
                    // Ground position — type already known from generalization.
                    ConstraintArg::Ground(t) => t.clone(),
                };
                ded_types.push((pos, ty));
            }

            // Only attempt reverse improvement when all determined positions are ground.
            let all_ded_ground = ded_types.iter().all(|(_, ty)| !ty.has_inference_vars());
            if all_ded_ground {
                // Scan InstanceEnv for an instance whose determined-position type
                // unifies with the ground determined types we have.
                let instance_env = state.get_working_instance_env();
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
                        let det_type_var = Type::Var(det_var.clone(), 0);
                        state.fd_in_progress.insert(det_var.clone());
                        let result = Box::pin(unify(
                            &det_type_var,
                            det_ty,
                            state,
                            constraints,
                            span.clone(),
                            depth + 1,
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
                ConstraintArg::Var(v) => state.apply(&Type::Var(v.clone(), 0)),
                // Ground position — type already known from generalization.
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
        // Two paths:
        // 1. Resolver classes: type-stage function normalization (falls through to MPTC on Unknown)
        // 2. General MPTC: InstanceEnv lookup (with literal widening on miss)

        // Extract class_decl with a scoped read lock so the guard drops before
        // any subsequent &mut state borrows (e.g. in lookup_mptc).
        let class_decl_for_fd = { state.env.read().unwrap().get_class(class) };
        let result_type = if let Some(class_decl) = class_decl_for_fd {
            // Check for resolver or fall back to general MPTC instance lookup
            // Try resolver first if available; fall through to MPTC on Unknown.
            let resolver_result = if let Some(ref resolver_name) = class_decl.resolver.clone() {
                // Resolver-based path: construct a TypeStageApp and normalize it.
                // Normalization looks up fn_name in the type-stage scope chain and invokes it.
                let det_arg_types: Vec<Type> = det_types.iter().map(|(_, ty)| ty.clone()).collect();
                let stage_app = Type::StageApp {
                    fn_name: resolver_name.clone(),
                    args: det_arg_types,
                };
                let mut norm_ctx = crate::type_normalize::NormCtxt::new(state.eval_ctx.clone());
                norm_ctx.type_stage_scope = state.type_stage_scope.clone();
                let resolved =
                    crate::type_normalize::normalize(&stage_app, &state.type_vars, &mut norm_ctx)
                        .await
                        .map_err(|e| {
                            TypeDiagnostic::error("type-error", e.to_string(), span.clone())
                        })?;

                // If normalization returned a stuck TypeStageApp, we can't improve yet.
                // Defer: the deferred_equalities mechanism will retry when more types are ground.
                if matches!(resolved, Type::StageApp { .. }) {
                    continue;
                }
                // If the resolver returned Unknown, fall through to MPTC instance lookup.
                // This allows resolver-based classes (e.g. Indexable with FieldType) to have
                // instances for types the resolver cannot handle (e.g. List, Bytes, Map)
                // while still using the resolver for structural record field lookup.
                if matches!(resolved, Type::Unknown) {
                    None
                } else {
                    Some(resolved)
                }
            } else {
                None
            };
            if let Some(resolved) = resolver_result {
                resolved
            } else {
                // No resolver result — fall back to general MPTC instance lookup via InstanceEnv.
                // Literal widening: IntLiteral → Int, StringLiteral → Str. These are Rust-internal
                // types; prelude cannot declare instances for them, but class satisfaction is
                // covariant: if T <: S and S satisfies C, then T satisfies C. We widen on miss.
                let det_arg_types: Vec<Type> = det_types.iter().map(|(_, ty)| ty.clone()).collect();

                // Build a temporary InstanceEnv snapshot to avoid borrow checker conflict.
                let instance_env = state.get_working_instance_env();
                let lookup_result =
                    Box::pin(instance_env.lookup_mptc(class, &det_arg_types, state)).await;

                // On miss, retry with widened literal types (IntLiteral→Int, StringLiteral→Str).
                let lookup_result = if lookup_result.is_none() {
                    let widened: Vec<Type> = det_arg_types
                        .iter()
                        .map(|ty| crate::typecheck::typecheck_call::widen_literal_types(ty.clone()))
                        .collect();
                    if widened != det_arg_types {
                        let instance_env2 = state.get_working_instance_env();
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
                                            TypeDiagnostic::error("type-error",
                                                format!(
                                                    "no instance for {} (determined field {} missing)",
                                                    class, pos
                                                ),
                                                span.clone(),
                                            )
                                        })?,
                                    None => {
                                        return Err(TypeDiagnostic::error("type-error",
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
                                return Err(TypeDiagnostic::error(
                                    "type-error",
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
                        // All determining positions are ground at this point.
                        // If lookup_mptc returns None, there's genuinely no matching
                        // instance — unless a determining position is `Unknown` (gradual),
                        // in which case we defer. Only error when all determining positions
                        // are definitively non-matching.
                        let should_defer = det_types
                            .iter()
                            .any(|(_, ty)| !is_definitely_no_instance_for(class, ty));
                        if should_defer {
                            continue;
                        }
                        return Err(TypeDiagnostic::error(
                            "type-error",
                            format!("no instance for {}", class),
                            span.clone(),
                        ));
                    }
                }
            }
        } else {
            // Class not found in class_env — should not happen
            return Err(TypeDiagnostic::error(
                "type-error",
                format!("unknown class {}", class),
                span.clone(),
            ));
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

            let ded_type_var = Type::Var(ded_var.clone(), 0);

            // Bindings go directly into state.type_vars — no separate substitution needed.
            state.fd_in_progress.insert(ded_var.clone());
            let result = Box::pin(unify(
                &ded_type_var,
                &result_type,
                state,
                constraints,
                span.clone(),
                depth + 1,
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
fn is_definitely_no_instance_for(_class: &str, ty: &Type) -> bool {
    // For all MPTC classes, treat any ground non-Unknown type as
    // definitely non-matching when lookup_mptc returns None.
    // Unknown gets special handling (defer).
    !matches!(ty, Type::Unknown)
}

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
) -> Result<Type, TypeDiagnostic> {
    // Check recursion depth to prevent infinite loops on cyclic types
    if depth > MAX_RESOLVE_HAS_FIELD_DEPTH {
        return Err(TypeDiagnostic::error(
            "type-error",
            "HasField recursion depth exceeded".to_string(),
            span,
        ));
    }

    // Resolve label to concrete string
    let label_str = match label {
        Label::Concrete(s) => s.clone(),
        Label::Var(var_name) => {
            // Look up the label var in substitution
            match state.lookup_binding(var_name) {
                Some(Type::StringLiteral(s)) => s,
                _ => {
                    return Err(TypeDiagnostic::error(
                        "type-error",
                        format!("label variable {} not bound to a string literal", var_name),
                        span,
                    ))
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
                        Err(TypeDiagnostic::error("type-error",
                            format!("record has no field '{}'", label_str),
                            span,
                        ))
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
        Type::Var(_, _) => Err(TypeDiagnostic::error("type-error",
            "cannot resolve HasField constraint on unbound type variable (expected caller to defer)".to_string(),
            span,
        )),

        // All other types don't support field access
        _ => Err(TypeDiagnostic::error("type-error",
            format!("type {} does not support field access", dict_type),
            span,
        )),
    }
}

const MAX_APPLY_DEPTH: usize = 256;

// Thread-local visited set for `apply_substitution`.
// Declared at module level so the static-initialization semantics are
// clear: the HashSet is allocated once per thread and reused across all calls.
thread_local! {
    static VISITED_TYPES: std::cell::RefCell<HashSet<String>> = std::cell::RefCell::new(HashSet::new());
}

/// Apply substitution to a type: resolve all bound TypeVars by looking up bindings
/// in the unified `type_vars` IndexMap.
pub fn apply_substitution(ty: &Type, type_vars: &IndexMap<String, TypeVarEntry>) -> Type {
    // Short-circuit: if the type has no inference variables, there is nothing to substitute.
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
        Type::Var(name, level) => {
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
                None => Cow::Owned(Type::Var(name.clone(), *level)),
            }
        }
        Type::Dict(row) => {
            let applied_row = apply_row_with_visited(row, type_vars, depth + 1, visited_types);
            Cow::Owned(Type::Dict(applied_row))
        }
        Type::Function {
            params,
            ret,
            typed_variadics,
            rest,
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
            typed_variadics: typed_variadics
                .iter()
                .map(|(name, ty)| {
                    (
                        name.clone(),
                        apply_type_with_visited(ty, type_vars, depth + 1, visited_types)
                            .into_owned(),
                    )
                })
                .collect(),
            rest: rest.as_ref().map(|boxed| {
                Box::new((
                    boxed.0.clone(),
                    apply_type_with_visited(&boxed.1, type_vars, depth + 1, visited_types)
                        .into_owned(),
                ))
            }),
            ret: Box::new(
                apply_type_with_visited(ret, type_vars, depth + 1, visited_types).into_owned(),
            ),
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
        Type::StageApp { fn_name, args } => Cow::Owned(Type::StageApp {
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
        Type::TyConResolved(_, _) => Cow::Borrowed(ty),
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

/// Directional record subtyping: constrain sub_row ≤ sup_row.
///
/// Step 1 — Width + depth (sup's fields must be coverable in sub, COVARIANT):
/// For each field (k, sup_ty) in sup_row.fields, sub must provide a compatible type:
///   - If sub_row.fields has k: constrain(sub_row.fields[k], sup_ty)
///   - Else if sub_row.tail is Uniform { value: sub_v, .. }: constrain(sub_v, sup_ty)
///   - Else: error (missing field)
///
/// Step 2 — Tail compatibility:
///   - (_, RowTail::Empty): Ok (sub may have more fields — width subtyping)
///   - (sub_tail, RowTail::Uniform { value: sup_v, .. }):
///     All sub's fields and uniform tail must be subtypes of sup_v.
///     Key types must also be compatible if sup has a key type.
async fn constrain_rows(
    sub_row: &Row,
    sup_row: &Row,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) -> Result<(), TypeDiagnostic> {
    // Step 1: Width + depth — sup's fields must be coverable in sub (COVARIANT).
    for (k, sup_ty) in &sup_row.fields {
        if let Some(sub_ty) = sub_row.fields.get(k) {
            // Sub has the field: constrain sub_ty ≤ sup_ty.
            Box::pin(constrain(sub_ty, sup_ty, state, constraints, span.clone())).await?;
        } else if let RowTail::Uniform { value: sub_v, .. } = &sub_row.tail {
            // Sub's uniform tail covers this field: constrain sub_v ≤ sup_ty.
            Box::pin(constrain(sub_v, sup_ty, state, constraints, span.clone())).await?;
        } else {
            // Sub doesn't have the field and has no uniform tail: error.
            return Err(TypeDiagnostic::error(
                "type-error",
                format!("missing field '{}': record subtype constraint", k),
                span,
            ));
        }
    }

    // Step 2: Tail compatibility.
    match (&sub_row.tail, &sup_row.tail) {
        // Sub may have more fields than sup (width subtyping).
        (_, RowTail::Empty) => Ok(()),

        // Sup is uniform: all sub's fields and tail must be subtypes of sup_v.
        (
            sub_tail,
            RowTail::Uniform {
                key: sup_key,
                value: sup_v,
            },
        ) => {
            // All sub's named fields must be subtypes of sup_v.
            for sub_field_ty in sub_row.fields.values() {
                Box::pin(constrain(
                    sub_field_ty,
                    sup_v,
                    state,
                    constraints,
                    span.clone(),
                ))
                .await?;
            }

            // If sub has a uniform tail, its value must also be a subtype of sup_v.
            if let RowTail::Uniform {
                key: sub_key,
                value: sub_v,
            } = sub_tail
            {
                Box::pin(constrain(sub_v, sup_v, state, constraints, span.clone())).await?;

                // If sup has a key type, sub's key must also be a subtype.
                if let Some(sup_k) = sup_key {
                    if let Some(sub_k) = sub_key {
                        Box::pin(constrain(sub_k, sup_k, state, constraints, span.clone())).await?;
                    } else {
                        return Err(TypeDiagnostic::error(
                            "type-error",
                            "key type mismatch: sup has key type but sub doesn't",
                            span,
                        ));
                    }
                }
            }
            // If sub_tail is Empty, that's fine — closed sub satisfies open sup.

            Ok(())
        }
    }
}

// S-861: equirecursive-checker
// substitute_recvar and substitute_recvar_row are pub(crate) in type_def.rs (canonical
// location, next to unfold_once). Imported here via `use super::*` (type_def re-exports
// everything from the parent module). No local copy needed.

/// Lower levels of all type/row variables in `ty` to min(their level, cap_level).
/// Performs occurs check simultaneously: returns `true` if `occurs_name` appears in the
/// tree, `false` otherwise. No allocation — directly updates `state.levels` in a single
/// recursive walk.
///
/// Uses `get_level_for_occurs_check` (returns 0 for unregistered names) rather than
/// failing with an error for unregistered names, because:
/// - μ-binder names in `Type::Recursive { var, body }` are string identifiers, not fresh
///   TypeVars from `fresh_type_var_with`, so they are never registered in `state.levels`.
///   Level 0 (outermost scope) is the correct default — it caps any TypeVars in the body
///   to the binder's scope.
/// - TypeVars created directly in tests without `fresh_type_var_with` also have no entry
///   in `state.levels`. Level 0 is safe: it treats them as outermost-scope, which is
///   conservative (never unsoundly generalizes).
fn lower_levels_check_occurs(
    ty: &Type,
    occurs_name: &str,
    cap_level: u32,
    state: &mut InferState,
) -> bool {
    match ty {
        Type::Var(name, _) => {
            let found = name == occurs_name;
            let current_level = state.get_level_for_occurs_check(name);
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
            typed_variadics,
            rest,
            required_count: _,
        } => {
            let mut found = false;
            for (_name, p_ty) in params {
                found |= lower_levels_check_occurs(p_ty, occurs_name, cap_level, state);
            }
            for (_, tv_ty) in typed_variadics {
                found |= lower_levels_check_occurs(tv_ty, occurs_name, cap_level, state);
            }
            if let Some(r) = rest {
                found |= lower_levels_check_occurs(&r.1, occurs_name, cap_level, state);
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
            let current_level = state.get_level_for_occurs_check(name);
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
        | Type::TyConResolved(_, _)
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
        Type::StageApp { fn_name: _, args } => {
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
/// Returns immediately when α has no class constraints — there is nothing to transfer.
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

// bind_single_type_var_from_compound removed: replaced by bas_cvar1_rewrite
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
    depth: usize,
) -> Result<(), TypeDiagnostic> {
    // Partition into TypeVars and concrete (non-TypeVar) members
    let type_vars: Vec<&Type> = compound_members
        .iter()
        .filter(|m| matches!(m, Type::Var(_, _)))
        .collect();
    let concrete_members: Vec<&Type> = compound_members
        .iter()
        .filter(|m| !matches!(m, Type::Var(_, _)))
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
        return Err(TypeDiagnostic::error(
            "type-error",
            format!(
                "cannot unify {} with {}",
                concrete,
                Type::Union(compound_members.to_vec())
            ),
            span,
        ));
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
        let Type::Var(var_name, _) = tv else {
            continue;
        };

        let alpha_level = state.get_level_for_occurs_check(var_name);
        if lower_levels_check_occurs(&bound_type, var_name, alpha_level, state) {
            return Err(TypeDiagnostic::error(
                "type-error",
                format!("infinite type: {var_name} occurs in {bound_type}"),
                span,
            ));
        }

        if type_vars.len() == 1 {
            // Single TypeVar: bind directly (equational constraint)
            let promoted = promote_literal_for_constrained_var(
                var_name,
                bound_type.clone(),
                constraints,
                state,
            );
            check_constraints_on_var(var_name, &promoted, state, constraints, span.clone(), depth)
                .await?;
            state.bind_type_var(var_name.clone(), promoted);
            return Ok(());
        } else {
            // Multiple TypeVars: add as lower bound (inequality constraint)
            state
                .bounds
                .entry(var_name.clone())
                .or_default()
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
    depth: usize,
) -> Result<(), TypeDiagnostic> {
    // Partition into TypeVars and concrete (non-TypeVar) members
    let type_vars: Vec<&Type> = compound_members
        .iter()
        .filter(|m| matches!(m, Type::Var(_, _)))
        .collect();
    let concrete_members: Vec<&Type> = compound_members
        .iter()
        .filter(|m| !matches!(m, Type::Var(_, _)))
        .collect();

    if type_vars.is_empty() {
        if Type::is_subtype(
            &Type::Intersection(compound_members.to_vec()),
            concrete,
            Some(&state.tycon_env),
        ) {
            return Ok(());
        }
        return Err(TypeDiagnostic::error(
            "type-error",
            format!(
                "cannot unify {} with {}",
                concrete,
                Type::Intersection(compound_members.to_vec())
            ),
            span,
        ));
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
        let Type::Var(var_name, _) = tv else {
            continue;
        };

        let alpha_level = state.get_level_for_occurs_check(var_name);
        if lower_levels_check_occurs(&bound_type, var_name, alpha_level, state) {
            return Err(TypeDiagnostic::error(
                "type-error",
                format!("infinite type: {var_name} occurs in {bound_type}"),
                span,
            ));
        }

        if type_vars.len() == 1 {
            // Single TypeVar: bind directly
            let promoted = promote_literal_for_constrained_var(
                var_name,
                bound_type.clone(),
                constraints,
                state,
            );
            check_constraints_on_var(var_name, &promoted, state, constraints, span.clone(), depth)
                .await?;
            state.bind_type_var(var_name.clone(), promoted);
            return Ok(());
        } else {
            // Multiple TypeVars: add as upper bound
            state
                .bounds
                .entry(var_name.clone())
                .or_default()
                .add_upper(bound_type.clone());
        }
    }

    Ok(())
}

/// Validate that two function types have compatible arity and variadic structure.
///
/// Returns `Ok(())` when both sides have the same fixed-param count, the same
/// variadic/non-variadic status, and the same number of typed-variadic buckets.
/// Returns a structured `Err` for each mismatch kind.
///
/// Called by both `unify()` and `constrain()` so the error messages are consistent
/// regardless of which judgment is being applied.
fn check_function_arity(
    p1_len: usize,
    p2_len: usize,
    is_variadic_1: bool,
    is_variadic_2: bool,
    tv1_len: usize,
    tv2_len: usize,
    has_rest_1: bool,
    has_rest_2: bool,
    span: Span,
) -> Result<(), TypeDiagnostic> {
    if p1_len != p2_len {
        return Err(TypeDiagnostic::error(
            "type-error",
            format!(
                "arity mismatch: expected {} arguments, got {}",
                p1_len, p2_len
            ),
            span,
        ));
    }
    if is_variadic_1 != is_variadic_2 {
        return Err(TypeDiagnostic::error(
            "type-error",
            format!(
                "variadic mismatch: {} vs {}",
                if is_variadic_1 {
                    "variadic"
                } else {
                    "non-variadic"
                },
                if is_variadic_2 {
                    "variadic"
                } else {
                    "non-variadic"
                }
            ),
            span,
        ));
    }
    if tv1_len != tv2_len {
        return Err(TypeDiagnostic::error(
            "type-error",
            format!(
                "variadic bucket count mismatch: {} typed bucket(s) vs {} typed bucket(s)",
                tv1_len, tv2_len
            ),
            span,
        ));
    }
    if has_rest_1 != has_rest_2 {
        return Err(TypeDiagnostic::error(
            "type-error",
            "variadic rest mismatch: one function has an untyped rest parameter, the other does not".to_string(),
            span,
        ));
    }
    Ok(())
}

pub async fn unify(
    a: &Type,
    b: &Type,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
    depth: usize,
) -> Result<(), TypeDiagnostic> {
    if depth >= MAX_UNIFY_DEPTH {
        return Err(TypeDiagnostic::error(
            "type-error",
            format!("unification depth limit exceeded (limit: {MAX_UNIFY_DEPTH})"),
            span,
        ));
    }
    // Apply current substitution to both sides (Robinson step: chase bound vars).
    let a_substituted = state.apply(a);
    let b_substituted = state.apply(b);

    // Normalize both types (for TypeStageApp reduction).
    // allow_eval is false: resolver evaluation is disabled to prevent runtime errors from
    // propagating into type inference. With allow_eval=false, normalize() cannot produce Err.
    let mut norm_ctx = crate::type_normalize::NormCtxt::new(state.eval_ctx.clone());
    norm_ctx.type_stage_scope = state.type_stage_scope.clone();
    norm_ctx.allow_eval = false;
    let a = crate::type_normalize::normalize(&a_substituted, &state.type_vars, &mut norm_ctx)
        .await
        .map_err(|e| TypeDiagnostic::error("type-error", e.to_string(), span.clone()))?;
    let b = crate::type_normalize::normalize(&b_substituted, &state.type_vars, &mut norm_ctx)
        .await
        .map_err(|e| TypeDiagnostic::error("type-error", e.to_string(), span.clone()))?;
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

        // Unknown-consistency: gradual typing treatment (Siek & Taha 2006, §3).
        //
        // Unknown is the dynamic type `?` in gradual typing — it is CONSISTENT with all types,
        // not equal to them. Consistency is not unification: unify(?, T) succeeds for all T
        // without binding any type variable. No occurs check is needed because Unknown never
        // creates a new TypeVar binding — it is a terminal type that simply propagates.
        //
        // When Unknown meets a TypeVar, we zero the variable's level to prevent
        // over-generalization: if α were later unified with ?, it should not escape its scope.
        // When Unknown meets a non-TypeVar, we zero all vars in the non-Unknown side for the
        // same reason — any TypeVar "touched" by Unknown must not be generalized.
        //
        // This behavior is SOUND and CORRECT for gradual typing. Do not change it to fail
        // or emit a diagnostic — that would break the gradual type system.
        (Type::Unknown, Type::Var(name, _)) => {
            state.set_level(name.clone(), 0);
            Ok(())
        }
        (Type::Var(name, _), Type::Unknown) => {
            state.set_level(name.clone(), 0);
            Ok(())
        }
        (Type::Unknown, other) | (other, Type::Unknown) => {
            // Zero levels of all type/row vars in the non-Unknown side to prevent
            // over-generalization. E.g., unify(Unknown, Fn(TypeVar("b",3) -> Int))
            // must zero b's level so b will not be generalized at its binding site.
            let mut type_vars = HashSet::new();
            other.collect_all_vars(&mut type_vars);
            for var in &type_vars {
                state.set_level(var.clone(), 0);
            }
            // Warn when Unknown meets a concrete type (not TypeVar/Unknown/Error/Any).
            // Unknown is consistent with all types but the consistency is a potential
            // runtime type error — missing annotation causes undefined behavior.
            if !matches!(
                other,
                Type::Unknown | Type::Var(..) | Type::Error(_) | Type::Any
            ) {
                state.diagnostics.push(TypeDiagnostic::warn(
                    "unknown-type",
                    "type unknown",
                    span.clone(),
                ));
            }
            Ok(())
        }

        // Top (Any) unification: Any is the top type, consistent with all types.
        // Treat symmetrically with Unknown: zero levels to prevent over-generalization.
        // Any should not appear in unification positions in well-typed programs; when it does
        // (e.g., from explicit @Any annotations), this is the correct conservative treatment.
        (Type::Any, Type::Var(name, _)) => {
            state.set_level(name.clone(), 0);
            Ok(())
        }
        (Type::Var(name, _), Type::Any) => {
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
        (Type::Var(name_a, _), Type::Var(name_b, _)) => {
            let level_a = state.get_level_for_occurs_check(name_a);
            let level_b = state.get_level_for_occurs_check(name_b);

            // Bind the higher-level variable to the lower-level one.
            // If levels are equal, bind left-to-right for determinism.
            // Bind the higher-level variable to the lower-level one for determinism.
            if level_a >= level_b {
                // Bind name_a → TypeVar(name_b)
                transfer_class_constraints(name_a, name_b, constraints);
                state.bind_type_var(name_a.clone(), Type::Var(name_b.clone(), level_b));
            } else {
                // Bind name_b → TypeVar(name_a)
                transfer_class_constraints(name_b, name_a, constraints);
                state.bind_type_var(name_b.clone(), Type::Var(name_a.clone(), level_a));
            }
            Ok(())
        }

        // U-VAR-LEVEL: bind α to τ, lower levels of all β ∈ FTV(τ) and all ρ ∈ FRV(τ)
        (Type::Var(name, _), _) => {
            // Fused occurs check + level lowering: one tree walk, zero HashSet allocations.
            // lower_levels_check_occurs returns true if `name` appears in the type tree
            // (infinite-type guard), and simultaneously lowers all var levels to cap_level.
            let alpha_level = state.get_level_for_occurs_check(name);
            if lower_levels_check_occurs(&b, name, alpha_level, state) {
                return Err(TypeDiagnostic::error(
                    "type-error",
                    format!("infinite type: {name} occurs in {b}"),
                    span,
                ));
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
            if let Type::Var(beta_name, _) = &b {
                transfer_class_constraints(name, beta_name, constraints);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                state.bind_type_var(name.clone(), b);
            } else if let Type::Operator(beta_name) = &b {
                transfer_class_constraints(name, beta_name, constraints);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                state.bind_type_var(name.clone(), b);
            } else {
                // Binding α to a concrete type — check constraints normally
                check_constraints_on_var(name, &b, state, constraints, span.clone(), depth).await?;
                state.bind_type_var(name.clone(), b);
            }
            Ok(())
        }
        // U-VAR-LEVEL-SYM: bind α to τ, lower levels of all β ∈ FTV(τ) and all ρ ∈ FRV(τ)
        (_, Type::Var(name, _)) => {
            // Fused occurs check + level lowering: one tree walk, zero HashSet allocations.
            let alpha_level = state.get_level_for_occurs_check(name);
            if lower_levels_check_occurs(&a, name, alpha_level, state) {
                return Err(TypeDiagnostic::error(
                    "type-error",
                    format!("infinite type: {name} occurs in {a}"),
                    span,
                ));
            }
            // Promote literal types when binding a constrained type variable.
            let a = promote_literal_for_constrained_var(name, a, constraints, state);

            // CONSTRAINT TRANSFER: when binding α to β (both TypeVars or Operator), transfer Class
            // constraints from α to β instead of checking. β inherits α's obligations and will be
            // checked when β is bound to a concrete type. HasField constraints are NOT transferred
            // (they reference the dict variable, not the param).
            // bind_at_level routes the binding to the frame matching the TypeVar's creation level.
            if let Type::Var(beta_name, _) = &a {
                transfer_class_constraints(name, beta_name, constraints);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                state.bind_type_var(name.clone(), a);
            } else if let Type::Operator(beta_name) = &a {
                transfer_class_constraints(name, beta_name, constraints);
                // After transferring constraints, bind α to β directly — no check_constraints_on_var
                state.bind_type_var(name.clone(), a);
            } else {
                // Binding α to a concrete type — check constraints normally
                check_constraints_on_var(name, &a, state, constraints, span.clone(), depth).await?;
                state.bind_type_var(name.clone(), a);
            }
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
            Box::pin(constrain(
                &opened_a,
                &opened_b,
                state,
                constraints,
                span.clone(),
            ))
            .await?;
            Box::pin(constrain(&opened_b, &opened_a, state, constraints, span)).await
        }

        // Arm 4 (open-left): left is Recursive, right is a concrete type (not TypeVar — that
        // was caught by the U-VAR-LEVEL-SYM arm above; not Recursive — caught by Arm 3 above).
        // Open the left side with a fresh TypeVar and bidirectionally constrain with the right.
        (Type::Recursive { var: va, body: ba }, _) => {
            let fresh = state.fresh_type_var(&span);
            let opened_a = substitute_recvar(ba, va, &fresh);
            Box::pin(constrain(&opened_a, &b, state, constraints, span.clone())).await?;
            Box::pin(constrain(&b, &opened_a, state, constraints, span)).await
        }

        // Arm 5 (open-right): right is Recursive, left is a concrete type (not TypeVar — caught
        // above; not Recursive — caught by Arm 3 above).
        // Open the right side with a fresh TypeVar and bidirectionally constrain with the left.
        (_, Type::Recursive { var: vb, body: bb }) => {
            let fresh = state.fresh_type_var(&span);
            let opened_b = substitute_recvar(bb, vb, &fresh);
            Box::pin(constrain(&a, &opened_b, state, constraints, span.clone())).await?;
            Box::pin(constrain(&opened_b, &a, state, constraints, span)).await
        }

        (Type::Function { .. }, Type::Function { .. }) => {
            // unify(Fn, Fn): delegate unconditionally to bidirectional constrain.
            // C-FN in constrain() handles all cases:
            //   - zero-param variadic (any-function semantics)
            //   - arity and variadic structure validation via check_function_arity
            //   - contravariant params, covariant return
            // There is one correct path — no pre-checks or special cases here.
            let (ac, bc) = (a.clone(), b.clone());
            Box::pin(constrain(&ac, &bc, state, constraints, span.clone())).await?;
            Box::pin(constrain(&bc, &ac, state, constraints, span)).await
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

        // Negation unification: bidirectional constrain (contravariant).
        (Type::Negation(t1), Type::Negation(t2)) => {
            let (t1c, t2c) = ((**t1).clone(), (**t2).clone());
            // Negation is contravariant, but for unification we need bidirectional constraint.
            // constrain(~T1, ~T2) checks T2 ≤ T1; constrain(~T2, ~T1) checks T1 ≤ T2.
            Box::pin(constrain(&t2c, &t1c, state, constraints, span.clone())).await?;
            Box::pin(constrain(&t1c, &t2c, state, constraints, span)).await
        }

        // Negation disjointness: if T <: A, then T & ~A = Never (provably empty intersection).
        // We can statically reject this case without full RDNF normalization — if is_subtype(T, A)
        // holds, the intersection is provably Never. For all other cases (uncertain overlap), we
        // remain conservative and allow unification to succeed. Runtime value_matches_type handles
        // the residual constraint for `[@[[without T]] expr]` TypeAsserts.
        (concrete, Type::Negation(inner)) if !matches!(concrete, Type::Var(..) | Type::Unknown) => {
            if Type::is_subtype(concrete, inner, Some(&state.tycon_env)) {
                Err(TypeDiagnostic::error("type-error",
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
        (Type::Negation(inner), concrete) if !matches!(concrete, Type::Var(..) | Type::Unknown) => {
            if Type::is_subtype(concrete, inner, Some(&state.tycon_env)) {
                Err(TypeDiagnostic::error("type-error",
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
                return Err(TypeDiagnostic::error(
                    "type-error",
                    format!("cannot unify {} with {}", a.clone(), b.clone()),
                    span,
                ));
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
                            format!(" (defined at {}:{}:{})", s.file, s.start_line, s.start_col)
                        })
                        .unwrap_or_default();
                    let loc2 = d2
                        .definition_span
                        .as_ref()
                        .map(|s| {
                            format!(" (defined at {}:{}:{})", s.file, s.start_line, s.start_col)
                        })
                        .unwrap_or_default();
                    Err(TypeDiagnostic::error(
                        "type-error",
                        format!(
                            "type constructor '{n1}' refers to two distinct definitions: \
                             {n1}{loc1} vs {n1}{loc2}"
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

        // UNIFY-TYCON-RESOLVED: TyConResolved unified with TyConResolved.
        // Uses Arc::ptr_eq for identity check — same Arc = same type definition.
        (Type::TyConResolved(n1, arc1), Type::TyConResolved(n2, arc2)) => {
            if Arc::ptr_eq(arc1, arc2) {
                Ok(())
            } else {
                // Different Arcs — cross-scope shadowing detected.
                let loc1 = arc1
                    .definition_span
                    .as_ref()
                    .map(|s| format!(" (defined at {}:{}:{})", s.file, s.start_line, s.start_col))
                    .unwrap_or_default();
                let loc2 = arc2
                    .definition_span
                    .as_ref()
                    .map(|s| format!(" (defined at {}:{}:{})", s.file, s.start_line, s.start_col))
                    .unwrap_or_default();
                Err(TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "type constructor '{n1}' refers to two distinct definitions: \
                         {n1}{loc1} vs {n2}{loc2}"
                    ),
                    span,
                ))
            }
        }

        // UNIFY-TYCON-INTEROP: TyConResolved unified with TyCon (string-based).
        // Check name equality for interop with unresolved/builtin TyCon uses.
        (Type::TyConResolved(name, _arc), Type::TyCon(n))
        | (Type::TyCon(n), Type::TyConResolved(name, _arc)) => {
            if name == n {
                Ok(())
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    format!("cannot unify {} with {}", a.clone(), b.clone()),
                    span,
                ))
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
                    Err(TypeDiagnostic::error("type-error",
                        format!(
                            "cannot unify nominal variant with type '{}': variant is not a member of '{}'",
                            n, n
                        ),
                        span,
                    ))
                }
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    format!("cannot unify {} with {}", a.clone(), b.clone()),
                    span,
                ))
            }
        }
        (Type::NominalVariant { .. }, Type::TyCon(n)) => {
            if let Some(def) = state.tycon_env.get(n.as_str()) {
                let body = def.body.clone();
                let tycon_env = Some(&state.tycon_env);
                if Type::is_subtype(&a, &body, tycon_env) {
                    Ok(())
                } else {
                    Err(TypeDiagnostic::error("type-error",
                        format!(
                            "cannot unify nominal variant with type '{}': variant is not a member of '{}'",
                            n, n
                        ),
                        span,
                    ))
                }
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    format!("cannot unify {} with {}", a.clone(), b.clone()),
                    span,
                ))
            }
        }

        // UNIFY-TYCON-UNION: TyCon unified with a Union of NominalVariants.
        //
        // Succeeds when every union member is a subtype of the TyCon's declared body.
        // This is the subset direction — the union must be covered entirely by the
        // TyCon's constructor family.
        //
        // Symmetric: both (TyCon, Union) and (Union, TyCon) are handled here.
        // Guard: only fire when the union has no TypeVars. If it has TypeVars, fall through
        // to C-Var1 which handles constraint rewriting for inference variables.
        (Type::TyCon(n), Type::Union(members))
            if !members.iter().any(|m| matches!(m, Type::Var(_, _))) =>
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
                    Err(TypeDiagnostic::error(
                        "type-error",
                        format!("cannot unify {} with {}", a, b),
                        span,
                    ))
                }
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    format!("cannot unify {} with {}", a, b),
                    span,
                ))
            }
        }
        (Type::Union(members), Type::TyCon(n))
            if !members.iter().any(|m| matches!(m, Type::Var(_, _))) =>
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
                    Err(TypeDiagnostic::error(
                        "type-error",
                        format!("cannot unify {} with {}", a, b),
                        span,
                    ))
                }
            } else {
                Err(TypeDiagnostic::error(
                    "type-error",
                    format!("cannot unify {} with {}", a, b),
                    span,
                ))
            }
        }

        // UNIFY-OPERATOR-TO-OPERATOR: bind higher-level Operator to lower-level Operator.
        // Follows Kiselyov L3 invariant (same as TypeVar-to-TypeVar at lines 1837-1860).
        (Type::Operator(m), Type::Operator(n)) if m != n => {
            let level_m = state.get_level_for_occurs_check(m);
            let level_n = state.get_level_for_occurs_check(n);

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
            Ok(())
        }

        // UNIFY-OPERATOR: bind type constructor variable m to a type T.
        // Occurs check prevents infinite kinds (m ∉ ftv(T)).
        // Kind check premise is deferred to hkt-kind-inference.
        (Type::Operator(m), _) => {
            // Fused occurs check + level lowering (Kiselyov L3 invariant for Operator variables)
            let alpha_level = state.get_level_for_occurs_check(m);
            if lower_levels_check_occurs(&b, m, alpha_level, state) {
                return Err(TypeDiagnostic::error(
                    "type-error",
                    format!("infinite type: operator variable {} occurs in {}", m, b),
                    span,
                ));
            }
            // CONSTRAINT TRANSFER: when binding m to TypeVar, transfer constraints
            // instead of checking. When binding to a concrete type, check constraints normally.
            // bind_at_level routes to the frame matching the Operator variable's creation level.
            if let Type::Var(beta_name, _) = &b {
                transfer_class_constraints(m, beta_name, constraints);
                state.bind_type_var(m.clone(), b.clone());
            } else {
                // Binding to concrete type — check constraints
                check_constraints_on_var(m, &b, state, constraints, span.clone(), depth).await?;
                state.bind_type_var(m.clone(), b.clone());
            }
            Ok(())
        }
        // UNIFY-OPERATOR-SYM: symmetric case
        (_, Type::Operator(m)) => {
            // Fused occurs check + level lowering (Kiselyov L3 invariant for Operator variables)
            let alpha_level = state.get_level_for_occurs_check(m);
            if lower_levels_check_occurs(&a, m, alpha_level, state) {
                return Err(TypeDiagnostic::error(
                    "type-error",
                    format!("infinite type: operator variable {} occurs in {}", m, a),
                    span,
                ));
            }
            // CONSTRAINT TRANSFER: when binding m to TypeVar, transfer constraints
            // instead of checking. When binding to a concrete type, check constraints normally.
            // bind_at_level routes to the frame matching the Operator variable's creation level.
            if let Type::Var(beta_name, _) = &a {
                transfer_class_constraints(m, beta_name, constraints);
                state.bind_type_var(m.clone(), a.clone());
            } else {
                // Binding to concrete type — check constraints
                check_constraints_on_var(m, &a, state, constraints, span.clone(), depth).await?;
                state.bind_type_var(m.clone(), a.clone());
            }
            Ok(())
        }

        // UNIFY-APP: delegate unconditionally to bidirectional constrain.
        //
        // Variance-directed dispatch belongs entirely in constrain()'s C-App arm, which
        // looks up TyConDef.variance[i] and routes each argument position to
        // Covariant/Contravariant/Invariant/Phantom constraint accordingly.
        //
        // Type constructor application. For matching TyCon spines, all arg positions
        // use bidirectional constrain — variance is directional only in constrain()'s
        // C-App arm; in unify() equality is symmetric so all positions are bidirectional.
        // For non-TyCon App or mismatched constructors, recurse into unify() on the
        // components (NOT constrain) to avoid constrain→unify→constrain infinite loops
        // on App types whose head is a TypeVar or has no registered TyConDef.
        (Type::App(f1, a1), Type::App(f2, a2)) => {
            if let (Some((name1, args1)), Some((name2, args2))) =
                (extract_tycon_spine(&a), extract_tycon_spine(&b))
            {
                if name1 == name2 && args1.len() == args2.len() {
                    for (arg_a, arg_b) in args1.iter().zip(args2.iter()) {
                        let (ac, bc) = ((*arg_a).clone(), (*arg_b).clone());
                        Box::pin(constrain(&ac, &bc, state, constraints, span.clone())).await?;
                        Box::pin(constrain(&bc, &ac, state, constraints, span.clone())).await?;
                    }
                    return Ok(());
                }
            }
            // Non-TyCon App or mismatched constructors: recurse via unify() on components.
            // Using constrain() here would create constrain→unify→constrain loops.
            Box::pin(unify(f1, f2, state, constraints, span.clone(), depth + 1)).await?;
            Box::pin(unify(a1, a2, state, constraints, span, depth + 1)).await
        }

        // Record unification: bidirectional constrain_rows for field subtyping,
        // plus UNIFY-UNIFORM tail unification for TypeVar binding (Rémy 1994 §3).
        // Field subtyping is a constrain() concern; tail TypeVar binding is a
        // unify() concern — equality requires direct substitution, not bounds.
        (Type::Dict(row1), Type::Dict(row2)) => {
            let (r1c, r2c) = (row1.clone(), row2.clone());
            constrain_rows(&r1c, &r2c, state, constraints, span.clone()).await?;
            constrain_rows(&r2c, &r1c, state, constraints, span.clone()).await?;

            // UNIFY-UNIFORM: unify tail types and bind Uniform-tail TypeVars.
            use crate::type_def::RowTail;
            match (&row1.tail, &row2.tail) {
                (RowTail::Empty, RowTail::Empty) => Ok(()),

                (
                    RowTail::Uniform { key: k1, value: v1 },
                    RowTail::Uniform { key: k2, value: v2 },
                ) => {
                    Box::pin(unify(v1, v2, state, constraints, span.clone(), depth + 1)).await?;
                    if let (Some(k1_ty), Some(k2_ty)) = (k1, k2) {
                        Box::pin(unify(
                            k1_ty,
                            k2_ty,
                            state,
                            constraints,
                            span.clone(),
                            depth + 1,
                        ))
                        .await?;
                    }
                    // UNIFY-UNIFORM named-field validation: runs after constrain_rows to handle
                    // the tail-specific TypeVar binding cases (Rémy 1994 §3). This step is NOT
                    // redundant with constrain_rows:
                    //   • constrain_rows enforces field-to-field directional subtyping (Int ≤ Str),
                    //     adding bounds to TypeVars it encounters in field positions.
                    //   • This block enforces the Uniform value constraint: every named field from
                    //     BOTH rows must conform to the unified Uniform value type V. That invariant
                    //     is separate from field-to-field subtyping and cannot be expressed as a
                    //     constrain_rows pair (constrain_rows has no access to the Uniform join).
                    //   • When V is a TypeVar (alpha), we bind alpha to the join of all field types —
                    //     a direct substitution that constrain_rows (which only adds bounds) cannot do.
                    //   • When V is a concrete type, the is_subtype check validates fields that
                    //     constrain_rows accepted as bounded: a field that passed covariant constraint
                    //     is re-confirmed against the concrete Uniform type via structural equality.
                    //     For ground-type fields this fires only if constrain_rows would have already
                    //     caught a mismatch, so in practice this path catches no new errors for
                    //     well-formed programs — but is retained as a defense-in-depth invariant check.
                    let v_fixed = state.apply(v1);
                    let all_fields: Vec<Type> = row1
                        .fields
                        .values()
                        .chain(row2.fields.values())
                        .cloned()
                        .collect();
                    if !all_fields.is_empty() {
                        if let Type::Var(alpha, _) = &v_fixed {
                            let join = Type::normalize_union(all_fields);
                            Box::pin(unify(
                                &Type::Var(alpha.clone(), 0),
                                &join,
                                state,
                                constraints,
                                span.clone(),
                                depth + 1,
                            ))
                            .await?;
                        } else if !v_fixed.has_inference_vars() {
                            for field_ty in &all_fields {
                                let field_fixed = state.apply(field_ty);
                                if !Type::is_subtype(&field_fixed, &v_fixed, None) {
                                    return Err(TypeDiagnostic::error(
                                        "type-error",
                                        format!(
                                            "field type {} does not conform to Uniform constraint {}",
                                            field_fixed, v_fixed
                                        ),
                                        span.clone(),
                                    ));
                                }
                            }
                        }
                    }
                    Ok(())
                }

                (RowTail::Empty, RowTail::Uniform { value, .. })
                | (RowTail::Uniform { value, .. }, RowTail::Empty) => {
                    let v_fixed = state.apply(value);
                    let field_types: Vec<Type> = row1
                        .fields
                        .values()
                        .chain(row2.fields.values())
                        .cloned()
                        .collect();
                    if field_types.is_empty() {
                        return Ok(());
                    }
                    if let Type::Var(alpha, _) = &v_fixed {
                        let join = Type::normalize_union(field_types);
                        Box::pin(unify(
                            &Type::Var(alpha.clone(), 0),
                            &join,
                            state,
                            constraints,
                            span.clone(),
                            depth + 1,
                        ))
                        .await?;
                    } else if !v_fixed.has_inference_vars() {
                        for field_ty in &field_types {
                            let field_fixed = state.apply(field_ty);
                            if !Type::is_subtype(&field_fixed, &v_fixed, Some(&state.tycon_env)) {
                                return Err(TypeDiagnostic::error(
                                    "type-error",
                                    format!(
                                        "field type {} does not conform to Uniform constraint {}",
                                        field_fixed, v_fixed
                                    ),
                                    span.clone(),
                                ));
                            }
                        }
                    }
                    Ok(())
                }
            }
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
                return Err(TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "cannot unify nominal variants with different tags: {}.{} and {}.{}",
                        tycon1, ctor1, tycon2, ctor2
                    ),
                    span,
                ));
            }
            let (f1c, f2c) = (fields1.clone(), fields2.clone());
            constrain_rows(&f1c, &f2c, state, constraints, span.clone()).await?;
            constrain_rows(&f2c, &f1c, state, constraints, span).await
        }

        (Type::NominalVariant { tycon, ctor, .. }, Type::Dict(_)) => Err(TypeDiagnostic::error(
            "type-error",
            format!(
                "cannot unify nominal variant {}.{} with structural record",
                tycon, ctor
            ),
            span,
        )),
        (Type::Dict(_), Type::NominalVariant { tycon, ctor, .. }) => Err(TypeDiagnostic::error(
            "type-error",
            format!(
                "cannot unify structural record with nominal variant {}.{}",
                tycon, ctor
            ),
            span,
        )),

        // Record ↔ Intersection-of-Records unification.
        //
        // When a concrete Record is unified against an Intersection whose members are all
        // Records (the shape produced by multi-field `@[x: Int  y: String]` annotations),
        // bidirectionally constrain the record against EACH member.
        //
        // This arm is placed BEFORE the C-Var2 patterns because those patterns require
        // `!concrete.has_inference_vars()`, which would not fire for intersections whose
        // members contain RowVar tails (which are inference variables).  Additionally,
        // C-Var2 only handles intersections with exactly one TypeVar — not the all-Record
        // intersection shape we are handling here.
        (Type::Dict(_), Type::Intersection(members))
            if members.iter().all(|m| matches!(m, Type::Dict(_))) =>
        {
            let (ac, members) = (a.clone(), members.clone());
            for member in &members {
                let mc = member.clone();
                Box::pin(constrain(&ac, &mc, state, constraints, span.clone())).await?;
                Box::pin(constrain(&mc, &ac, state, constraints, span.clone())).await?;
            }
            Ok(())
        }
        (Type::Intersection(members), Type::Dict(_))
            if members.iter().all(|m| matches!(m, Type::Dict(_))) =>
        {
            let (bc, members) = (b.clone(), members.clone());
            for member in &members {
                let mc = member.clone();
                Box::pin(constrain(&mc, &bc, state, constraints, span.clone())).await?;
                Box::pin(constrain(&bc, &mc, state, constraints, span.clone())).await?;
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
                && members.iter().any(|m| matches!(m, Type::Var(_, _))) =>
        {
            let members = members.clone();
            bas_cvar1_rewrite(&members, concrete, state, constraints, span, depth).await
        }

        // Symmetric C-Var1: Union on the left, concrete on the right
        (Type::Union(members), concrete)
            if !concrete.has_inference_vars()
                && members.iter().any(|m| matches!(m, Type::Var(_, _))) =>
        {
            let members = members.clone();
            bas_cvar1_rewrite(&members, concrete, state, constraints, span, depth).await
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
                && members.iter().any(|m| matches!(m, Type::Var(_, _))) =>
        {
            let members = members.clone();
            bas_cvar2_rewrite(&members, concrete, state, constraints, span, depth).await
        }

        // Symmetric C-Var2: concrete on the left, Intersection on the right
        (concrete, Type::Intersection(members))
            if !concrete.has_inference_vars()
                && members.iter().any(|m| matches!(m, Type::Var(_, _))) =>
        {
            let members = members.clone();
            bas_cvar2_rewrite(&members, concrete, state, constraints, span, depth).await
        }

        // TypeStageApp unification cases (after normalization in chr-normalization sprint).
        // Case 1: same function name -> pairwise bidirectional constrain args
        (
            Type::StageApp {
                fn_name: f1,
                args: a1,
            },
            Type::StageApp {
                fn_name: f2,
                args: a2,
            },
        ) if f1 == f2 => {
            if a1.len() != a2.len() {
                return Err(TypeDiagnostic::error(
                    "type-error",
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
                let (a1c, a2c) = (arg1.clone(), arg2.clone());
                Box::pin(constrain(&a1c, &a2c, state, constraints, span.clone())).await?;
                Box::pin(constrain(&a2c, &a1c, state, constraints, span.clone())).await?;
            }
            Ok(())
        }
        // Case 2: different function names -> error
        (Type::StageApp { fn_name: f1, .. }, Type::StageApp { fn_name: f2, .. }) => {
            Err(TypeDiagnostic::error(
                "type-error",
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
        (Type::StageApp { .. }, concrete) | (concrete, Type::StageApp { .. })
            if !matches!(concrete, Type::Var(..) | Type::Unknown | Type::Any) =>
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
                Err(TypeDiagnostic::error(
                    "type-error",
                    format!("cannot unify {} with {}", b, a),
                    span,
                ))
            }
        }

        _ => Err(TypeDiagnostic::error(
            "type-error",
            format!("cannot unify {} with {}", a, b),
            span,
        )),
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
pub async fn constrain(
    sub: &Type,
    sup: &Type,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) -> Result<(), TypeDiagnostic> {
    // Apply current substitution to both sides.
    let sub_substituted = state.apply(sub);
    let sup_substituted = state.apply(sup);

    // Normalize both types.
    // allow_eval is false: resolver evaluation is disabled inside constrain().
    let mut norm_ctx = crate::type_normalize::NormCtxt::new(state.eval_ctx.clone());
    norm_ctx.type_stage_scope = state.type_stage_scope.clone();
    norm_ctx.allow_eval = false;
    let sub = crate::type_normalize::normalize(&sub_substituted, &state.type_vars, &mut norm_ctx)
        .await
        .map_err(|e| TypeDiagnostic::error("type-error", e.to_string(), span.clone()))?;
    let sup = crate::type_normalize::normalize(&sup_substituted, &state.type_vars, &mut norm_ctx)
        .await
        .map_err(|e| TypeDiagnostic::error("type-error", e.to_string(), span.clone()))?;
    drop(norm_ctx);

    if sub == sup {
        return Ok(());
    }

    match (&sub, &sup) {
        // Error absorption: absorb silently to prevent cascade errors.
        (Type::Error(_), _) | (_, Type::Error(_)) => return Ok(()),

        // Unknown: directional — zero levels of affected vars, accept the constraint.
        (Type::Unknown, Type::Var(name, _)) => {
            state.set_level(name.clone(), 0);
            return Ok(());
        }
        (Type::Var(name, _), Type::Unknown) => {
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
                && members.iter().any(|m| matches!(m, Type::Var(_, _))) =>
        {
            let members = members.clone();
            return bas_cvar1_rewrite(&members, &sub, state, constraints, span, 0).await;
        }

        // [C-VAR2] (Parreaux & Chau 2022, §3.2.1): α ∧ τ₁ ≤ τ₂ → α ≤ ~τ₁ ∨ τ₂
        //
        // Directional: fires only when Intersection is on the LEFT (sub position).
        // In constrain(sub, sup), sub=Intersection means we are constraining the intersection
        // to fit into sup. The TypeVar in the intersection contributes an upper bound.
        (Type::Intersection(members), _)
            if !sup.has_inference_vars()
                && members.iter().any(|m| matches!(m, Type::Var(_, _))) =>
        {
            let members = members.clone();
            return bas_cvar2_rewrite(&members, &sup, state, constraints, span, 0).await;
        }

        // TypeVar accumulation: sub <: TypeVar(α) → α has lower bound sub.
        // Collect sub as a lower bound of α rather than binding α = sub.
        // This preserves principal types: α can still be instantiated to any supertype of sub.
        // Guard: sub must be a ground type (no inference vars) to avoid pushing unsolved TypeVars
        // as bounds. If sub contains TypeVars, fall through to unify() which handles U-VAR-LEVEL.
        (_, Type::Var(var_name, _)) if !sub.has_inference_vars() => {
            let alpha_level = state.get_level_for_occurs_check(var_name);
            if lower_levels_check_occurs(&sub, var_name, alpha_level, state) {
                return Err(TypeDiagnostic::error(
                    "type-error",
                    format!("infinite type: {var_name} occurs in {sub}"),
                    span,
                ));
            }
            let normalized = crate::bas::to_rdnf(&sub);
            let flat = crate::bas::flatten_rdnf_to_type(normalized);
            if !matches!(flat, Type::Never) {
                state
                    .bounds
                    .entry(var_name.clone())
                    .or_default()
                    .add_lower(flat);
            }
            return Ok(());
        }

        // TypeVar accumulation: TypeVar(α) <: sup → α has upper bound sup.
        // Collect sup as an upper bound of α rather than binding α = sup.
        // Guard: sup must be a ground type (no inference vars) to avoid pushing unsolved TypeVars
        // as bounds. If sup contains TypeVars, fall through to unify() which handles U-VAR-LEVEL.
        (Type::Var(var_name, _), _) if !sup.has_inference_vars() => {
            let alpha_level = state.get_level_for_occurs_check(var_name);
            if lower_levels_check_occurs(&sup, var_name, alpha_level, state) {
                return Err(TypeDiagnostic::error(
                    "type-error",
                    format!("infinite type: {var_name} occurs in {sup}"),
                    span,
                ));
            }
            let normalized = crate::bas::to_rdnf(&sup);
            let flat = crate::bas::flatten_rdnf_to_type(normalized);
            if !matches!(flat, Type::Any) {
                state
                    .bounds
                    .entry(var_name.clone())
                    .or_default()
                    .add_upper(flat);
            }
            return Ok(());
        }

        // [C-FN] Structural function-type decomposition with correct variance.
        // Pierce & Turner (2000) §6; Parreaux & Chau (2022) §3.2.1 C-FN:
        //   (S1→T1) ≤ (S2→T2)  iff  S2 ≤ S1 (params contravariant) ∧ T1 ≤ T2 (return covariant)
        //
        // Fires before the ground-type BAS arm so TypeVar-containing function types accumulate
        // bounds rather than binding immediately via unify()'s symmetric equality.
        // Arity/variadic errors come from check_function_arity() — no delegation to unify().
        (
            Type::Function {
                params: p1,
                ret: r1,
                typed_variadics: tv1,
                rest: rest1,
                required_count: _,
            },
            Type::Function {
                params: p2,
                ret: r2,
                typed_variadics: tv2,
                rest: rest2,
                required_count: _,
            },
        ) => {
            let is_variadic_1 = !tv1.is_empty() || rest1.is_some();
            let is_variadic_2 = !tv2.is_empty() || rest2.is_some();
            let is_any_function_1 = p1.is_empty() && is_variadic_1;
            let is_any_function_2 = p2.is_empty() && is_variadic_2;

            // Any-function ≤ any concrete-arity: constrain return types only (covariant).
            if is_any_function_1 && !p2.is_empty() {
                let (r1c, r2c) = ((**r1).clone(), (**r2).clone());
                return Box::pin(constrain(&r1c, &r2c, state, constraints, span)).await;
            }
            if is_any_function_2 && !p1.is_empty() {
                let (r1c, r2c) = ((**r1).clone(), (**r2).clone());
                return Box::pin(constrain(&r1c, &r2c, state, constraints, span)).await;
            }

            // Validate arity and variadic structure; propagate error directly if mismatched.
            check_function_arity(
                p1.len(),
                p2.len(),
                is_variadic_1,
                is_variadic_2,
                tv1.len(),
                tv2.len(),
                rest1.is_some(),
                rest2.is_some(),
                span.clone(),
            )?;

            // Clone to release match borrows before recursive async constrain() calls.
            let p1c: Vec<_> = p1.iter().map(|(n, t)| (n.clone(), t.clone())).collect();
            let p2c: Vec<_> = p2.iter().map(|(n, t)| (n.clone(), t.clone())).collect();
            let tv1c: Vec<_> = tv1.iter().map(|(n, t)| (n.clone(), t.clone())).collect();
            let tv2c: Vec<_> = tv2.iter().map(|(n, t)| (n.clone(), t.clone())).collect();
            let rest1c = rest1.as_ref().map(|b| (b.0.clone(), b.1.clone()));
            let rest2c = rest2.as_ref().map(|b| (b.0.clone(), b.1.clone()));
            let r1c = (**r1).clone();
            let r2c = (**r2).clone();

            // Fixed params: CONTRAVARIANT — constrain(sup_param, sub_param).
            for ((_, ty1), (_, ty2)) in p1c.iter().zip(p2c.iter()) {
                Box::pin(constrain(ty2, ty1, state, constraints, span.clone())).await?;
            }
            // Typed variadics: CONTRAVARIANT.
            for ((_, ty1), (_, ty2)) in tv1c.iter().zip(tv2c.iter()) {
                Box::pin(constrain(ty2, ty1, state, constraints, span.clone())).await?;
            }
            // Rest param: CONTRAVARIANT.
            if let (Some(rb1), Some(rb2)) = (&rest1c, &rest2c) {
                Box::pin(constrain(&rb2.1, &rb1.1, state, constraints, span.clone())).await?;
            }
            // Return type: COVARIANT — constrain(sub_ret, sup_ret).
            return Box::pin(constrain(&r1c, &r2c, state, constraints, span)).await;
        }

        // [C-Dict] Directional record subtyping via constrain_rows.
        (Type::Dict(r1), Type::Dict(r2)) => {
            let (r1c, r2c) = (r1.clone(), r2.clone());
            return constrain_rows(&r1c, &r2c, state, constraints, span).await;
        }

        // [C-NominalVariant] Nominal variant with same tycon/ctor: constrain fields.
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
        ) if tycon1 == tycon2 && ctor1 == ctor2 => {
            let (f1c, f2c) = (fields1.clone(), fields2.clone());
            return constrain_rows(&f1c, &f2c, state, constraints, span).await;
        }

        // [C-App] Type constructor application with variance-directed constraints.
        // Mirrors UNIFY-APP but uses constrain() with correct variance rather than symmetric unify().
        (Type::App(_, _), Type::App(_, _)) => {
            // Attempt variance-directed constraint for TyCon spine forms.
            if let (Some((name1, args1)), Some((name2, args2))) =
                (extract_tycon_spine(&sub), extract_tycon_spine(&sup))
            {
                if name1 == name2 && args1.len() == args2.len() {
                    if let Some(def) = state.tycon_env.get(name1).cloned() {
                        for (i, (arg_sub, arg_sup)) in args1.iter().zip(args2.iter()).enumerate() {
                            let var = if i < def.variance.len() {
                                def.variance[i]
                            } else {
                                return Err(TypeDiagnostic::error(
                                    "type-error",
                                    format!(
                                        "type constructor {} applied with more arguments than declared variance positions",
                                        name1
                                    ),
                                    span.clone(),
                                ));
                            };
                            let (sub_c, sup_c) = ((*arg_sub).clone(), (*arg_sup).clone());
                            match var {
                                Variance::Covariant => {
                                    Box::pin(constrain(
                                        &sub_c,
                                        &sup_c,
                                        state,
                                        constraints,
                                        span.clone(),
                                    ))
                                    .await?;
                                }
                                Variance::Contravariant => {
                                    // CONTRAVARIANT: swap directions!
                                    Box::pin(constrain(
                                        &sup_c,
                                        &sub_c,
                                        state,
                                        constraints,
                                        span.clone(),
                                    ))
                                    .await?;
                                }
                                Variance::Invariant => {
                                    // INVARIANT: bidirectional.
                                    Box::pin(constrain(
                                        &sub_c,
                                        &sup_c,
                                        state,
                                        constraints,
                                        span.clone(),
                                    ))
                                    .await?;
                                    Box::pin(constrain(
                                        &sup_c,
                                        &sub_c,
                                        state,
                                        constraints,
                                        span.clone(),
                                    ))
                                    .await?;
                                }
                                Variance::Phantom => {}
                            }
                        }
                        return Ok(());
                    }
                    // TyCon name match but no TyConDef: fall through to unify() for structural
                    // recursion on components (unify() applies conservative bidirectional constrain on args).
                }
                // Different TyCons: fall through to unify() for error.
            }
            // Not a TyCon spine: fall through to unify() for structural recursion.
        }

        // [C-Recursive] Recursive type constraints: open with fresh TypeVar and constrain.
        (Type::Recursive { var: va, body: ba }, Type::Recursive { var: vb, body: bb }) => {
            let (vac, bac, vbc, bbc) = (va.clone(), (**ba).clone(), vb.clone(), (**bb).clone());
            let fresh = state.fresh_type_var(&span);
            let opened_a = substitute_recvar(&bac, &vac, &fresh);
            let opened_b = substitute_recvar(&bbc, &vbc, &fresh);
            return Box::pin(constrain(&opened_a, &opened_b, state, constraints, span)).await;
        }
        (Type::Recursive { var: va, body: ba }, _) if !matches!(sup, Type::Recursive { .. }) => {
            let (vac, bac) = (va.clone(), (**ba).clone());
            let fresh = state.fresh_type_var(&span);
            let opened_a = substitute_recvar(&bac, &vac, &fresh);
            return Box::pin(constrain(&opened_a, &sup, state, constraints, span)).await;
        }
        (_, Type::Recursive { var: vb, body: bb }) if !matches!(sub, Type::Recursive { .. }) => {
            let (vbc, bbc) = (vb.clone(), (**bb).clone());
            let fresh = state.fresh_type_var(&span);
            let opened_b = substitute_recvar(&bbc, &vbc, &fresh);
            return Box::pin(constrain(&sub, &opened_b, state, constraints, span)).await;
        }

        // [C-Negation] Negation is CONTRAVARIANT: ~T₁ ≤ ~T₂ iff T₂ ≤ T₁ (swap directions).
        (Type::Negation(t1), Type::Negation(t2)) => {
            let (t1c, t2c) = ((**t1).clone(), (**t2).clone());
            return Box::pin(constrain(&t2c, &t1c, state, constraints, span)).await;
        }

        // [C-TypeStageApp] TypeStageApp is INVARIANT: same function name, bidirectional on args.
        (
            Type::StageApp {
                fn_name: f1,
                args: args1,
            },
            Type::StageApp {
                fn_name: f2,
                args: args2,
            },
        ) if f1 == f2 && args1.len() == args2.len() => {
            let pairs: Vec<_> = args1
                .iter()
                .zip(args2.iter())
                .map(|(a, b)| (a.clone(), b.clone()))
                .collect();
            for (a, b) in &pairs {
                Box::pin(constrain(a, b, state, constraints, span.clone())).await?;
                Box::pin(constrain(b, a, state, constraints, span.clone())).await?;
            }
            return Ok(());
        }

        // Ground-type subtype check: neither side contains inference variables.
        // A ≤ B iff is_empty(to_rdnf(A & ~B)) — BAS subtyping judgment.
        // At argument-passing sites, the argument type need only be a SUBTYPE of the
        // parameter type, not EQUAL. This enables [or D1 D2] ≤ Dict(open) when both
        // D1 and D2 are Dict subtypes, and [or [] Dict] ≤ Dict(open) similarly.
        _ if !sub.has_inference_vars() && !sup.has_inference_vars() => {
            let mut sigma = std::collections::HashSet::new();
            if Type::is_subtype_bas(&sub, &sup, None, &mut sigma) {
                return Ok(());
            }
            // Subtype check failed: fall through to unify() for a structured error.
        }

        // Fallthrough to unify(): non-structural cases only.
        // - TypeVar-TypeVar: unify() binds variables (C-Var1/C-Var2 above handle union/intersection)
        // - Atomic mismatches (Int vs Str): unify() produces structured errors
        //
        // INVARIANT: every compound Type variant that carries structural sub-terms has an
        // explicit constrain() arm above — EXCEPT Union/Intersection in the non-TypeVar-bearing
        // position, which fall through to unify() for the following reasons:
        //   • Union in SUP position (with TypeVar): handled by C-Var1 above.
        //   • Intersection in SUB position (with TypeVar): handled by C-Var2 above.
        //   • Union in SUB position WITHOUT TypeVar members: falls through to unify()'s U-SUBSUME
        //     arm, which calls is_subtype(Union, sup) — a correct ground-type check.
        //   • Intersection in SUP position WITHOUT TypeVar members: same U-SUBSUME fallthrough.
        //   • Union/Intersection in either position WITH TypeVar members but no matching C-Var1/2
        //     guard: falls through to unify()'s symmetric handling — a known limitation.
        // If a new compound variant is added, add an explicit constrain() arm for it here.
        // Do not rely on U-SUBSUME fallthrough for TypeVar-containing compound types.
        _ => {}
    }

    unify(&sub, &sup, state, constraints, span, 0).await
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
/// Unification failures are propagated immediately with `?`. A deferred equality that fails
/// unification after both sides are fully reduced is a genuine type error and must be surfaced.
///
/// Called after each SCC's substitution merge in `run_typecheck_dict` (typecheck_cek.rs).
/// Union-vs-Union deferred equalities (from the arm above) also land here.
/// See doc/06-type-inference.md §Constraint Generation rules [U-TSA-DEFER].
pub async fn process_deferred_equalities(
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    span: Span,
) -> Result<(), TypeDiagnostic> {
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
        let mut norm_ctx = crate::type_normalize::NormCtxt::new(state.eval_ctx.clone());
        norm_ctx.type_stage_scope = state.type_stage_scope.clone();
        for (a, b) in deferred {
            // Normalize both sides
            let a_norm = crate::type_normalize::normalize(&a, &state.type_vars, &mut norm_ctx)
                .await
                .map_err(|e| TypeDiagnostic::error("type-error", e.to_string(), span.clone()))?;
            let b_norm = crate::type_normalize::normalize(&b, &state.type_vars, &mut norm_ctx)
                .await
                .map_err(|e| TypeDiagnostic::error("type-error", e.to_string(), span.clone()))?;

            if !a_norm.has_type_stage_app() && !b_norm.has_type_stage_app() {
                // Both sides fully reduced — attempt unification. Propagate failures.
                Box::pin(unify(&a_norm, &b_norm, state, constraints, span.clone(), 0)).await?;
                progress = true;
                // Don't re-defer (fully reduced)
            } else {
                // Still stuck — keep deferred for the next iteration
                state.deferred_equalities.push((a_norm, b_norm));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "type_unify_tests.rs"]
mod type_unify_tests;
