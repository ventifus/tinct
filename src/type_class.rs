//! Type class declarations, constraints, and class/instance environments.
//!
//! This module contains the type class system infrastructure including
//! `ClassDecl`, `Constraint`, `ClassEnv`, and `InstanceEnv`.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;

use crate::ast::Span;
use crate::rust_span;
use crate::types::{instantiate_at_level, unify, InferState, Kind, Label, Type};

/// Structural discharge rule for a typeclass — a general mechanism for declaring
/// that a typeclass is satisfied by a structural property of the type rather than
/// by a registered instance. This avoids hardcoding class names in the constraint solver.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum StructuralDischarge {
    /// No structural rule — use normal instance resolution (the default).
    #[default]
    None,
    /// Only closed dicts (`RowTail::Empty`) satisfy this constraint.
    /// Open dicts (`RowTail::Uniform`) do NOT satisfy it and produce a type error.
    /// Used by the `Record` typeclass to enforce closed-dict semantics.
    ClosedDict,
}

/// A single argument position in a `Constraint::Class`.
///
/// Most positions hold a type variable name that will be renamed during instantiation.
/// Determined positions (those resolved by functional-dependency improvement before
/// generalization) are stored as `Ground(Type)` so that `instantiate_scheme` and
/// `check_constraints_on_var` can use the concrete type directly without needing
/// to look it up in the substitution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConstraintArg {
    /// A type variable that will be renamed during instantiation.
    Var(String),
    /// A concrete ground type — passed through unchanged during instantiation.
    Ground(Type),
}

impl ConstraintArg {
    /// Returns the variable name if this is a `Var` position, `None` for `Ground`.
    pub fn as_var(&self) -> Option<&str> {
        match self {
            ConstraintArg::Var(s) => Some(s),
            ConstraintArg::Ground(_) => None,
        }
    }

    /// Returns the ground type if this is a `Ground` position, `None` for `Var`.
    pub fn as_ground(&self) -> Option<&Type> {
        match self {
            ConstraintArg::Var(_) => None,
            ConstraintArg::Ground(t) => Some(t),
        }
    }
}

/// Constraint on a type variable (type class membership or structural property)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Constraint {
    /// Type class constraint: `class vars` (e.g., `Numeric a` or `Add a b c`)
    ///
    /// `class`: The type class declaration (provides name, functional dependencies, resolver, etc.)
    /// `vars`: Arguments to the constraint — either type variable names or concrete ground types.
    ///   Use `ConstraintArg::Var(name)` for positions that are still polymorphic and will be
    ///   renamed during instantiation, and `ConstraintArg::Ground(ty)` for positions whose type
    ///   was determined by functional-dependency improvement before generalization.
    /// `origin_name`: Name of the function/builtin that introduced this constraint (for T013 diagnostics)
    /// `origin_span`: Span of the argument that introduced this constraint (for T013 diagnostics)
    ///
    /// Functional dependencies are accessed via `class.determines`.
    /// For `Add a b c` with FD `(a,b) → c`: `class.determines = vec![(vec![0,1], vec![2])]`
    Class {
        class: Arc<ClassDecl>,
        vars: Vec<ConstraintArg>,
        origin_name: Option<Arc<str>>,
        origin_span: Option<Span>,
    },
    /// HasField constraint: `HasField label dict_var field_var`
    /// Asserts that dict_var has a field at label with type field_var.
    /// Functional dependency: (label, dict_var) → field_var
    HasField {
        label: Label,
        dict_var: String,
        field_var: String,
    },
}

impl Constraint {
    /// Create a single-parameter Class constraint from a class name string.
    /// Constructs a minimal `ClassDecl` with just the name (no params, superclasses, or FDs).
    ///
    /// **WARNING: This method is ONLY safe for classes that have no functional dependencies.**
    ///
    /// Because `new_by_name` creates a `ClassDecl` with `determines: vec![]`, using it for
    /// FD-bearing classes causes functional dependency improvement to silently skip.
    /// For classes with functional dependencies, use `state.env.read().unwrap().get_class(class_name)` to
    /// retrieve the full `ClassDecl`, then construct `Constraint::Class { class, vars, .. }` directly.
    pub fn new_by_name(name: impl Into<String>, var: impl Into<String>) -> Self {
        let name = name.into();
        let class = Arc::new(ClassDecl {
            name,
            params: vec![],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
            structural_discharge: StructuralDischarge::None,
            method_signatures: vec![],
        });
        Self::Class {
            class,
            vars: vec![ConstraintArg::Var(var.into())],
            origin_name: None,
            origin_span: None,
        }
    }
}

impl fmt::Display for ConstraintArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConstraintArg::Var(s) => write!(f, "{}", s),
            ConstraintArg::Ground(t) => write!(f, "{}", t),
        }
    }
}

impl fmt::Display for Constraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Constraint::Class { class, vars, .. } => {
                write!(f, "{}", class.name)?;
                for arg in vars {
                    write!(f, " {}", arg)?;
                }
                Ok(())
            }
            Constraint::HasField {
                label,
                dict_var,
                field_var,
            } => write!(f, "HasField {} {} {}", label, dict_var, field_var),
        }
    }
}

