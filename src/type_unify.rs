//! Substitution, unification, and constraint solving for Hindley-Milner polymorphism
//! with Rémy-style row polymorphism and algebraic subtyping.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

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
        "Equatable" => matches!(
            ty,
            Type::Int
                | Type::IntLiteral(_)
                | Type::Float
                | Type::Str
                | Type::StringLiteral(_)
                | Type::Bool
                | Type::Number
        ),
        // Showable: base class for string conversion. Hardcoded for primitives that
        // have built-in str conversion. Combined with structural propagation above,
        // this means Record([x: Int, y: Str]) satisfies Showable because both Int
        // and Str satisfy Showable and the constraint propagates through all fields.
        "Showable" => matches!(
            ty,
            Type::Int
                | Type::IntLiteral(_)
                | Type::Float
                | Type::Str
                | Type::StringLiteral(_)
                | Type::Bool
                | Type::Number
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
                    if vars.len() == 1 && vars[0] == *target_var {
                        if is_superclass_of(class_env, class, target_class) {
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
    // If they're the same, trivially true
    if subclass == superclass {
        return true;
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
        if is_superclass_of(class_env, direct_super, superclass) {
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
fn check_constraints_on_var(
    var_name: &str,
    concrete_ty: &Type,
    subst: &Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // Find all constraints on this variable
    for constraint in &state.constraints.clone() {
        match constraint {
            Constraint::Class { class, vars, .. } if vars.len() == 1 && vars[0] == var_name => {
                // Single-parameter type class constraint (e.g., Numeric a)
                // First, check the fixed instance sets (B4 constrained type variables)
                if satisfies_constraint(concrete_ty, class) {
                    continue;
                }

                // If not in fixed instance set, try instance resolution
                // This enables user-defined instances (future work: dictionary construction)
                // Clone instance_env to avoid borrowing state both immutably (for the
                // field access) and mutably (as the unify parameter) at the same time.
                let inst_env = state.instance_env.clone();
                if inst_env
                    .resolve_instance(class, concrete_ty, state)
                    .is_some()
                {
                    // Instance found - constraint satisfied
                    continue;
                }

                // No instance found - constraint violated
                return Err(TypeError::new(
                    format!("type {} does not satisfy constraint {}", concrete_ty, class),
                    span,
                ));
            }
            Constraint::Class {
                class,
                vars,
                fundeps,
            } if vars.len() > 1 => {
                // Multi-parameter type class constraint with functional dependencies
                // Check if this variable binding triggers FD improvement
                improve_functional_dependency(
                    class,
                    vars,
                    fundeps,
                    var_name,
                    concrete_ty,
                    subst,
                    state,
                    span,
                )?;
            }
            Constraint::HasField { .. } => {
                // HasField constraints are resolved separately in resolve_has_field
                // They don't participate in check_constraints_on_var
                continue;
            }
            _ => continue,
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

        // All determining positions are ground - look up the instance
        let result_type = lookup_arithmetic_instance(
            class,
            &det_types
                .iter()
                .map(|(_, _, ty)| ty.clone())
                .collect::<Vec<_>>(),
        )?;

        // Unify each determined position with the result type
        for &ded_pos in ded_positions {
            if ded_pos >= vars.len() {
                continue;
            }
            let ded_var = &vars[ded_pos];
            let ded_type_var = Type::TypeVar(ded_var.clone(), 0);

            // Unify the determined variable with the result type
            // Use std::mem::take to avoid borrow conflicts (same pattern as typecheck.rs:2124)
            let mut subst = std::mem::take(&mut state.subst);
            let result = unify(&ded_type_var, &result_type, &mut subst, state, span);
            state.subst = subst;
            result?;
        }
    }

    Ok(())
}

/// Hardcoded instance lookup for arithmetic type classes with functional dependencies.
/// Given the determining types (a, b), returns the determined type (c).
///
/// This is a FAST PATH for the 9 builtin instances of Add/Sub/Mul/Div (all have FD (a,b) → c).
/// For other MPTCs with functional dependencies, this function should be generalized to query
/// `state.instance_env` for matching instances (future work: sprint type-precision-fixes Task 5).
///
/// GENERALIZATION PLAN:
/// 1. Given class name and determining types, iterate `instance_env.iter_instances()`
/// 2. For each instance of the class, extract the instance type (which for MPTCs is App-encoded)
/// 3. Check if determining positions match the given types (via unification or syntactic equality)
/// 4. Return the determined type(s) if a unique match is found
/// 5. Keep this hardcoded table as the fast path for Add/Sub/Mul/Div (performance)
fn lookup_arithmetic_instance(class: &str, det_types: &[Type]) -> Result<Type, TypeError> {
    if det_types.len() != 2 {
        return Err(TypeError::new(
            format!(
                "arithmetic class {} expects 2 determining types, got {}",
                class,
                det_types.len()
            ),
            Span::origin(),
        ));
    }

    let a = &det_types[0];
    let b = &det_types[1];

    // Normalize types for comparison
    let key = (type_key(a), type_key(b));

    // FAST PATH: hardcoded instances for Add/Sub/Mul/Div (performance)
    match class {
        "Add" | "Sub" | "Mul" => match key {
            ("Int", "Int") => Ok(Type::Int),
            ("Float", "Float") => Ok(Type::Float),
            ("Int", "Float") | ("Float", "Int") => Ok(Type::Float),
            ("Number", "Number")
            | ("Number", "Int")
            | ("Int", "Number")
            | ("Number", "Float")
            | ("Float", "Number") => Ok(Type::Number),
            _ => Err(TypeError::new(
                format!("no instance for {} {} {}", class, a, b),
                Span::origin(),
            )),
        },
        "Div" => match key {
            ("Int", "Int") | ("Float", "Float") | ("Int", "Float") | ("Float", "Int") => {
                Ok(Type::Float)
            }
            ("Number", "Number")
            | ("Number", "Int")
            | ("Int", "Number")
            | ("Number", "Float")
            | ("Float", "Number") => Ok(Type::Number),
            _ => Err(TypeError::new(
                format!("no instance for Div {} {}", a, b),
                Span::origin(),
            )),
        },
        _ => {
            // FUTURE: general MPTC instance lookup would go here
            // For now, return error for unknown classes (no other MPTCs with FDs exist yet)
            Err(TypeError::new(
                format!("unknown arithmetic class {}", class),
                Span::origin(),
            ))
        }
    }
}

/// Helper to extract a type key for instance lookup
fn type_key(ty: &Type) -> &'static str {
    match ty {
        Type::Int => "Int",
        Type::Float => "Float",
        Type::Number => "Number",
        Type::IntLiteral(_) => "Int", // Promoted
        _ => "Unknown",
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
        "Add",
        "Sub",
        "Mul",
        "Div",
    ];

    let has_promotable_constraint = state.constraints.iter().any(|c| match c {
        Constraint::Class { class, vars, .. } => {
            vars.contains(&var_name.to_string()) && PROMOTABLE_CLASSES.contains(&class.as_str())
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
            format!(
                "cannot resolve HasField constraint on unbound type variable (expected caller to defer)"
            ),
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
            | Type::Handle
            | Type::Uri
            | Type::Timestamp
            | Type::Duration
            | Type::ClockCap
            | Type::Timezone
            | Type::QuicSession
            | Type::Http2Session
            | Type::Http3Session
            | Type::DatagramHandle => {
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
                    match ctor_name.as_str() {
                        "Seq" => return Cow::Owned(Type::Seq(Box::new(a_applied))),
                        _ => {}
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
        _ => false,
    }
}

/// Compact type variable bounds into a concrete type.
/// - Multiple lower bounds -> inferred union: α ⊇ Int, α ⊇ Str -> α = Int | Str
/// - Multiple upper bounds -> inferred intersection: α ⊆ Number, α ⊆ Equatable -> α = Number & Equatable
/// - Both lower and upper -> meet the bounds, preferring the lower (more specific)
/// - No bounds -> Unknown (unconstrained variable)
///
/// This is used during let-generalization to resolve type variables to concrete types
/// when they have accumulated bounds but no direct equality binding.
#[allow(dead_code)] // Scaffolding for algebraic subtyping migration
pub fn compact_bounds(var_name: &str, bounds: &TypeVarBounds, level: u32) -> Type {
    match (bounds.lower.is_empty(), bounds.upper.is_empty()) {
        (true, true) => {
            // No bounds -- unconstrained type variable remains as TypeVar
            Type::TypeVar(var_name.to_string(), level)
        }
        (false, true) => {
            // Only lower bounds -> union
            if bounds.lower.len() == 1 {
                bounds.lower[0].clone()
            } else {
                Type::normalize_union(bounds.lower.clone())
            }
        }
        (true, false) => {
            // Only upper bounds -> intersection
            if bounds.upper.len() == 1 {
                bounds.upper[0].clone()
            } else {
                Type::normalize_intersection(bounds.upper.clone())
            }
        }
        (false, false) => {
            // Both lower and upper bounds -> prefer lower (more specific)
            // In full algebraic subtyping, we'd compute meet(upper) ∩ join(lower),
            // but for the migration phase, we use the lower bound as the principal type.
            if bounds.lower.len() == 1 {
                bounds.lower[0].clone()
            } else {
                Type::normalize_union(bounds.lower.clone())
            }
        }
    }
}

/// Check that all type variable bounds are satisfiable.
/// A type variable α is satisfiable iff join(lower) <: meet(upper), where:
/// - join(lower) = Type::normalize_union(lower bounds) if multiple, else the single bound
/// - meet(upper) = Type::normalize_intersection(upper bounds) if multiple, else the single bound
///
/// This should be called after constraint accumulation to detect unsatisfiable bounds.
#[allow(dead_code)] // Scaffolding for algebraic subtyping migration
pub fn check_bounds_satisfiable(state: &InferState, span: Span) -> Result<(), TypeError> {
    for (var_name, bounds) in &state.bounds {
        // Compute join of lower bounds
        let lower_joined = if bounds.lower.is_empty() {
            None
        } else if bounds.lower.len() == 1 {
            Some(bounds.lower[0].clone())
        } else {
            Some(Type::normalize_union(bounds.lower.clone()))
        };

        // Compute meet of upper bounds
        let upper_meet = if bounds.upper.is_empty() {
            None
        } else if bounds.upper.len() == 1 {
            Some(bounds.upper[0].clone())
        } else {
            Some(Type::normalize_intersection(bounds.upper.clone()))
        };

        // Check satisfiability: join(lower) <: meet(upper)
        match (lower_joined, upper_meet) {
            (Some(lower), Some(upper)) => {
                if !Type::is_subtype(&lower, &upper) {
                    return Err(TypeError {
                        message: format!(
                            "unsatisfiable bounds for type variable {}: lower bound {} is not a subtype of upper bound {}",
                            var_name, lower, upper
                        ),
                        span,
                        notes: Vec::new(),
                    });
                }
            }
            (None, None) => {
                // No bounds -- always satisfiable
            }
            (Some(_), None) | (None, Some(_)) => {
                // Only lower or only upper -- always satisfiable
            }
        }
    }
    Ok(())
}

#[allow(dead_code)] // Scaffolding for algebraic subtyping migration
pub fn constrain(
    sub: &Type,
    sup: &Type,
    state: &mut InferState,
    span: Span,
    reason: &str,
) -> Result<(), TypeError> {
    // Apply substitution to both sides (chase existing bindings)
    let sub = state.subst.apply(sub);
    let sup = state.subst.apply(sup);

    // Reflexive: τ <: τ
    if sub == sup {
        return Ok(());
    }

    // Error absorption: Error <: τ and τ <: Error are no-ops
    if matches!(sub, Type::Error) || matches!(sup, Type::Error) {
        return Ok(());
    }

    // Top is the supertype of everything: τ <: Top for all τ
    if matches!(sup, Type::Top) {
        return Ok(());
    }

    // Unknown consistency: Unknown relates via consistency, not subtyping.
    // Treat Unknown ~ τ as always satisfiable (gradual typing).
    if matches!(sub, Type::Unknown) || matches!(sup, Type::Unknown) {
        return Ok(());
    }

    match (&sub, &sup) {
        // Type variable in subtype position: α <: τ -> τ is an upper bound on α
        (Type::TypeVar(var_name, _), _) => {
            let bounds = state
                .bounds
                .entry(var_name.clone())
                .or_insert_with(TypeVarBounds::new);
            bounds.add_upper(sup.clone());
            Ok(())
        }

        // Type variable in supertype position: τ <: α -> τ is a lower bound on α
        (_, Type::TypeVar(var_name, _)) => {
            let bounds = state
                .bounds
                .entry(var_name.clone())
                .or_insert_with(TypeVarBounds::new);
            bounds.add_lower(sub.clone());
            Ok(())
        }

        // Literal promotion
        (Type::IntLiteral(_), Type::Int | Type::Number) => Ok(()),
        (Type::StringLiteral(_), Type::Str) => Ok(()),
        (Type::Int | Type::Float, Type::Number) => Ok(()),

        // Seq covariance: Seq[A] <: Seq[B] iff A <: B
        (Type::Seq(sub_elem), Type::Seq(sup_elem)) => {
            constrain(sub_elem, sup_elem, state, span, "seq element")
        }

        // Function contravariant params, covariant return
        (
            Type::Function {
                params: sub_params,
                ret: sub_ret,
                variadic: sub_variadic,
            },
            Type::Function {
                params: sup_params,
                ret: sup_ret,
                variadic: sup_variadic,
            },
        ) => {
            if sub_variadic != sup_variadic {
                return Err(TypeError {
                    message: format!(
                        "variadic mismatch: cannot constrain {} <: {} ({})",
                        sub, sup, reason
                    ),
                    span,
                    notes: Vec::new(),
                });
            }
            if sub_params.len() != sup_params.len() {
                return Err(TypeError {
                    message: format!(
                        "function arity mismatch: {} params vs {} params ({})",
                        sub_params.len(),
                        sup_params.len(),
                        reason
                    ),
                    span,
                    notes: Vec::new(),
                });
            }

            // Parameters are CONTRAVARIANT: Fn(A->...) <: Fn(B->...) requires B <: A
            for ((_sub_name, sub_param_ty), (_sup_name, sup_param_ty)) in
                sub_params.iter().zip(sup_params.iter())
            {
                constrain(
                    sup_param_ty,
                    sub_param_ty,
                    state,
                    span,
                    "function parameter (contravariant)",
                )?;
            }

            // Return is COVARIANT: Fn(...->A) <: Fn(...->B) requires A <: B
            constrain(sub_ret, sup_ret, state, span, "function return (covariant)")
        }

        // Record structural subtyping with row polymorphism
        (Type::Record(sub_row), Type::Record(sup_row)) => {
            // All fields in sup must be present in sub with subtype field types
            for (key, sup_ty) in &sup_row.fields {
                match sub_row.fields.get(key) {
                    Some(sub_ty) => {
                        constrain(
                            sub_ty,
                            sup_ty,
                            state,
                            span,
                            &format!("record field '{}'", key),
                        )?;
                    }
                    None => {
                        return Err(TypeError {
                            message: format!(
                                "record missing required field '{}': {} does not satisfy {} ({})",
                                key, sub, sup, reason
                            ),
                            span,
                            notes: Vec::new(),
                        });
                    }
                }
            }

            // BAS: tail check — under BAS all tails are Empty, width subtyping handled by is_subtype
            Ok(())
        }

        // Union subtyping
        // [UNION-INJ]: A <: A | B and B <: A | B (any concrete type is subtype of union containing it)
        (_, Type::Union(sup_members)) => {
            // τ <: Union iff τ <: at least one member
            for member in sup_members {
                if constrain(&sub, member, state, span, reason).is_ok() {
                    return Ok(());
                }
            }
            Err(TypeError {
                message: format!(
                    "type {} is not a subtype of any union member in {} ({})",
                    sub, sup, reason
                ),
                span,
                notes: Vec::new(),
            })
        }

        // [UNION-ELIM]: A | B <: τ iff A <: τ AND B <: τ
        (Type::Union(sub_members), _) => {
            for member in sub_members {
                constrain(member, &sup, state, span, reason)?;
            }
            Ok(())
        }

        // Intersection subtyping
        // [INTERSECT-INTRO]: A & B <: A and A & B <: B (intersection is subtype of each member)
        (Type::Intersection(sub_members), _) => {
            // Intersection <: τ iff at least one member <: τ
            for member in sub_members {
                if constrain(member, &sup, state, span, reason).is_ok() {
                    return Ok(());
                }
            }
            Err(TypeError {
                message: format!(
                    "no intersection member in {} is a subtype of {} ({})",
                    sub, sup, reason
                ),
                span,
                notes: Vec::new(),
            })
        }

        // [INTERSECT-ELIM]: τ <: A & B iff τ <: A AND τ <: B
        (_, Type::Intersection(sup_members)) => {
            for member in sup_members {
                constrain(&sub, member, state, span, reason)?;
            }
            Ok(())
        }

        // Incompatible ground types
        _ => Err(TypeError {
            message: format!(
                "type mismatch: {} is not a subtype of {} ({})",
                sub, sup, reason
            ),
            span,
            notes: Vec::new(),
        }),
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
    // Collect all Class constraints on α. Early-exit if there are none.
    // For single-param constraints only (MPTC transfer is more complex and deferred)
    let alpha_constraints: Vec<(String, Vec<String>, Vec<(Vec<usize>, Vec<usize>)>)> = state
        .constraints
        .iter()
        .filter_map(|c| match c {
            Constraint::Class {
                class,
                vars,
                fundeps,
            } if vars.len() == 1 && vars[0] == alpha => {
                Some((class.clone(), vars.clone(), fundeps.clone()))
            }
            _ => None,
        })
        .collect();
    if alpha_constraints.is_empty() {
        return;
    }

    // Transfer to β (deduplicated: only add if not already present).
    // Collect β's existing class names as owned Strings so we do not hold a
    // shared borrow on `state.constraints` while the loop pushes new entries.
    let beta_existing: HashSet<String> = state
        .constraints
        .iter()
        .filter_map(|c| match c {
            Constraint::Class { class, vars, .. } if vars.len() == 1 && vars[0] == beta => {
                Some(class.clone())
            }
            _ => None,
        })
        .collect();
    for (class, _vars, fundeps) in alpha_constraints {
        if !beta_existing.contains(&class) {
            state.constraints.push(Constraint::Class {
                class,
                vars: vec![beta.to_string()],
                fundeps,
            });
        }
    }
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
    let a = subst.apply_with_visited(a, &mut visited_types, &mut visited_rows);
    visited_types.clear();
    let b = subst.apply_with_visited(b, &mut visited_types, &mut visited_rows);

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
            let mut row_vars = HashSet::new();
            other.collect_all_vars(&mut type_vars, &mut row_vars);
            for var in type_vars.iter().chain(row_vars.iter()) {
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
            let mut row_vars = HashSet::new();
            other.collect_all_vars(&mut type_vars, &mut row_vars);
            for var in type_vars.iter().chain(row_vars.iter()) {
                state.levels.insert(var.clone(), 0);
            }
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
            // This matches the is_subtype invariance check (k1 == k2).
            // Use bidirectional subtype check to handle type aliases and normalization.
            if !Type::is_subtype(k1, k2) || !Type::is_subtype(k2, k1) {
                return Err(TypeError::new(
                    format!("Map key types differ: {} vs {}", k1, k2),
                    span,
                ));
            }
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
        (Type::Handle, Type::Handle) => Ok(()),
        (Type::DatagramHandle, Type::DatagramHandle) => Ok(()),

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
            // Partition union members into TypeVars and concrete members
            let type_vars: Vec<_> = members
                .iter()
                .filter(|m| matches!(m, Type::TypeVar(_, _)))
                .collect();
            let concrete_members: Vec<_> = members
                .iter()
                .filter(|m| !matches!(m, Type::TypeVar(_, _)))
                .collect();

            // C-Var1 applies when there is exactly one TypeVar in the union
            if type_vars.len() == 1 {
                let already_covered = concrete_members
                    .iter()
                    .any(|m| Type::is_subtype(concrete, m));

                if already_covered {
                    // The concrete type is already covered by a non-var member — no binding needed.
                    Ok(())
                } else {
                    // Bind the TypeVar to the concrete type (conservative C-Var1)
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
                    let concrete_promoted =
                        promote_literal_for_constrained_var(var_name, concrete.clone(), state);
                    check_constraints_on_var(var_name, &concrete_promoted, subst, state, span)?;
                    subst
                        .type_map
                        .borrow_mut()
                        .insert(var_name.clone(), concrete_promoted.clone());
                    subst.check_size(span)?;
                    Ok(())
                }
            } else {
                // More than one TypeVar, or zero TypeVars with no subtype relation — fall through
                Err(TypeError::type_mismatch(&a, &b, span))
            }
        }

        // Symmetric C-Var1: Union on the left, concrete on the right
        (Type::Union(members), concrete) if !concrete.has_inference_vars() => {
            let type_vars: Vec<_> = members
                .iter()
                .filter(|m| matches!(m, Type::TypeVar(_, _)))
                .collect();
            let concrete_members: Vec<_> = members
                .iter()
                .filter(|m| !matches!(m, Type::TypeVar(_, _)))
                .collect();

            if type_vars.len() == 1 {
                let already_covered = concrete_members
                    .iter()
                    .any(|m| Type::is_subtype(concrete, m));

                if already_covered {
                    Ok(())
                } else {
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
                    let concrete_promoted =
                        promote_literal_for_constrained_var(var_name, concrete.clone(), state);
                    check_constraints_on_var(var_name, &concrete_promoted, subst, state, span)?;
                    subst
                        .type_map
                        .borrow_mut()
                        .insert(var_name.clone(), concrete_promoted.clone());
                    subst.check_size(span)?;
                    Ok(())
                }
            } else {
                Err(TypeError::type_mismatch(&a, &b, span))
            }
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
            let type_vars: Vec<_> = members
                .iter()
                .filter(|m| matches!(m, Type::TypeVar(_, _)))
                .collect();
            let concrete_members: Vec<_> = members
                .iter()
                .filter(|m| !matches!(m, Type::TypeVar(_, _)))
                .collect();

            if type_vars.len() == 1 {
                // If concrete members already satisfy the target, TypeVar can be anything
                let already_satisfied = concrete_members
                    .iter()
                    .any(|m| Type::is_subtype(m, concrete));

                if already_satisfied {
                    Ok(())
                } else {
                    // Bind TypeVar to the target concrete type (conservative C-Var2)
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
                    let concrete_promoted =
                        promote_literal_for_constrained_var(var_name, concrete.clone(), state);
                    check_constraints_on_var(var_name, &concrete_promoted, subst, state, span)?;
                    subst
                        .type_map
                        .borrow_mut()
                        .insert(var_name.clone(), concrete_promoted.clone());
                    subst.check_size(span)?;
                    Ok(())
                }
            } else {
                Err(TypeError::type_mismatch(&a, &b, span))
            }
        }

        // Symmetric C-Var2: concrete on the left, Intersection on the right
        (concrete, Type::Intersection(members)) if !concrete.has_inference_vars() => {
            let type_vars: Vec<_> = members
                .iter()
                .filter(|m| matches!(m, Type::TypeVar(_, _)))
                .collect();
            let concrete_members: Vec<_> = members
                .iter()
                .filter(|m| !matches!(m, Type::TypeVar(_, _)))
                .collect();

            if type_vars.len() == 1 {
                let already_satisfied = concrete_members
                    .iter()
                    .any(|m| Type::is_subtype(m, concrete));

                if already_satisfied {
                    Ok(())
                } else {
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
                    let concrete_promoted =
                        promote_literal_for_constrained_var(var_name, concrete.clone(), state);
                    check_constraints_on_var(var_name, &concrete_promoted, subst, state, span)?;
                    subst
                        .type_map
                        .borrow_mut()
                        .insert(var_name.clone(), concrete_promoted.clone());
                    subst.check_size(span)?;
                    Ok(())
                }
            } else {
                Err(TypeError::type_mismatch(&a, &b, span))
            }
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

#[cfg(test)]
#[path = "type_unify_tests.rs"]
mod type_unify_tests;
