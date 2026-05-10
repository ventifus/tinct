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
    pub row_map: HashMap<String, Row>,   // ρ → r  (kind: Row)
}

const MAX_APPLY_DEPTH: usize = 256;

/// Maximum size of the substitution map (combined type_map + row_map entries).
/// Prevents resource exhaustion from quadratic growth in pathological cases.
/// Raised from 10K to 50K to accommodate real-world K8s-style configs with
/// hundreds of open-record dot-accesses that each bind a fresh row variable.
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
            row_map: HashMap::new(),
        }
    }

    /// Check if the substitution is empty (no bindings in either map).
    /// Used to guard against unnecessary allocation in apply() operations.
    pub fn is_empty(&self) -> bool {
        self.type_map.is_empty() && self.row_map.is_empty()
    }

    /// Check if the substitution has exceeded the maximum allowed size.
    /// Returns an error if the combined size of type_map and row_map exceeds MAX_SUBST_SIZE.
    pub(crate) fn check_size(&self, span: Span) -> Result<(), TypeError> {
        let total_size = self.type_map.len() + self.row_map.len();
        if total_size > MAX_SUBST_SIZE {
            Err(TypeError::new(
                format!(
                    "type inference resource limit exceeded (substitution size {} > {}) — use fewer chained dot-accesses or add explicit type annotations to break constraint chains",
                    total_size, MAX_SUBST_SIZE
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
        // Avoids allocating visited_types/visited_rows HashSets for the common case.
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
            | Type::Http3Session => {
                return ty.clone();
            }
            _ => {}
        }
        let mut visited_types = HashSet::new();
        let mut visited_rows = HashSet::new();
        self.apply_type(ty, 0, &mut visited_types, &mut visited_rows)
            .into_owned()
    }

    /// Apply substitution with externally-supplied visited sets.
    /// Allows sharing visited sets across multiple apply() calls to avoid repeated allocation.
    /// The caller must clear the visited sets between uses.
    pub fn apply_with_visited(
        &self,
        ty: &Type,
        visited_types: &mut HashSet<String>,
        visited_rows: &mut HashSet<String>,
    ) -> Type {
        if self.is_empty() {
            return ty.clone();
        }
        self.apply_type(ty, 0, visited_types, visited_rows)
            .into_owned()
    }

    fn apply_type<'a>(
        &self,
        ty: &'a Type,
        depth: usize,
        visited_types: &mut HashSet<String>,
        visited_rows: &mut HashSet<String>,
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
                            .apply_type(bound, 0, visited_types, visited_rows)
                            .into_owned();
                        visited_types.remove(name);
                        Cow::Owned(result)
                    }
                    None => Cow::Owned(Type::TypeVar(name.clone(), *level)),
                }
            }
            Type::Record(row) => {
                let applied_row = self.apply_row(row, depth + 1, visited_types, visited_rows);
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
                            self.apply_type(p_ty, depth + 1, visited_types, visited_rows)
                                .into_owned(),
                        )
                    })
                    .collect(),
                ret: Box::new(
                    self.apply_type(ret, depth + 1, visited_types, visited_rows)
                        .into_owned(),
                ),
                variadic: *variadic,
            }),
            Type::Seq(elem) => Cow::Owned(Type::Seq(Box::new(
                self.apply_type(elem, depth + 1, visited_types, visited_rows)
                    .into_owned(),
            ))),
            Type::Union(members) => {
                let applied_members: Vec<Type> = members
                    .iter()
                    .map(|m| {
                        self.apply_type(m, depth + 1, visited_types, visited_rows)
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
                        self.apply_type(m, depth + 1, visited_types, visited_rows)
                            .into_owned()
                    })
                    .collect();
                // Re-normalize after substitution to maintain invariants
                Cow::Owned(Type::normalize_intersection(applied_members))
            }
            Type::Negation(inner) => Cow::Owned(Type::Negation(Box::new(
                self.apply_type(inner, depth + 1, visited_types, visited_rows)
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
        visited_rows: &mut HashSet<String>,
    ) -> Row {
        if depth >= MAX_APPLY_DEPTH {
            return row.clone();
        }

        // Apply substitution to field types. apply_type returns Cow<'_, Type>;
        // .into_owned() is called here because new_fields needs owned Types.
        // Primitive field types (Int, Str, etc.) avoid cloning inside apply_type
        // and only allocate here when ownership is required for the HashMap.
        let new_fields: HashMap<String, Type> = row
            .fields
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    self.apply_type(v, depth + 1, visited_types, visited_rows)
                        .into_owned(),
                )
            })
            .collect();

        // Resolve tail
        match &row.tail {
            RowTail::Empty => Row {
                fields: new_fields,
                tail: RowTail::Empty,
            },
            RowTail::RowVar(name, level) => {
                if visited_rows.contains(name) {
                    // Cycle detected: return unresolved row var
                    return Row {
                        fields: new_fields,
                        tail: RowTail::RowVar(name.clone(), *level),
                    };
                }
                match self.row_map.get(name) {
                    Some(bound_row) => {
                        visited_rows.insert(name.clone());
                        // Reset depth to 0 when following a RowVar binding: cycle-protection
                        // is handled by visited_rows; depth guards structural recursion only.
                        let resolved = self.apply_row(bound_row, 0, visited_types, visited_rows);
                        visited_rows.remove(name);

                        // Merge fields: explicit fields (new_fields) take precedence.
                        // Duplicates CAN legitimately arise here: a row variable may
                        // have been bound (by a prior unification step or by direct
                        // construction) to a row that re-introduces a field already
                        // present in the explicit fields.  The contains_key guard
                        // ensures the explicit field always wins, matching Rémy's
                        // semantics for row-variable substitution application.
                        // See test_substitution_apply_row_var_duplicate_field.
                        let mut merged = new_fields;
                        for (key, value) in resolved.fields {
                            if !merged.contains_key(&key) {
                                merged.insert(key, value);
                            }
                        }
                        Row {
                            fields: merged,
                            tail: resolved.tail,
                        }
                    }
                    None => Row {
                        fields: new_fields,
                        tail: RowTail::RowVar(name.clone(), *level),
                    },
                }
            }
        }
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