/// Type class declaration (Wadler & Blott 1989)
/// Example: `[class [Equatable a] eq: [Fn@Bool [a a]]]`
#[derive(Debug, Clone)]
pub struct ClassDecl {
    /// Class name (e.g., "Equatable")
    pub name: String,
    /// Type parameters with their kinds (e.g., [("a", Kind::Type)])
    pub params: Vec<(String, Kind)>,
    /// Superclass constraints as (class_name, Vec<param_names>) tuples.
    /// Example: ("Functor", vec!["f"]) means this class extends Functor with parameter f.
    /// Updated from Vec<(String, String)> to Vec<(String, Vec<String>)> for multi-param support.
    pub superclasses: Vec<(String, Vec<String>)>,
    /// Functional dependencies: (determining_positions, determined_positions) pairs.
    /// Each pair is (Vec<usize>, Vec<usize>) indexing into `params`.
    /// Example: for Add a b c with FD (a,b) → c: determines = vec![(vec![0,1], vec![2])]
    pub(crate) determines: Vec<(Vec<usize>, Vec<usize>)>,
    /// Type-stage resolver function name — names a function in the type-stage env.
    /// When Some, the resolver is called at type-check time to compute determined types from determining types.
    pub(crate) resolver: Option<String>,
    /// Whether the resolver is injective (one-to-one mapping).
    /// If true, the type checker uses the resolver result to refine the determining types
    /// via reverse functional dependency improvement.
    ///
    /// When `resolver_injective = true` and a determined-position variable becomes ground,
    /// `improve_functional_dependency_inner` in `type_unify.rs` fires the reverse FD:
    /// it scans `InstanceEnv` for an instance whose determined-position type matches the
    /// ground determined type, extracts the corresponding determining-position types, and
    /// unifies them with the constraint's determining-position variables.
    ///
    /// Read site: `check_constraints_on_var` → `improve_functional_dependency_inner`
    /// (see `type_unify.rs`: `resolver_injective` is captured in `ApplicableConstraint::MultiParam`).
    pub(crate) resolver_injective: bool,
    /// Structural discharge rule — enables this typeclass to be satisfied by a structural
    /// property without a registered instance. See `StructuralDischarge` for variants.
    pub structural_discharge: StructuralDischarge,
    /// Method signatures declared in the class body (S-886: class method synthesis).
    /// Each entry is (method_name, method_type) where method_type uses the class's type
    /// parameters as TypeVars. E.g., for Addable: [("+", Fn(TypeVar("a"), TypeVar("b")) -> TypeVar("c"))].
    /// Used by infer_class_decl_from_surface to inject method schemes into TypeEnv.
    /// Vec instead of HashMap to avoid Hash trait requirement (Type doesn't implement Hash).
    pub method_signatures: Vec<(String, crate::type_def::Type)>,
}

impl PartialEq for ClassDecl {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for ClassDecl {}

impl std::hash::Hash for ClassDecl {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl fmt::Display for ClassDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// Type class instance declaration
/// Example: `[instance [Equatable Int] eq: [fn [x y] [= x y]]]`
#[derive(Debug, Clone)]
pub struct InstanceDecl {
    /// Class name (e.g., "Equatable")
    pub class_name: String,
    /// Instance type (e.g., Int, or type constructor application).
    /// For multi-parameter type classes, this is a Record with numbered fields:
    /// `[Add Int Float Float]` → `Record {0: Int, 1: Float, 2: Float}`.
    pub instance_type: Type,
    /// Determining positions (indices into the multi-param pattern) used to build the lookup key.
    /// Empty for single-parameter classes (no functional dependencies).
    /// Example: for `Add a b c` with FD `(a,b) → c`, this is `vec![0, 1]`.
    pub det_positions: Vec<usize>,
    /// Method implementations: method_name -> inferred type
    /// (The actual dictionary value is stored in eval::ClassDictionary)
    pub method_types: HashMap<String, Type>,
}

/// Class environment: lexically scoped registry of type class declarations.
///
/// Follows the same parent-chain scoping model as `TypeEnv`:
/// - A `BTreeMap` per scope frame with a parent pointer
/// - Insertions go into the current frame; lookups walk the chain (inner wins)
/// - Prelude classes live in the root frame — visible everywhere
/// - A class in an inner dict is visible only to that dict's descendants
///
/// `iter_classes()` returns only the current frame's entries (used by `imports.rs` seeding).
/// `get()` walks the full parent chain for lookups.
///
/// `parent` uses `Arc` so the parent frame can be shared without cloning when creating children.
/// `InferState.env` (Arc<RwLock<Env>>) is the canonical store for class declarations;
/// `ClassEnv` is used as a temporary snapshot via `state.build_class_env_snapshot()`.
///
/// Uses `BTreeMap` for deterministic iteration order across threads. HashMap iteration
/// order is randomized per-process (via `RandomState` hashing) and differs across threads
/// with separate hash seeds — using HashMap here would cause non-deterministic class
/// resolution results when `build_prelude_env()` is called from different threads
/// (e.g., the corpus-test thread vs. the main thread). BTreeMap is sorted by key
/// and produces the same iteration order on every thread.
#[derive(Debug, Clone)]
pub struct ClassEnv {
    classes: BTreeMap<String, ClassDecl>,
    parent: Option<std::sync::Arc<ClassEnv>>,
}

impl ClassEnv {
    /// Create a new root-level (no parent) ClassEnv.
    pub fn new() -> Self {
        Self {
            classes: BTreeMap::new(),
            parent: None,
        }
    }

