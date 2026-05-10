//! Substitution, unification, and constraint solving for Hindley-Milner polymorphism
//! with Rémy-style row polymorphism and algebraic subtyping.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::ast::Span;

use super::*;

/// Check if a type satisfies a type class constraint.
/// Returns true if the type is an instance of the class.
/// This implements the fixed instance sets for Elm-style constrained type variables.
pub fn satisfies_constraint(ty: &Type, class_name: &str) -> bool {
    match class_name {
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
        "Comparable" => matches!(
            ty,
            Type::Int
                | Type::IntLiteral(_)
                | Type::Float
                | Type::Str
                | Type::StringLiteral(_)
                | Type::Number
        ),
        "Numeric" => matches!(
            ty,
            Type::Int | Type::IntLiteral(_) | Type::Float | Type::Number
        ),
        "Showable" => {
            // All types are showable (all have str representations)
            !matches!(ty, Type::Error)
        }
        "Mappable" => matches!(ty, Type::Record(_) | Type::Seq(_)),
        "Appendable" => matches!(
            ty,
            Type::Str | Type::StringLiteral(_) | Type::Record(_) | Type::Seq(_)
        ),
        _ => false, // Unknown constraint class
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
    for constraint in context {
        if constraint.var == target.var {
            if is_superclass_of(class_env, &constraint.class, &target.class) {
                return true;
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

    // Check direct superclasses
    if subclass_decl.superclasses.contains(&superclass.to_string()) {
        return true;
    }

    // Check transitive superclasses (recursively)
    for direct_super in &subclass_decl.superclasses {
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
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // Find all constraints on this variable
    for constraint in &state.constraints.clone() {
        if constraint.var == var_name {
            // First, check the fixed instance sets (B4 constrained type variables)
            if satisfies_constraint(concrete_ty, &constraint.class) {
                continue;
            }

            // If not in fixed instance set, try instance resolution
            // This enables user-defined instances (future work: dictionary construction)
            // Clone instance_env to avoid borrowing state both immutably (for the
            // field access) and mutably (as the unify parameter) at the same time.
            let inst_env = state.instance_env.clone();
            if inst_env
                .resolve_instance(&constraint.class, concrete_ty, state)
                .is_some()
            {
                // Instance found - constraint satisfied
                continue;
            }

            // No instance found - constraint violated
            return Err(TypeError::new(
                format!(
                    "type {} does not satisfy constraint {}",
                    concrete_ty, constraint.class
                ),
                span,
            ));
        }
    }
    Ok(())
}

/// When binding a constrained type variable, promote literal types to their parent types.
/// This prevents `[+ 1 2]` from failing: without promotion, `_t0` (Numeric) would bind
/// to `IntLiteral(1)`, then unification of `IntLiteral(1)` with `IntLiteral(2)` would fail.
/// With promotion, `_t0` binds to `Int`, and both `IntLiteral(1)` and `IntLiteral(2)` unify
/// with `Int` via the literal-to-parent promotion rules.
fn promote_literal_for_constrained_var(var_name: &str, ty: Type, state: &InferState) -> Type {
    // Only promote if the variable has constraints
    let has_constraints = state.constraints.iter().any(|c| c.var == var_name);
    if !has_constraints {
        return ty;
    }
    match ty {
        Type::IntLiteral(_) => Type::Int,
        Type::StringLiteral(_) => Type::Str,
        _ => ty,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Substitution {
    pub type_map: HashMap<String, Type>, // α → τ  (kind: Type)
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
            type_map: HashMap::new(),
        }
    }

    /// Check if the substitution is empty (no bindings).
    /// Used to guard against unnecessary allocation in apply() operations.
    pub fn is_empty(&self) -> bool {
        self.type_map.is_empty()
    }

    /// Check if the substitution has exceeded the maximum allowed size.
    /// Returns an error if type_map exceeds MAX_SUBST_SIZE.
    pub(crate) fn check_size(&self, span: Span) -> Result<(), TypeError> {
        if self.type_map.len() > MAX_SUBST_SIZE {
            Err(TypeError::new(
                format!(
                    "type inference resource limit exceeded (substitution size {} > {}) — use fewer chained dot-accesses or add explicit type annotations to break constraint chains",
                    self.type_map.len(), MAX_SUBST_SIZE
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
                match self.type_map.get(name) {
                    Some(bound) => {
                        visited_types.insert(name.clone());
                        // Reset depth to 0 when following a TypeVar binding: chain-following
                        // is cycle-protected by visited_types; depth guards structural
                        // recursion only. Resetting prevents premature truncation of
                        // long-but-shallow substitution chains (items 5/6).
                        let result = self
                            .apply_type(bound, 0, visited_types)
                            .into_owned();
                        visited_types.remove(name);
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
                            self.apply_type(p_ty, depth + 1, visited_types)
                                .into_owned(),
                        )
                    })
                    .collect(),
                ret: Box::new(
                    self.apply_type(ret, depth + 1, visited_types)
                        .into_owned(),
                ),
                variadic: *variadic,
            }),
            Type::Seq(elem) => Cow::Owned(Type::Seq(Box::new(
                self.apply_type(elem, depth + 1, visited_types)
                    .into_owned(),
            ))),
            Type::Map(key, val) => Cow::Owned(Type::Map(
                Box::new(
                    self.apply_type(key, depth + 1, visited_types)
                        .into_owned(),
                ),
                Box::new(
                    self.apply_type(val, depth + 1, visited_types)
                        .into_owned(),
                ),
            )),
            Type::Union(members) => {
                let applied_members: Vec<Type> = members
                    .iter()
                    .map(|m| {
                        self.apply_type(m, depth + 1, visited_types)
                            .into_owned()
                    })
                    .collect();
                // Re-normalize after substitution to maintain invariants
                Cow::Owned(Type::normalize_union(applied_members))
            }
            Type::Intersection(members) => {
                let applied_members: Vec<Type> = members
                    .iter()
                    .map(|m| {
                        self.apply_type(m, depth + 1, visited_types)
                            .into_owned()
                    })
                    .collect();
                // Re-normalize after substitution to maintain invariants
                Cow::Owned(Type::normalize_intersection(applied_members))
            }
            Type::Negation(inner) => Cow::Owned(Type::Negation(Box::new(
                self.apply_type(inner, depth + 1, visited_types)
                    .into_owned(),
            ))),
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
                    self.apply_type(v, depth + 1, visited_types)
                        .into_owned(),
                )
            })
            .collect();

        Row { fields: new_fields }
    }

    /// Test-only introspection: lookup a type variable binding in the type_map.
    /// Used in type checker tests for asserting substitution contents; not called from production code.
    /// For production access to substitution results, use `apply()` instead.
    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<&Type> {
        self.type_map.get(name)
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
    // Fields unique to one side are not errors — BAS width subtyping handles them.
    for (key, ty1) in &row1.fields {
        if let Some(ty2) = row2.fields.get(key) {
            unify(ty1, ty2, subst, state, span)?;
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
    // For now, treat Unknown ~ τ as always satisfiable (gradual typing).
    // TODO: when gradual-typing-split is complete, this needs refinement.
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
            // Check constraints before binding
            check_constraints_on_var(name, &b, state, span)?;
            subst.type_map.insert(name.clone(), b);
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
            // Check constraints before binding
            check_constraints_on_var(name, &a, state, span)?;
            subst.type_map.insert(name.clone(), a);
            subst.check_size(span)?;
            Ok(())
        }

        // Literal-to-parent promotions
        // Note: These rules are bidirectional (IntLiteral <-> Int) for unification symmetry.
        // In a pure subtyping system, only IntLiteral <: Int would hold (not vice versa).
        // Bidirectional promotion simplifies unification but reduces diagnostic precision:
        // unify(Int, IntLiteral(42)) succeeds, whereas is_subtype(Int, IntLiteral(42)) = false.
        (Type::IntLiteral(_), Type::Int | Type::Number) | (Type::Int, Type::Number) => Ok(()),
        (Type::Int | Type::Number, Type::IntLiteral(_)) | (Type::Number, Type::Int) => Ok(()),
        (Type::Float, Type::Number) | (Type::Number, Type::Float) => Ok(()),
        (Type::StringLiteral(_), Type::Str) | (Type::Str, Type::StringLiteral(_)) => Ok(()),
        (Type::IntLiteral(v1), Type::IntLiteral(v2)) => {
            if v1 == v2 {
                Ok(())
            } else {
                Err(TypeError::type_mismatch(
                    &Type::IntLiteral(*v1),
                    &Type::IntLiteral(*v2),
                    span,
                ))
            }
        }
        (Type::StringLiteral(s1), Type::StringLiteral(s2)) => {
            if s1 == s2 {
                Ok(())
            } else {
                Err(TypeError::type_mismatch(
                    &Type::StringLiteral(s1.clone()),
                    &Type::StringLiteral(s2.clone()),
                    span,
                ))
            }
        }
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

        // Never unification: Never unifies with anything (it's the bottom type)
        // TypeVar cases are handled by the general TypeVar patterns above (lines 1553, 1575)
        (Type::Never, _) | (_, Type::Never) => Ok(()),

        // Negation unification: structural (for now, basic support)
        (Type::Negation(t1), Type::Negation(t2)) => unify(t1, t2, subst, state, span),

        // Conservative Negation compatibility: any concrete type unifies with a Negation type.
        // Full BAS would require checking that the concrete type is disjoint from the negated type
        // (T <: ~A iff T ∩ A = Never) via RDNF normalization, which is not yet implemented.
        // For now, Type::Negation acts as a constraint that is enforced conservatively at runtime
        // (value_matches_type always returns true for Negation) rather than statically.
        // This prevents false type errors for `[@[[without T]] expr]` TypeAsserts.
        (_, Type::Negation(_)) | (Type::Negation(_), _) => Ok(()),

        // Capability types: reflexive unification only
        (Type::DirCap, Type::DirCap) => Ok(()),
        (Type::NetCap, Type::NetCap) => Ok(()),
        (Type::Handle, Type::Handle) => Ok(()),
        (Type::DatagramHandle, Type::DatagramHandle) => Ok(()),

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
                    check_constraints_on_var(var_name, &concrete_promoted, state, span)?;
                    subst
                        .type_map
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
                    check_constraints_on_var(var_name, &concrete_promoted, state, span)?;
                    subst
                        .type_map
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
                    check_constraints_on_var(var_name, &concrete_promoted, state, span)?;
                    subst
                        .type_map
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
                    check_constraints_on_var(var_name, &concrete_promoted, state, span)?;
                    subst
                        .type_map
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