/// Row variable occurs check: does row variable ρ occur in row r?
/// Checks both the tail (direct occurrence like ρ = {..., ...ρ}) and field types
/// (nested occurrence like ρ = {x: Record({y: Int, ...ρ})})
/// Chases TypeVar bindings through `subst` to detect transitive occurrences.
pub(super) fn row_var_occurs(var_name: &str, row: &Row, subst: &Substitution) -> bool {
    // Check field types for nested row variables
    let in_fields = row
        .fields
        .values()
        .any(|ty| row_var_occurs_in_type(var_name, ty, subst));
    // Check tail
    let in_tail = matches!(&row.tail, RowTail::RowVar(name, _) if name == var_name);
    in_fields || in_tail
}

/// Row variable occurs check in type: does ρ occur in type τ through Record nesting?
/// Chases TypeVar bindings through `subst` so that if α is bound to a type containing ρ,
/// the occurrence is detected. This mirrors Robinson's requirement that the occurs check
/// operates on substitution-applied types.
///
/// ## Call pattern analysis (Task 3: FTV/FRV caching feasibility)
///
/// This function is called exclusively through `row_var_occurs`, which is invoked:
///
///   1. In `unify_remainders` Cases 2, 3, 4 -- exactly **once** per row-variable binding,
///      checking whether the variable being bound appears in the row it would be bound to.
///      The walk is O(|unique_fields| x depth) -- unavoidable for a sound occurs check.
///
///   2. Via `row_var_occurs_pub` in `typecheck.rs` access-chain generation -- once per
///      dot-access on an open record.
///
/// The optimization proposed in TODO.md (pre-collect all free row vars once per unification
/// context, then check membership) would only help if `row_var_occurs` were called in a
/// loop over the same fields with different target variables. In the current code, Cases 2
/// and 3 each check ONE variable against ONE row (one call total). Case 4 checks TWO
/// different variables against TWO different rows -- a pre-collected FRV set cannot eliminate
/// either walk because they target different variables. There is no O(n*m) pattern to break.
///
/// **Decision**: no caching optimization is warranted at this call site. The occurs check
/// is already called the minimum number of times required for soundness. If future work
/// introduces a loop that calls `row_var_occurs` for each field in a large record (e.g., a
/// bulk row-compatibility check), revisit by collecting `FRV(row)` once before the loop via
/// `ty.collect_row_vars(&mut frv_set)` and replacing per-field tree walks with `frv_set.contains`.
pub(super) fn row_var_occurs_in_type(var_name: &str, ty: &Type, subst: &Substitution) -> bool {
    let mut visited = HashSet::new();
    row_var_occurs_in_type_impl(var_name, ty, subst, &mut visited)
}