    /// Look up a class declaration by name, walking the parent chain (inner wins).
    pub fn get(&self, name: &str) -> Option<&ClassDecl> {
        if let Some(decl) = self.classes.get(name) {
            return Some(decl);
        }
        self.parent.as_ref().and_then(|p| p.get(name))
    }

    /// Insert a class declaration into the CURRENT frame.
    pub fn insert(&mut self, class_decl: ClassDecl) {
        self.classes.insert(class_decl.name.clone(), class_decl);
    }
}

impl Default for ClassEnv {
    fn default() -> Self {
        Self::new()
    }
}

/// Instance environment: lexically scoped registry of type class instances.
///
/// Follows the same parent-chain scoping model as `ClassEnv` and `TypeEnv`:
/// - Instances are stored with a key of `(class_name, determining_type_strings)` for fast
///   exact-match lookup in the current frame.
/// - Insertions go into the current frame; lookups (via `lookup_mptc`) walk the chain.
/// - Prelude instances live in the root frame — visible everywhere.
/// - An instance in an inner dict is visible only to that dict's descendants.
///
/// **Local coherence:** Within a single scope frame, at most one instance per `(Class, Type)`
/// pair is allowed. Across scope levels, shadowing is allowed — the innermost instance wins.
/// Two `[instance [Monad Result] ...]` in the same dict is a type error; one in an outer
/// scope and one in an inner scope is valid (inner shadows).
///
/// The comment "Globally registered: coherence requires global uniqueness" no longer applies.
/// Lexically scoped with frame-local coherence enforced at insertion time.
///
/// Uses `BTreeMap` for deterministic iteration order across threads. `resolve_instance` and
/// `lookup_mptc` both iterate over instances and return the first match; with `HashMap` the
/// iteration order is randomized per-process and differs between threads (different hash seeds),
/// causing non-deterministic instance dispatch when `build_prelude_env()` runs in multiple
/// threads (e.g., the corpus-test spawn thread vs. the main thread). BTreeMap is sorted
/// by `(class_name, det_type_strings)` and produces the same iteration order on every thread,
/// making instance resolution deterministic regardless of thread scheduling.
#[derive(Debug, Clone)]
pub struct InstanceEnv {
    instances: BTreeMap<(String, Vec<String>), InstanceDecl>,
    parent: Option<std::sync::Arc<InstanceEnv>>,
}

impl InstanceEnv {
    /// Create a new root-level (no parent) InstanceEnv.
    pub fn new() -> Self {
        Self {
            instances: BTreeMap::new(),
            parent: None,
        }
    }

    /// Build the lookup key for an instance declaration.
    ///
    /// For single-parameter classes (`det_positions` is empty), the key is a one-element vec
    /// containing the string representation of `instance_type`.
    ///
    /// For MPTC classes, the key is the string representations of the types at each
    /// determining position within the encoded Record (`instance_type`).
    fn build_key(inst: &InstanceDecl) -> (String, Vec<String>) {
        let det_strings = if inst.det_positions.is_empty() {
            // Single-parameter class: use the canonical string of the full instance type
            vec![type_to_string_key(&inst.instance_type)]
        } else {
            // Multi-parameter class: extract types at determining positions from the Record
            match &inst.instance_type {
                Type::Dict(row) => inst
                    .det_positions
                    .iter()
                    .map(|&pos| {
                        type_to_string_key(
                            row.fields
                                .get(&pos.to_string())
                                .expect("determining position field must exist in instance row"),
                        )
                    })
                    .collect(),
                // Fallback: if not a Record, use the canonical string for each position
                _ => vec![type_to_string_key(&inst.instance_type)],
            }
        };
        (inst.class_name.clone(), det_strings)
    }

    /// Insert an instance.
    ///
    /// Inserts idempotently: if an instance with the same key already exists, the duplicate is
    /// silently discarded (returns `Ok(())`). This handles user code re-declaring an instance
    /// that was already seeded from the prelude cache.
    ///
    /// The key is `(class_name, determining_type_strings)` derived from `inst.det_positions`.
    /// For single-parameter classes the key is `(class_name, [instance_type_string])`.
    ///
    /// This method does NOT perform structural overlap checking (string-key dedup only).
    /// Built-in and prelude instance registration is known-disjoint by construction.
    pub fn insert(&mut self, inst: InstanceDecl) -> Result<(), String> {
        let key = Self::build_key(&inst);
        if self.instances.contains_key(&key) {
            // Exact duplicate: idempotent, no error.
            // This covers re-declarations of prelude instances in user code and corpus tests.
            return Ok(());
        }
        self.instances.insert(key, inst);
        Ok(())
    }

    /// Look up an MPTC instance by class name and the ground determining types.
    ///
    /// Uses structural unification to match instances rather than string-key lookup.
    /// This correctly handles HKT instance heads like `[Channel t]` where the type
    /// variable `t` needs to unify with concrete query types like `Int`.
    ///
    /// Returns `Some(InstanceDecl)` (owned, with freshened types) if a matching instance
    /// is found, `None` otherwise.
    ///
    /// This is the query API for MPTC functional-dependency resolution: the caller supplies the
    /// ground types at the determining positions of the class's FD, and this method returns the
    /// registered instance whose determining positions unify with the query types.
    ///
    /// Note: This method performs unification checks but does not modify the global substitution.
    /// It uses a temporary substitution for matching purposes only. Returns a cloned instance
    /// to avoid borrow checker issues when state is also needed by the caller.
    pub async fn lookup_mptc(
        &self,
        class: &str,
        determining_types: &[Type],
        state: &mut InferState,
    ) -> Option<InstanceDecl> {
        // Collect candidate instances for this class
        for ((cname, _), inst) in &self.instances {
            if cname != class {
                continue;
            }

            // F1 FIX: Save state before candidate probe to prevent leakage from failed matches.
            // unify() mutates state.type_vars and state.deferred_equalities.
            // Failed candidates must not leak these mutations.
            // per_origin_counter is intentionally NOT saved/restored (monotonically advancing).
            let saved_type_vars = state.type_vars.clone();
            let saved_deferred = state.deferred_equalities.clone();
            let saved_bounds = state.bounds.clone();

            // Freshen the ENTIRE instance_type at once so that shared type variables
            // (e.g. `K` in both `Map[K V]` and the standalone `K` position) map to the
            // same fresh name. Freshening each position independently would create
            // unrelated fresh vars, breaking determined-type resolution.
            let freshened_instance_type =
                instantiate_at_level(&inst.instance_type, state, &rust_span!());

            // Extract the determining types from the freshened instance pattern.
            // For multi-param instances, instance_type is a Record with numbered fields.
            let instance_det_types: Vec<Type> = if inst.det_positions.is_empty() {
                // Single-parameter class: the entire instance_type is the determining type
                vec![freshened_instance_type.clone()]
            } else {
                // Multi-parameter class: extract types at determining positions
                match &freshened_instance_type {
                    Type::Dict(row) => inst
                        .det_positions
                        .iter()
                        .filter_map(|&pos| row.fields.get(&pos.to_string()).cloned())
                        .collect(),
                    _ => {
                        // Malformed instance, skip — restore state first (not per_origin_counter).
                        state.type_vars = saved_type_vars.clone();
                        state.deferred_equalities = saved_deferred;
                        state.bounds = saved_bounds;
                        continue;
                    }
                }
            };

            // Check arity
            if instance_det_types.len() != determining_types.len() {
                state.type_vars = saved_type_vars.clone();
                state.deferred_equalities = saved_deferred;
                state.bounds = saved_bounds;
                continue;
            }

            // Attempt unification of all determining positions.
            // Probe directly into state.type_vars; snapshot/restore isolates the probe.
            let mut all_match = true;

            let mut probe_constraints: Vec<Constraint> = Vec::new();
            for (inst_ty, query_ty) in instance_det_types.iter().zip(determining_types.iter()) {
                if Box::pin(unify(
                    inst_ty,
                    query_ty,
                    state,
                    &mut probe_constraints,
                    rust_span!(),
                    0,
                ))
                .await
                .is_err()
                {
                    all_match = false;
                    break;
                }
            }

            if all_match {
                // Capture resolved instance type BEFORE restoring state (bindings are in state.type_vars).
                let resolved_instance_type = state.apply(&freshened_instance_type);

                // Restore state: discard probe mutations (not per_origin_counter).
                state.type_vars = saved_type_vars.clone();
                state.deferred_equalities = saved_deferred;
                state.bounds = saved_bounds;

                return Some(InstanceDecl {
                    class_name: inst.class_name.clone(),
                    instance_type: resolved_instance_type,
                    det_positions: inst.det_positions.clone(),
                    method_types: inst.method_types.clone(),
                });
            } else {
                // Restore state after failed probe (not per_origin_counter).
                state.type_vars = saved_type_vars.clone();
                state.deferred_equalities = saved_deferred;
                state.bounds = saved_bounds;
            }
        }

        // No match in current frame — walk parent chain.
        if let Some(parent) = &self.parent {
            return Box::pin(parent.lookup_mptc(class, determining_types, state)).await;
        }

        None
    }