/// Implementation of `row_var_occurs_in_type` with cycle detection.
/// Defense-in-depth: tracks visited TypeVars to prevent unbounded recursion
/// on cyclic type_map bindings (should be impossible under correct occurs-check
/// invariants, but defended against for robustness).
fn row_var_occurs_in_type_impl(
    var_name: &str,
    ty: &Type,
    subst: &Substitution,
    visited: &mut HashSet<String>,
) -> bool {
    match ty {
        Type::Record(row) => row_var_occurs(var_name, row, subst),
        Type::Function {
            params,
            ret,
            variadic: _,
        } => {
            params
                .iter()
                .any(|(_name, p_ty)| row_var_occurs_in_type_impl(var_name, p_ty, subst, visited))
                || row_var_occurs_in_type_impl(var_name, ret, subst, visited)
        }
        Type::Seq(elem) => row_var_occurs_in_type_impl(var_name, elem, subst, visited),
        Type::Union(members) => members
            .iter()
            .any(|m| row_var_occurs_in_type_impl(var_name, m, subst, visited)),
        Type::Intersection(members) => members
            .iter()
            .any(|m| row_var_occurs_in_type_impl(var_name, m, subst, visited)),
        Type::TypeVar(name, _) => {
            // Chase TypeVar binding: if α is bound to τ in subst, check τ for ρ
            // Cycle detection: if we've already visited this TypeVar, return false
            // to prevent infinite recursion on cyclic bindings (impossible under
            // correct occurs-check invariants, but defended against for robustness).
            //
            // Monotone visited set: once a TypeVar is visited, it stays visited.
            // The occurs-check result is path-independent -- if ρ does not occur in
            // the resolution of α via one path, it won't occur via any other path,
            // because subst.type_map is deterministic (each name maps to exactly
            // one type). Removing on backtrack would only cause redundant re-traversal
            // without changing the result.
            if !visited.insert(name.clone()) {
                return false;
            }
            if let Some(bound) = subst.type_map.get(name) {
                row_var_occurs_in_type_impl(var_name, bound, subst, visited)
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Check if a row variable name should be hidden from display (starts with '_')
fn is_display_hidden(name: &str) -> bool {
    name.starts_with('_')
}

/// Resolve a row by following bound row variables in the substitution
fn resolve_row<'a>(row: &'a Row, subst: &Substitution) -> Cow<'a, Row> {
    match &row.tail {
        RowTail::RowVar(name, _level) => {
            if let Some(bound) = subst.row_map.get(name) {
                // Fast-path: if the original row has no fields, the resolved row is the result.
                // No need to clone and merge -- return the resolved row directly.
                if row.fields.is_empty() {
                    let mut visited_types = HashSet::new();
                    let mut visited_rows = HashSet::new();
                    return Cow::Owned(subst.apply_row(
                        bound,
                        0,
                        &mut visited_types,
                        &mut visited_rows,
                    ));
                }

                // Apply the row to chase through the binding
                let mut visited_types = HashSet::new();
                let mut visited_rows = HashSet::new();
                let resolved = subst.apply_row(bound, 0, &mut visited_types, &mut visited_rows);
                // Merge fields: original fields take precedence.
                // Overlap can arise when ρ was bound by a different unification call
                // (e.g., {y: T, ...ρ} ~ {y: T, x: S} binds ρ -> {x: S}, then
                // resolving {x: U, ...ρ} sees x in both the explicit and bound rows).
                let mut merged = row.fields.clone();
                for (key, value) in resolved.fields {
                    if !merged.contains_key(&key) {
                        merged.insert(key, value);
                    }
                }
                Cow::Owned(Row {
                    fields: merged,
                    tail: resolved.tail,
                })
            } else {
                Cow::Borrowed(row)
            }
        }
        RowTail::Empty => Cow::Borrowed(row),
    }
}

/// Unify two row tails
fn unify_tails(
    t1: &RowTail,
    t2: &RowTail,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    match (t1, t2) {
        (RowTail::Empty, RowTail::Empty) => Ok(()),
        (RowTail::RowVar(rho1, _), RowTail::RowVar(rho2, _)) => {
            // No occurs check needed: resolve_row guarantees both unbound, so binding ρ₁→{…ρ₁} cannot occur (Robinson vacuous satisfaction)
            if rho1 == rho2 {
                Ok(())
            } else {
                // Bind rho1 to Row { fields: {}, tail: RowVar(rho2) }
                // Lower levels symmetrically
                //
                // The level asymmetry is safe: rho1 is bound to Row({}, RowVar(rho2)), eliminating it
                // from the constraint set. Only rho2 remains free, so only its level needs lowering to
                // prevent unsound generalization (Kiselyov 2013). However, we lower rho2's level to
                // min(rho1_level, rho2_level) to maintain the invariant that binding eliminates the
                // higher-level variable.
                let rho1_level = state.levels.get(rho1).copied().unwrap_or(0);
                let rho2_level = state.levels.get(rho2).copied().unwrap_or(0);
                state
                    .levels
                    .insert(rho2.clone(), rho2_level.min(rho1_level));

                subst.row_map.insert(
                    rho1.clone(),
                    Row {
                        fields: HashMap::new(),
                        tail: RowTail::RowVar(rho2.clone(), rho2_level.min(rho1_level)),
                    },
                );
                subst.check_size(span)?;
                Ok(())
            }
        }
        (RowTail::RowVar(rho, _), RowTail::Empty) | (RowTail::Empty, RowTail::RowVar(rho, _)) => {
            // Bind rho to Row { fields: {}, tail: Empty }
            subst.row_map.insert(
                rho.clone(),
                Row {
                    fields: HashMap::new(),
                    tail: RowTail::Empty,
                },
            );
            subst.check_size(span)?;
            Ok(())
        }
    }
}

/// Lower the level of all type vars and row vars appearing in a row to min(their level, max_level).
/// Called after a row-variable binding to prevent unsound generalization of inner vars.
fn lower_row_var_levels(row: &Row, max_level: u32, state: &mut InferState) {
    // Collect both type vars and row vars in a single pass over field types
    let mut type_vars = HashSet::new();
    let mut row_vars = HashSet::new();
    for ty in row.fields.values() {
        ty.collect_all_vars(&mut type_vars, &mut row_vars);
    }
    // Also collect the tail row var if present
    if let RowTail::RowVar(name, _) = &row.tail {
        row_vars.insert(name.clone());
    }
    // Lower all collected vars in a single pass
    for var in type_vars.iter().chain(&row_vars) {
        let current = state.levels.get(var).copied().unwrap_or(0);
        state.levels.insert(var.clone(), current.min(max_level));
    }
}

/// Public wrapper for `row_var_occurs` -- used in access-chain constraint generation
/// (doc/07-type-extensions.md Part 5) to check for cyclic row bindings before binding.
pub fn row_var_occurs_pub(var_name: &str, row: &Row, subst: &Substitution) -> bool {
    row_var_occurs(var_name, row, subst)
}

/// Public wrapper for `lower_row_var_levels` -- used in access-chain constraint generation
/// (doc/07-type-extensions.md Part 5) to enforce level invariants before binding a row variable.
pub fn lower_row_var_levels_pub(row: &Row, max_level: u32, state: &mut InferState) {
    lower_row_var_levels(row, max_level, state);
}

/// Case 4 of Wand (1987): both rows have unique fields and distinct RowVar tails.
///
/// Creates a fresh row variable ρ_fresh to represent the shared unknown tail, then:
///   - Binds ρ₁ → Row { fields: U₂, tail: RowVar(ρ_fresh) }
///   - Binds ρ₂ → Row { fields: U₁, tail: RowVar(ρ_fresh) }
///
/// This correctly propagates constraints: if either tail is later unified with a
/// concrete row, the binding flows through ρ_fresh to the other side.
///
/// # Soundness
///
/// Before each binding, `row_var_occurs` is called to detect would-be cyclic
/// bindings (infinite row types).  After each `row_map.insert`, `check_size` is
/// called to enforce the global substitution size limit.  Level lowering
/// (`lower_row_var_levels`) is applied to both rows before binding so that
/// inner type/row variables cannot escape their scope via the fresh tail
/// (Kiselyov 2013 §level-lowering).
fn partition_fields_and_bind(
    unique1: HashMap<String, Type>,
    rho1: &str,
    unique2: HashMap<String, Type>,
    rho2: &str,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // Allocate a fresh row variable ρ_fresh to act as the shared unknown tail
    let rho_fresh_name = format!("_t{}", state.name_counter);
    state.name_counter = state.name_counter.saturating_add(1);
    let rho_fresh_level = state.level;
    state.levels.insert(rho_fresh_name.clone(), rho_fresh_level);

    let fresh_tail = RowTail::RowVar(rho_fresh_name.clone(), rho_fresh_level);

    // Build the two rows that each RowVar will be bound to.
    // ρ₁ → Row { fields: U₂, tail: ρ_fresh }
    // ρ₂ → Row { fields: U₁, tail: ρ_fresh }
    let row2_with_fresh = Row {
        fields: unique2,
        tail: fresh_tail.clone(),
    };
    let row1_with_fresh = Row {
        fields: unique1,
        tail: RowTail::RowVar(rho_fresh_name, rho_fresh_level),
    };

    // Occurs check: ρ₁ must not appear in (U₂ ∪ {ρ_fresh})
    if row_var_occurs(rho1, &row2_with_fresh, subst) {
        let rho1_display = if is_display_hidden(rho1) {
            "an anonymous open row".to_string()
        } else {
            rho1.to_string()
        };
        return Err(TypeError::new(
            format!("infinite row type: {rho1_display} occurs in its own binding"),
            span,
        ));
    }

    // Occurs check: ρ₂ must not appear in (U₁ ∪ {ρ_fresh})
    if row_var_occurs(rho2, &row1_with_fresh, subst) {
        let rho2_display = if is_display_hidden(rho2) {
            "an anonymous open row".to_string()
        } else {
            rho2.to_string()
        };
        return Err(TypeError::new(
            format!("infinite row type: {rho2_display} occurs in its own binding"),
            span,
        ));
    }

    // Level lowering: prevent inner vars from escaping their scope through the fresh tail
    let rho1_level = state.levels.get(rho1).copied().unwrap_or(0);
    let rho2_level = state.levels.get(rho2).copied().unwrap_or(0);
    lower_row_var_levels(&row2_with_fresh, rho1_level, state);
    lower_row_var_levels(&row1_with_fresh, rho2_level, state);

    // Bind ρ₁ → Row { fields: U₂, tail: ρ_fresh }
    subst.row_map.insert(rho1.to_string(), row2_with_fresh);
    subst.check_size(span)?;
    // Bind ρ₂ → Row { fields: U₁, tail: ρ_fresh }
    subst.row_map.insert(rho2.to_string(), row1_with_fresh);
    subst.check_size(span)?;

    Ok(())
}

/// Unify remainders (unique fields + tails) -- implements Wand (1987) 4-case algorithm.
///
/// Soundness invariant: every binding case calls `row_var_occurs` BEFORE
/// `subst.row_map.insert` to prevent construction of infinite row types
/// (Robinson 1965, extended for rows per Rémy 1994).  Verified for Cases 2-4.
fn unify_remainders(
    unique1: HashMap<String, Type>,
    tail1: RowTail,
    unique2: HashMap<String, Type>,
    tail2: RowTail,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    let u1_empty = unique1.is_empty();
    let u2_empty = unique2.is_empty();

    // NOTE: Case 4 must be matched BEFORE Cases 2/3 to prevent shadowing
    match (&tail1, &tail2) {
        // Case 1: No unique fields on either side -- unify tails directly
        (_, _) if u1_empty && u2_empty => unify_tails(&tail1, &tail2, subst, state, span),

        // Case 4: Both have unique fields and both have RowVar tails -- create fresh row variable.
        // Delegates to `partition_fields_and_bind` which encapsulates the occurs checks,
        // level lowering, and dual binding logic (Wand 1987, Case 4).
        (RowTail::RowVar(rho1, _), RowTail::RowVar(rho2, _))
            if !u1_empty && !u2_empty && rho1 != rho2 =>
        {
            partition_fields_and_bind(unique1, rho1, unique2, rho2, subst, state, span)
        }

        // Case 2: Only left has unique fields -- right tail must absorb them
        // Guard: u2_empty required -- when both sides have unique fields with different RowVars, Case 4 applies; this guard ensures Case 2 only fires when unique2 is genuinely empty.
        (_, RowTail::RowVar(rho2, _)) if !u1_empty && u2_empty => {
            let row_to_bind = Row {
                fields: unique1,
                tail: tail1,
            };
            if row_var_occurs(rho2, &row_to_bind, subst) {
                let rho2_display = if is_display_hidden(rho2) {
                    "an anonymous open row".to_string()
                } else {
                    rho2.clone()
                };
                return Err(TypeError::new(
                    format!("infinite row type: {rho2_display} occurs in its own binding"),
                    span,
                ));
            }
            // Lower levels of inner vars to rho2's level before binding
            let rho2_level = state.levels.get(rho2).copied().unwrap_or(0);
            lower_row_var_levels(&row_to_bind, rho2_level, state);
            subst.row_map.insert(rho2.clone(), row_to_bind);
            subst.check_size(span)?;
            Ok(())
        }

        // Case 3: Only right has unique fields -- left tail must absorb them
        // Guard: u1_empty required -- when both sides have unique fields with different RowVars,
        // Case 4 applies; this guard ensures Case 3 only fires when unique1 is genuinely empty.
        (RowTail::RowVar(rho1, _), _) if !u2_empty && u1_empty => {
            let row_to_bind = Row {
                fields: unique2,
                tail: tail2,
            };
            if row_var_occurs(rho1, &row_to_bind, subst) {
                let rho1_display = if is_display_hidden(rho1) {
                    "an anonymous open row".to_string()
                } else {
                    rho1.clone()
                };
                return Err(TypeError::new(
                    format!("infinite row type: {rho1_display} occurs in its own binding"),
                    span,
                ));
            }
            // Lower levels of inner vars to rho1's level before binding
            let rho1_level = state.levels.get(rho1).copied().unwrap_or(0);
            lower_row_var_levels(&row_to_bind, rho1_level, state);
            subst.row_map.insert(rho1.clone(), row_to_bind);
            subst.check_size(span)?;
            Ok(())
        }

        // Error case: closed tail cannot absorb unique fields
        (_, RowTail::Empty) if !u1_empty => Err(TypeError::new(
            format!("extra fields [{}] in closed row", {
                let mut keys: Vec<_> = unique1.keys().cloned().collect();
                keys.sort();
                keys.join(", ")
            }),
            span,
        )),
        (RowTail::Empty, _) if !u2_empty => Err(TypeError::new(
            format!("extra fields [{}] in closed row", {
                let mut keys: Vec<_> = unique2.keys().cloned().collect();
                keys.sort();
                keys.join(", ")
            }),
            span,
        )),

        // Error case: same row variable with different unique fields on BOTH sides
        // This handles {x: Int, ...rho} ~ {y: Str, ...rho} which would require
        // rho to simultaneously provide both x and y, which is impossible
        (RowTail::RowVar(rho1, _), RowTail::RowVar(rho2, _))
            if rho1 == rho2 && !u1_empty && !u2_empty =>
        {
            let mut fields: Vec<_> = unique1.keys().chain(unique2.keys()).cloned().collect();
            fields.sort();
            let rho1_display = if rho1.starts_with('_') {
                "an anonymous open row".to_string()
            } else {
                rho1.clone()
            };
            Err(TypeError::new(
                format!(
                    "incompatible fields [{}] with shared row variable {}",
                    fields.join(", "),
                    rho1_display
                ),
                span,
            ))
        }

        // All 7 pattern cases are exhaustive over (u1_empty, tail1, u2_empty, tail2); this arm is dead by invariant.
        _ => unreachable!("unify_remainders: all cases should be covered"),
    }
}

/// Unify two rows using field partitioning
fn unify_rows(
    row1: &Row,
    row2: &Row,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // Step 1: Resolve bound row variables
    let resolved1 = resolve_row(row1, subst).into_owned();
    let resolved2 = resolve_row(row2, subst).into_owned();

    // Fast-path: both rows are closed and have identical key sets -- the common case
    // for checking an inferred closed record against an annotated closed record.
    // Skip all partition allocation and proceed directly to per-field unification.
    if resolved1.tail == RowTail::Empty
        && resolved2.tail == RowTail::Empty
        && resolved1.fields.len() == resolved2.fields.len()
        && resolved1
            .fields
            .keys()
            .all(|k| resolved2.fields.contains_key(k))
    {
        for (key, ty1) in &resolved1.fields {
            let ty2 = &resolved2.fields[key];
            unify(ty1, ty2, subst, state, span)?;
        }
        return Ok(());
    }

    // Step 2: Partition fields into shared and unique
    let keys1: HashSet<&String> = resolved1.fields.keys().collect();
    let keys2: HashSet<&String> = resolved2.fields.keys().collect();
    let shared: Vec<&String> = keys1.intersection(&keys2).copied().collect();

    let unique1: HashMap<String, Type> = resolved1
        .fields
        .iter()
        .filter(|(k, _)| !keys2.contains(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let unique2: HashMap<String, Type> = resolved2
        .fields
        .iter()
        .filter(|(k, _)| !keys1.contains(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Step 3: Unify shared field types
    for key in shared {
        let ty1 = &resolved1.fields[key];
        let ty2 = &resolved2.fields[key];
        unify(ty1, ty2, subst, state, span)?;
    }

    // Step 3.5: Re-resolve tails after shared-field unification
    // Step 3's recursive unify() calls may have bound row variables that appear
    // as resolved1.tail or resolved2.tail (e.g., when unifying nested Record types
    // that share a row variable with the outer row's tail). Passing stale tails to
    // Step 4 would cause unify_remainders to overwrite the Step-3 binding, violating
    // the Robinson (1965) substitution-threading invariant.
    //
    // Fast-path: both tails are already Empty -- re-resolution is a no-op (resolve_row
    // with RowTail::Empty returns the row unchanged). Skip the two resolve_row calls and
    // the Step 3.6 re-partition allocations; proceed directly to unify_remainders.
    if resolved1.tail == RowTail::Empty && resolved2.tail == RowTail::Empty {
        return unify_remainders(
            unique1,
            resolved1.tail.clone(),
            unique2,
            resolved2.tail.clone(),
            subst,
            state,
            span,
        );
    }

    let re_resolved1 = resolve_row(
        &Row {
            fields: unique1,
            tail: resolved1.tail.clone(),
        },
        subst,
    )
    .into_owned();
    let re_resolved2 = resolve_row(
        &Row {
            fields: unique2,
            tail: resolved2.tail.clone(),
        },
        subst,
    )
    .into_owned();

    // Step 3.6: Re-partition after re-resolution
    // Re-resolution may surface new fields from row variable bindings that overlap
    // with the other side's unique fields. These must be unified as shared fields
    // before passing the truly unique remainders to unify_remainders.
    let rekeys1: HashSet<&String> = re_resolved1.fields.keys().collect();
    let rekeys2: HashSet<&String> = re_resolved2.fields.keys().collect();
    let new_shared: Vec<&String> = rekeys1.intersection(&rekeys2).copied().collect();

    if !new_shared.is_empty() {
        // New shared fields surfaced by re-resolution -- unify them and re-partition.
        // Delegate to unify_rows which handles the full resolve-partition-unify-remainder
        // cycle. Terminates because each recursive entry requires Step 3 to have bound
        // at least one row variable (surfacing new_shared fields), strictly reducing the
        // number of unbound row variables. The occurs check prevents cyclic bindings.
        unify_rows(&re_resolved1, &re_resolved2, subst, state, span)
    } else {
        // Step 4: Unify remainders with re-resolved tails (no new shared fields)
        unify_remainders(
            re_resolved1.fields,
            re_resolved1.tail,
            re_resolved2.fields,
            re_resolved2.tail,
            subst,
            state,
            span,
        )
    }
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
            if let RowTail::RowVar(name, _) = &row.tail {
                let current_level = state.levels.get(name).copied().unwrap_or(0);
                state
                    .levels
                    .insert(name.clone(), current_level.min(cap_level));
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
        Type::Seq(elem) => lower_levels_check_occurs(elem, occurs_name, cap_level, state),
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
                        });
                    }
                }
            }

            // Check tail constraints
            match (&sub_row.tail, &sup_row.tail) {
                (_, RowTail::RowVar(_, _)) => {
                    // sup is open (has row var tail) -- sub can have any extra fields
                    Ok(())
                }
                (RowTail::Empty, RowTail::Empty) => {
                    // Both closed -- sub must not have extra fields
                    for key in sub_row.fields.keys() {
                        if !sup_row.fields.contains_key(key) {
                            return Err(TypeError {
                                message: format!(
                                    "record has extra field '{}': {} does not satisfy {} ({})",
                                    key, sub, sup, reason
                                ),
                                span,
                            });
                        }
                    }
                    Ok(())
                }
                (RowTail::RowVar(_, _), RowTail::Empty) => {
                    // sub is open, sup is closed -- cannot satisfy (sub may have extra fields)
                    Err(TypeError {
                        message: format!(
                            "open record cannot satisfy closed record: {} does not satisfy {} ({})",
                            sub, sup, reason
                        ),
                        span,
                    })
                }
            }
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
    // Shared visited sets avoid redundant allocation across both apply() calls.
    let mut visited_types = HashSet::new();
    let mut visited_rows = HashSet::new();
    let a = subst.apply_with_visited(a, &mut visited_types, &mut visited_rows);
    visited_types.clear();
    visited_rows.clear();
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

        (Type::Proxy, Type::Proxy) => Ok(()),

        // Never unification: Never unifies with anything (it's the bottom type)
        (Type::Never, _) | (_, Type::Never) => Ok(()),

        // Negation unification: structural (for now, basic support)
        (Type::Negation(t1), Type::Negation(t2)) => unify(t1, t2, subst, state, span),

        // Capability types: reflexive unification only
        (Type::DirCap, Type::DirCap) => Ok(()),
        (Type::NetCap, Type::NetCap) => Ok(()),
        (Type::Handle, Type::Handle) => Ok(()),

        // Record unification: delegate to row unification
        (Type::Record(row1), Type::Record(row2)) => unify_rows(row1, row2, subst, state, span),

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