    /// Reverse MPTC lookup: given a class name, the determined positions, and the ground
    /// determined types, find an instance whose determined-position types unify with the
    /// given types. If found, return the determining-position types extracted from that instance
    /// alongside the determining-position indices.
    ///
    /// This implements the reverse functional dependency improvement needed for injective
    /// resolvers: if we know the output of an injective resolver, we can infer the inputs.
    ///
    /// Returns `Some((determining_types, det_positions))` where `determining_types[i]` is
    /// the type at `det_positions[i]` in the matched instance.
    ///
    /// Returns `None` if no instance matches or if the class has no registered instances.
    ///
    /// Like `lookup_mptc`, this is a pure probe: all state mutations from unification
    /// attempts are discarded on failure (save/restore pattern). On success, the caller
    /// is responsible for unifying the returned determining types with the constraint vars.
    pub async fn reverse_lookup_mptc(
        &self,
        class: &str,
        ded_positions: &[usize],
        ded_types: &[Type],
        state: &mut InferState,
    ) -> Option<(Vec<Type>, Vec<usize>)> {
        for ((cname, _), inst) in &self.instances {
            if cname != class {
                continue;
            }

            // Save state before probe — restore on failure. per_origin_counter NOT saved.
            let saved_type_vars = state.type_vars.clone();
            let saved_deferred = state.deferred_equalities.clone();
            let saved_bounds = state.bounds.clone();
            // type_vars snapshot captures levels, bindings, and kinds in one clone

            // Freshen the entire instance type at once so shared type variables
            // across positions map to the same fresh names.
            let freshened_instance_type =
                instantiate_at_level(&inst.instance_type, state, &rust_span!());

            // Extract the determined-position types from the freshened instance.
            // For multi-param instances, instance_type is a Record with numbered fields.
            let instance_ded_types: Vec<Type> = match &freshened_instance_type {
                Type::Dict(row) => ded_positions
                    .iter()
                    .filter_map(|&pos| row.fields.get(&pos.to_string()).cloned())
                    .collect(),
                _ => {
                    // Single-parameter class or malformed: no determined positions to match.
                    // Restore and skip. per_origin_counter NOT restored.
                    state.type_vars = saved_type_vars.clone();
                    state.deferred_equalities = saved_deferred;
                    state.bounds = saved_bounds;
                    continue;
                }
            };

            // Arity check: must have the same number of determined types.
            if instance_ded_types.len() != ded_types.len() {
                state.type_vars = saved_type_vars.clone();
                state.deferred_equalities = saved_deferred;
                state.bounds = saved_bounds;
                continue;
            }

            // Probe: attempt to unify all determined positions with the query types.
            // Probe directly into state.type_vars; snapshot/restore isolates the probe.
            let mut all_match = true;

            let mut rl_probe_constraints: Vec<Constraint> = Vec::new();
            for (inst_ded_ty, query_ded_ty) in instance_ded_types.iter().zip(ded_types.iter()) {
                if Box::pin(unify(
                    inst_ded_ty,
                    query_ded_ty,
                    state,
                    &mut rl_probe_constraints,
                    rust_span!(),
                    0,
                ))
                .await
                .is_err()
                {
                    all_match = false;
                    break;
                }
            }

            if all_match {
                // Extract the determining-position types from the matched instance,
                // applying state (probe bindings) so that type variables in the determining
                // positions are resolved to concrete types derived from the determined-position
                // unification. Capture BEFORE restoring state.
                let det_position_indices: Vec<usize> = inst.det_positions.clone();
                let determining_types: Vec<Type> = match &freshened_instance_type {
                    Type::Dict(row) => det_position_indices
                        .iter()
                        .filter_map(|&pos| row.fields.get(&pos.to_string()).cloned())
                        .map(|ty| state.apply(&ty))
                        .collect(),
                    _ => {
                        // No determining positions to back-propagate for single-param classes.
                        // per_origin_counter NOT restored.
                        state.type_vars = saved_type_vars.clone();
                        state.deferred_equalities = saved_deferred;
                        state.bounds = saved_bounds;
                        continue;
                    }
                };

                // Restore state — the caller handles the actual unification of determining vars.
                // per_origin_counter NOT restored.
                state.type_vars = saved_type_vars.clone();
                state.deferred_equalities = saved_deferred;
                state.bounds = saved_bounds;

                return Some((determining_types, det_position_indices));
            } else {
                // Restore state after failed probe. per_origin_counter NOT restored.
                state.type_vars = saved_type_vars.clone();
                state.deferred_equalities = saved_deferred;
                state.bounds = saved_bounds;
            }
        }

        // No match in current frame — walk parent chain.
        if let Some(parent) = &self.parent {
            return Box::pin(parent.reverse_lookup_mptc(class, ded_positions, ded_types, state))
                .await;
        }

        None
    }

    /// Resolve an instance for the given class and target type using specificity-based selection.
    ///
    /// Collects ALL instances whose head types unify with the target, ranks them by specificity
    /// (fewest unresolved TypeVars in the original instance head = most specific), and returns
    /// the unique most-specific match. If two or more instances tie at the minimum specificity
    /// score, returns `Err` with an "ambiguous instances" diagnostic instead of picking
    /// arbitrarily (which would violate coherence).
    ///
    /// **Specificity score**: `count_unresolved_vars(inst.instance_type, temp_subst)` — counts
    /// TypeVars in the *original* (un-freshened) instance head that are not resolved by the
    /// unification substitution `temp_subst`. Because the substitution uses freshened names
    /// (e.g., `_t7` not `a`), original TypeVar names are never in `temp_subst`, so the score
    /// equals the number of declared type variables in the instance head. `[F Int]` → 0
    /// (most specific); `[F a]` → 1 (less specific).
    ///
    /// **Two-pass algorithm**:
    /// - Pass 1: probe each candidate with a temporary substitution, collect `(score, inst)` for
    ///   all matches, always restore state (save/restore probe pattern).
    /// - Pass 2: find minimum score, check for ties, re-run unification for the unique winner
    ///   to build the final resolved `InstanceDecl` with method types applied.
    ///
    /// Returns:
    /// - `Ok(Some(inst))` — unique most-specific match found
    /// - `Ok(None)` — no instance matches the target
    /// - `Err(msg)` — two or more equally-specific instances match (ambiguity error)
    pub async fn resolve_instance(
        &self,
        class_name: &str,
        target_type: &Type,
        state: &mut InferState,
    ) -> Result<Option<InstanceDecl>, String> {
        // Collect all instances for this class from the CURRENT FRAME only.
        // If no candidates in the current frame, delegate to parent.
        // This implements inner-wins semantics: the innermost frame with ANY instance
        // for this class takes precedence over the parent chain entirely.
        let mut candidates: Vec<&InstanceDecl> = Vec::new();

        for ((cname, _), inst) in &self.instances {
            if cname == class_name {
                candidates.push(inst);
            }
        }

        // If no candidates in the current frame, walk the parent chain.
        if candidates.is_empty() {
            if let Some(parent) = &self.parent {
                return Box::pin(parent.resolve_instance(class_name, target_type, state)).await;
            }
            return Ok(None);
        }

        // Pass 1: probe each candidate; collect (specificity_score, instance) for all that match.
        //
        // Specificity is measured on the ORIGINAL (un-freshened) instance type using temp_subst.
        // Original TypeVar names (e.g., "a") are never in temp_subst (which binds freshened names
        // like "_t7"), so every TypeVar in inst.instance_type counts as unresolved. This gives
        // the count of declared type variables in the instance head, correctly ranking
        // `[F Int]` (0) above `[F a]` (1).
        //
        // All state mutations from each probe are discarded; per_origin_counter is NOT saved/
        // restored — it advances monotonically to prevent TypeVar name reuse across candidates.
        let mut matches: Vec<(usize, &InstanceDecl)> = Vec::new();

        for inst in &candidates {
            // F1 FIX: Save state before candidate probe to prevent leakage from failed matches.
            // unify() mutates state.type_vars and state.deferred_equalities. Failed candidates
            // must not leak bindings/levels/kinds. per_origin_counter is NOT saved — it advances
            // monotonically to prevent TypeVar name reuse across probe candidates.
            let saved_type_vars = state.type_vars.clone();
            let saved_deferred = state.deferred_equalities.clone();
            let saved_bounds = state.bounds.clone();

            // Freshen the instance type to prevent variable leakage across resolution attempts.
            let freshened_instance_type =
                instantiate_at_level(&inst.instance_type, state, &rust_span!());

            // Probe directly into state.type_vars; snapshot/restore isolates the probe.
            let mut probe_constraints: Vec<Constraint> = Vec::new();
            let unify_ok = Box::pin(unify(
                &freshened_instance_type,
                target_type,
                state,
                &mut probe_constraints,
                rust_span!(),
                0,
            ))
            .await
            .is_ok();

            // Compute specificity BEFORE restoring state (bindings needed for resolution).
            let score = if unify_ok {
                Some(count_unresolved_vars(&inst.instance_type, &state.type_vars))
            } else {
                None
            };

            // Always restore state after the probe. per_origin_counter NOT restored.
            state.type_vars = saved_type_vars.clone();
            state.deferred_equalities = saved_deferred;
            state.bounds = saved_bounds;

            if let Some(score) = score {
                matches.push((score, inst));
            }
        }

        // No instance matched.
        if matches.is_empty() {
            return Ok(None);
        }

        // Find the minimum (most specific) score.
        let best_score = matches.iter().map(|(s, _)| *s).min().unwrap();

        // Collect all instances that achieve the best score.
        let winners: Vec<&InstanceDecl> = matches
            .iter()
            .filter(|(s, _)| *s == best_score)
            .map(|(_, inst)| *inst)
            .collect();

        // Ambiguity: two or more equally-specific instances match the target.
        if winners.len() > 1 {
            let names: Vec<String> = winners
                .iter()
                .map(|inst| inst.instance_type.to_string())
                .collect();
            return Err(format!(
                "ambiguous instances for class '{}' with target '{}': equally specific matches — {}",
                class_name,
                target_type,
                names.join(", "),
            ));
        }

        // Unique winner: re-run the unification to build the resolved InstanceDecl.
        // Pass 1 discarded all state mutations — we need the bindings from the winning
        // instance to apply to method types, so we unify once more.
        let winner = winners[0];

        // per_origin_counter is NOT saved/restored here — it advances monotonically.
        let saved_type_vars = state.type_vars.clone();
        let saved_deferred = state.deferred_equalities.clone();
        let saved_bounds = state.bounds.clone();

        let freshened_instance_type =
            instantiate_at_level(&winner.instance_type, state, &rust_span!());

        // This unification must succeed — we confirmed it in Pass 1.
        let mut winner_constraints: Vec<Constraint> = Vec::new();
        Box::pin(unify(
            &freshened_instance_type,
            target_type,
            state,
            &mut winner_constraints,
            rust_span!(),
            0,
        ))
        .await
        .expect("unify failed in Pass 2 after succeeding in Pass 1 — invariant violation");

        // Apply the unification bindings to method types (capture BEFORE restoring state).
        let freshened_method_types: HashMap<String, Type> = winner
            .method_types
            .iter()
            .map(|(name, ty)| {
                let freshened_ty = instantiate_at_level(ty, state, &rust_span!());
                (name.clone(), state.apply(&freshened_ty))
            })
            .collect();

        // Restore state after resolution. per_origin_counter NOT restored.
        state.type_vars = saved_type_vars.clone();
        state.deferred_equalities = saved_deferred;
        state.bounds = saved_bounds;

        Ok(Some(InstanceDecl {
            class_name: winner.class_name.clone(),
            instance_type: freshened_instance_type,
            det_positions: winner.det_positions.clone(),
            method_types: freshened_method_types,
        }))
    }
}

impl Default for InstanceEnv {
    fn default() -> Self {
        Self::new()
    }
}

/// Count TypeVars in `ty` that are not resolved (bound) in `type_vars`.
///
/// A TypeVar is "unresolved" if applying bindings to it still yields a TypeVar.
/// This measures how polymorphic an instance head remains after unification with a
/// target type: a fully concrete instance head (`[F Int]`) scores 0, while a
/// fully polymorphic head (`[F a]`) scores 1 for each free type variable.
///
/// Used by `resolve_instance` to select the most specific matching instance —
/// the one with the fewest unresolved TypeVars after unification.
fn count_unresolved_vars(
    ty: &Type,
    type_vars: &indexmap::IndexMap<String, crate::type_infer::TypeVarEntry>,
) -> usize {
    match ty {
        Type::Var(name, level) => {
            // Apply the substitution: if still a TypeVar, it is unresolved.
            match crate::types::apply_substitution(&Type::Var(name.clone(), *level), type_vars) {
                Type::Var(_, _) => 1,
                _ => 0,
            }
        }
        Type::App(f, a) => {
            count_unresolved_vars(f, type_vars) + count_unresolved_vars(a, type_vars)
        }
        Type::TyCon(_) => 0, // TyCon has no vars
        Type::Dict(row) => row
            .fields
            .values()
            .map(|field_ty| count_unresolved_vars(field_ty, type_vars))
            .sum(),
        Type::Function {
            params,
            ret,
            typed_variadics: _,
            rest: _,
            required_count: _,
        } => {
            let param_count: usize = params
                .iter()
                .map(|(_, p_ty)| count_unresolved_vars(p_ty, type_vars))
                .sum();
            param_count + count_unresolved_vars(ret, type_vars)
        }
        Type::Union(members) => members
            .iter()
            .map(|m| count_unresolved_vars(m, type_vars))
            .sum(),
        Type::Intersection(members) => members
            .iter()
            .map(|m| count_unresolved_vars(m, type_vars))
            .sum(),
        Type::Negation(inner) => count_unresolved_vars(inner, type_vars),
        Type::StageApp { fn_name: _, args } => args
            .iter()
            .map(|a| count_unresolved_vars(a, type_vars))
            .sum(),
        Type::NominalVariant {
            tycon: _,
            ctor: _,
            fields,
        } => fields
            .fields
            .values()
            .map(|field_ty| count_unresolved_vars(field_ty, type_vars))
            .sum(),
        // S-860: equirecursive-types-core — recurse into the body.
        Type::Recursive { var: _, body } => count_unresolved_vars(body, type_vars),
        // Concrete types: Int, Float, Str, Bool, Number, Unknown, Top, Error, etc.
        _ => 0,
    }
}

/// Normalize a type to a canonical string for use as an instance lookup key.
///
/// Promotes `IntLiteral` to `"Int"` and `StringLiteral` to `"String"` so that
/// literal types resolve to the same instance as their parent types.  All other
/// types use their `Display` representation unchanged (Type::Str displays as "String").
///
/// This function mirrors the normalization performed by `type_key` in `type_unify.rs`
/// for the hardcoded arithmetic instances.
pub fn type_to_string_key(ty: &Type) -> String {
    match ty {
        Type::IntLiteral(_) => "Int".to_string(),
        Type::StringLiteral(_) => "String".to_string(),
        _ => ty.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type_infer::InferState;
    use std::collections::HashMap;

    fn make_tycon_app(name: &str, elem: Type) -> Type {
        Type::App(Box::new(Type::TyCon(name.into())), Box::new(elem))
    }

    fn make_coll_a() -> Type {
        make_tycon_app("Coll", Type::Var("a".to_string(), 0))
    }

    fn make_coll_int() -> Type {
        make_tycon_app("Coll", Type::Int)
    }

    fn make_appendable_instance(instance_type: Type) -> InstanceDecl {
        InstanceDecl {
            class_name: "Appendable".to_string(),
            instance_type,
            det_positions: vec![],
            method_types: HashMap::new(),
        }
    }

    /// When both `[Coll Int]` and `[Coll a]` are registered, resolving against `Coll[Int]`
    /// must select `[Coll Int]` (score 0) over `[Coll a]` (score 1) by specificity.
    ///
    /// Both instances are inserted directly to test that `resolve_instance` handles
    /// equally-registered instances correctly.
    #[tokio::test]
    async fn test_resolve_instance_specificity_concrete_wins_over_polymorphic() {
        let mut state = InferState::new();
        let mut env = InstanceEnv::new();

        env.insert(make_appendable_instance(make_coll_a())).unwrap();
        env.insert(make_appendable_instance(make_coll_int()))
            .unwrap();

        let target = make_coll_int();
        let resolved = env
            .resolve_instance("Appendable", &target, &mut state)
            .await
            .expect("should not be ambiguous — concrete instance is strictly more specific");

        assert!(
            resolved.is_some(),
            "should find a matching instance for Coll[Int]"
        );
        let resolved = resolved.unwrap();

        assert!(
            !resolved.instance_type.has_inference_vars(),
            "resolved instance should be concrete (no TypeVars), got: {}",
            resolved.instance_type
        );
        if let Type::App(_, elem) = &resolved.instance_type {
            assert!(
                matches!(elem.as_ref(), Type::Int),
                "element of resolved type should be Int, got: {}",
                elem
            );
        } else {
            panic!(
                "resolved instance type should be Coll[Int], got: {}",
                resolved.instance_type
            );
        }
    }

    /// When `[Coll a]` and `[Coll b]` are both registered (equally polymorphic), resolving
    /// against `Coll[Int]` must report ambiguity — both score 1, so neither wins.
    #[tokio::test]
    async fn test_resolve_instance_ambiguity_equally_specific() {
        let mut state = InferState::new();
        let mut env = InstanceEnv::new();

        let coll_a_inst =
            make_appendable_instance(make_tycon_app("Coll", Type::Var("a".to_string(), 0)));
        let coll_b_inst =
            make_appendable_instance(make_tycon_app("Coll", Type::Var("b".to_string(), 0)));

        env.insert(coll_a_inst).unwrap();
        env.insert(coll_b_inst).unwrap();

        let target = make_coll_int();
        let result = env
            .resolve_instance("Appendable", &target, &mut state)
            .await;

        assert!(
            result.is_err(),
            "two equally-specific instances should yield an ambiguity error"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("ambiguous instances"),
            "error message should mention ambiguous instances, got: {msg}"
        );
        assert!(
            msg.contains("Appendable"),
            "error message should mention the class name, got: {msg}"
        );
    }

    /// When only `[Coll a]` is registered, it resolves for `Coll[Int]` without ambiguity —
    /// single match always wins regardless of polymorphism score.
    #[tokio::test]
    async fn test_resolve_instance_single_match_no_ambiguity() {
        let mut state = InferState::new();
        let mut env = InstanceEnv::new();

        // Only the polymorphic instance — no competition.
        env.insert(make_appendable_instance(make_coll_a())).unwrap();

        let target = make_coll_int();
        let resolved = env
            .resolve_instance("Appendable", &target, &mut state)
            .await
            .expect("single match should not be ambiguous");

        assert!(
            resolved.is_some(),
            "should find a matching instance for Coll[Int] via [Coll a]"
        );
    }

    /// Resolving against a target that matches no instance returns Ok(None).
    #[tokio::test]
    async fn test_resolve_instance_no_match_returns_none() {
        let mut state = InferState::new();
        let mut env = InstanceEnv::new();

        env.insert(make_appendable_instance(make_coll_int()))
            .unwrap();

        // Coll[Str] does not match Coll[Int].
        let target = make_tycon_app("Coll", Type::Str);
        let resolved = env
            .resolve_instance("Appendable", &target, &mut state)
            .await
            .expect("no match should not yield an error");

        assert!(
            resolved.is_none(),
            "Coll[Str] should not match Coll[Int] instance"
        );
    }
}
