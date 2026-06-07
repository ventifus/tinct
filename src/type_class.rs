//! Type class declarations, constraints, and class/instance environments.
//!
//! This module contains the type class system infrastructure including
//! `ClassDecl`, `Constraint`, `ClassEnv`, and `InstanceEnv`.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::ast::Span;
use crate::types::{instantiate_at_level, unify, InferState, Kind, Label, Type};

/// Constraint on a type variable (type class membership or structural property)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Constraint {
    /// Type class constraint: `class vars` (e.g., `Numeric a` or `Add a b c`)
    ///
    /// `class`: The type class declaration (provides name, functional dependencies, resolver, etc.)
    /// `vars`: Type variable names in the constraint (e.g., ["a"] for single-param, ["a", "b", "c"] for MPTC)
    /// `origin_name`: Name of the function/builtin that introduced this constraint (for T013 diagnostics)
    /// `origin_span`: Span of the argument that introduced this constraint (for T013 diagnostics)
    ///
    /// Functional dependencies are accessed via `class.determines`.
    /// For `Add a b c` with FD `(a,b) → c`: `class.determines = vec![(vec![0,1], vec![2])]`
    Class {
        class: Arc<ClassDecl>,
        vars: Vec<String>,
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
    /// Create a single-parameter Class constraint (backward compatibility helper)
    pub fn new(class: Arc<ClassDecl>, var: impl Into<String>) -> Self {
        Self::Class {
            class,
            vars: vec![var.into()],
            origin_name: None,
            origin_span: None,
        }
    }

    /// Create a single-parameter Class constraint from a class name string.
    /// Constructs a minimal `ClassDecl` with just the name (no params, superclasses, or FDs).
    ///
    /// **WARNING: This method is ONLY safe for non-FD classes.**
    ///
    /// Used in built-in environment construction where the full `ClassDecl` is not yet available.
    /// Because `new_by_name` creates a `ClassDecl` with `determines: vec![]`, using it for
    /// FD-bearing classes (Addable, Subtractable, Multipliable, Divisible, Indexable, Concatable)
    /// causes functional dependency improvement to silently skip: `improve_functional_dependency`
    /// finds no FDs in the minimal ClassDecl and cannot resolve determined types from determining types.
    ///
    /// For FD-bearing classes, use `state.class_env.get(class_name)` to retrieve the full `ClassDecl`
    /// with functional dependencies, then construct `Constraint::Class { class, vars, .. }` directly.
    ///
    /// Safe for non-FD classes: Equatable, Comparable, Showable, Mappable, Appendable.
    ///
    /// **Audit findings (B-315):**
    /// - `src/builtins_core.rs:1402` — safe (Showable)
    /// - `src/type_env.rs:2067` — safe (Showable, test code)
    pub fn new_by_name(name: impl Into<String>, var: impl Into<String>) -> Self {
        let name = name.into();
        debug_assert!(
            !matches!(
                name.as_str(),
                "Addable" | "Subtractable" | "Multipliable" | "Divisible" | "Indexable" | "Concatable"
            ),
            "Constraint::new_by_name used for FD-bearing class '{}' — use state.class_env lookup instead",
            name
        );
        let class = Arc::new(ClassDecl {
            name,
            params: vec![],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });
        Self::Class {
            class,
            vars: vec![var.into()],
            origin_name: None,
            origin_span: None,
        }
    }
}

impl fmt::Display for Constraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Constraint::Class { class, vars, .. } => {
                write!(f, "{}", class.name)?;
                for var in vars {
                    write!(f, " {}", var)?;
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// Type-stage resolver function name (e.g., "AddResult" for Add class).
    /// When Some, the resolver is called at type-check time to compute determined types from determining types.
    pub(crate) resolver: Option<String>,
    /// Whether the resolver is injective (one-to-one mapping).
    /// If true, the type checker uses the resolver result to refine the determining types
    /// via reverse functional dependency improvement (T-913).
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
/// - A `HashMap` per scope frame with a parent pointer
/// - Insertions go into the current frame; lookups walk the chain (inner wins)
/// - Prelude classes live in the root frame — visible everywhere
/// - A class in an inner dict is visible only to that dict's descendants
///
/// `iter_classes()` returns only the current frame's entries (used by `imports.rs` seeding).
/// `get()` walks the full parent chain for lookups.
///
/// `parent` uses `Arc` so the parent frame can be shared without cloning when creating children.
/// `InferState` holds a plain `ClassEnv` (not Arc); on dict entry/exit, `mem::take` and
/// parent-chain restore are used (see `typecheck_dict.rs`).
#[derive(Debug, Clone)]
pub struct ClassEnv {
    classes: HashMap<String, ClassDecl>,
    parent: Option<std::sync::Arc<ClassEnv>>,
}

impl ClassEnv {
    /// Create a new root-level (no parent) ClassEnv.
    pub fn new() -> Self {
        Self {
            classes: HashMap::new(),
            parent: None,
        }
    }

    /// Create a child ClassEnv frame whose parent is `parent`.
    ///
    /// Classes declared in the child frame are local and do not affect the parent.
    /// Lookups walk child → parent chain with inner-wins semantics.
    pub fn child(parent: std::sync::Arc<ClassEnv>) -> Self {
        Self {
            classes: HashMap::new(),
            parent: Some(parent),
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

    /// Insert a class declaration only if no class with that name is already visible
    /// (checks the full parent chain before inserting into the current frame).
    /// Used when seeding from the prelude cache to avoid overwriting user-defined classes.
    pub fn insert_if_absent(&mut self, class_decl: ClassDecl) {
        if self.get(&class_decl.name).is_none() {
            self.classes.insert(class_decl.name.clone(), class_decl);
        }
    }

    /// Iterate over class declarations in the CURRENT frame only (no parent traversal).
    ///
    /// Used by `imports.rs` seeding, which only needs to enumerate locally-introduced classes.
    pub fn iter_classes(&self) -> impl Iterator<Item = &ClassDecl> {
        self.classes.values()
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
#[derive(Debug, Clone)]
pub struct InstanceEnv {
    instances: HashMap<(String, Vec<String>), InstanceDecl>,
    parent: Option<std::sync::Arc<InstanceEnv>>,
}

impl InstanceEnv {
    /// Create a new root-level (no parent) InstanceEnv.
    pub fn new() -> Self {
        Self {
            instances: HashMap::new(),
            parent: None,
        }
    }

    /// Create a child InstanceEnv frame whose parent is `parent`.
    ///
    /// Instances declared in the child frame are local and do not affect the parent.
    /// Lookups walk child → parent chain with inner-wins semantics.
    pub fn child(parent: std::sync::Arc<InstanceEnv>) -> Self {
        Self {
            instances: HashMap::new(),
            parent: Some(parent),
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
                Type::Record(row) => inst
                    .det_positions
                    .iter()
                    .map(|&pos| {
                        row.fields
                            .get(&pos.to_string())
                            .map(type_to_string_key)
                            .unwrap_or_default()
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
    /// For overlap detection at user-facing definition time, callers should call
    /// `check_structural_overlap` before inserting (see `typecheck.rs` instance registration).
    /// Built-in and prelude instance registration skips overlap checking intentionally —
    /// the built-in instances are known-disjoint and overlap detection at prelude-load time
    /// would require a live InferState.
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

    /// Check whether a candidate instance structurally overlaps any already-registered instance
    /// for the same class.
    ///
    /// Two instances overlap when their head types can be unified — i.e., there exists a ground
    /// type that satisfies both patterns. For example, `[Seq a]` and `[Seq Int]` overlap because
    /// substituting `a = Int` satisfies both. Overlapping instances cause non-deterministic
    /// resolution: whichever instance is found first "wins", violating coherence.
    ///
    /// This is a pure probe: all state mutations from the unification attempt are discarded
    /// (same save/restore pattern as F1 fix in `lookup_mptc`). The check uses freshened copies
    /// of both the candidate and existing instance types so that shared type variable names
    /// do not accidentally unify across distinct instances.
    ///
    /// Returns `Ok(())` if no overlap is detected, or `Err(message)` identifying the overlapping
    /// pair. The caller is responsible for converting the message into a `TypeError`.
    ///
    /// This method takes `&self` (read-only) and `&mut InferState` (for freshening and probing).
    /// Callers that do not have a live `InferState` (built-in registration, prelude seeding)
    /// should skip this check — those instances are known-disjoint by construction.
    pub fn check_structural_overlap(
        &self,
        candidate: &InstanceDecl,
        state: &mut InferState,
    ) -> Result<(), String> {
        // Walk the parent chain to check for overlap across all frames (T-1031).
        // A user instance can overlap with a prelude instance in the parent frame,
        // which should be detected and reported.
        let mut current_env: Option<&InstanceEnv> = Some(self);
        while let Some(env) = current_env {
            for ((cname, _), existing) in &env.instances {
                if cname != &candidate.class_name {
                    continue;
                }

                // Save ALL fields BEFORE freshening — instantiate_at_level advances the
                // name_counter (now in state.subst) and extends state.levels with fresh type
                // variable entries. Saving before the call means both the freshening allocations
                // and the unification probe are fully rolled back, making this check completely
                // side-effect-free (mirrors patterns_overlap in typecheck.rs).
                // NOTE: name_counter is part of state.subst, so saved_subst captures it.
                let saved_levels = state.levels.clone();
                let saved_constraints = state.constraints.clone();
                let saved_kind_env = state.kind_env.clone();
                let saved_deferred = state.deferred_equalities.clone();
                let saved_subst = state.subst.clone();

                // Freshen both instance types independently so that a type variable named
                // `a` in `[Seq a]` and a type variable named `a` in another instance map
                // to distinct fresh variables and do not accidentally unify.
                let fresh_existing = instantiate_at_level(&existing.instance_type, state);
                let fresh_candidate = instantiate_at_level(&candidate.instance_type, state);

                let mut temp_subst = state.subst.clone();
                let overlaps = unify(
                    &fresh_existing,
                    &fresh_candidate,
                    &mut temp_subst,
                    state,
                    Span::origin(),
                )
                .is_ok();

                // Always restore state — this is a pure probe.
                // Restoring state.subst also restores name_counter (it lives in the Substitution).
                state.levels = saved_levels;
                state.constraints = saved_constraints;
                state.kind_env = saved_kind_env;
                state.deferred_equalities = saved_deferred;
                state.subst = saved_subst;

                if overlaps {
                    return Err(format!(
                        "overlapping instances for class '{}': new instance '{}' overlaps with existing instance '{}'",
                        candidate.class_name,
                        candidate.instance_type,
                        existing.instance_type,
                    ));
                }
            }
            current_env = env.parent.as_deref();
        }
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
    pub fn lookup_mptc(
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
            // unify() mutates state.levels, state.constraints, state.kind_env, state.deferred_equalities,
            // and state.subst (which now includes name_counter via instantiate_at_level).
            // Failed candidates must not leak these mutations.
            // NOTE: name_counter is part of state.subst, so saved_subst captures it.
            let saved_levels = state.levels.clone();
            let saved_constraints = state.constraints.clone();
            let saved_kind_env = state.kind_env.clone();
            let saved_deferred = state.deferred_equalities.clone();
            let saved_subst = state.subst.clone();

            // Freshen the ENTIRE instance_type at once so that shared type variables
            // (e.g. `K` in both `Map[K V]` and the standalone `K` position) map to the
            // same fresh name. Freshening each position independently would create
            // unrelated fresh vars, breaking determined-type resolution.
            let freshened_instance_type = instantiate_at_level(&inst.instance_type, state);

            // Extract the determining types from the freshened instance pattern.
            // For multi-param instances, instance_type is a Record with numbered fields.
            let instance_det_types: Vec<Type> = if inst.det_positions.is_empty() {
                // Single-parameter class: the entire instance_type is the determining type
                vec![freshened_instance_type.clone()]
            } else {
                // Multi-parameter class: extract types at determining positions
                match &freshened_instance_type {
                    Type::Record(row) => inst
                        .det_positions
                        .iter()
                        .filter_map(|&pos| row.fields.get(&pos.to_string()).cloned())
                        .collect(),
                    _ => {
                        // Malformed instance, skip — restore state first
                        // Restoring state.subst also restores name_counter.
                        state.levels = saved_levels;
                        state.constraints = saved_constraints;
                        state.kind_env = saved_kind_env;
                        state.deferred_equalities = saved_deferred;
                        state.subst = saved_subst;
                        continue;
                    }
                }
            };

            // Check arity
            if instance_det_types.len() != determining_types.len() {
                // Restoring state.subst also restores name_counter.
                state.levels = saved_levels;
                state.constraints = saved_constraints;
                state.kind_env = saved_kind_env;
                state.deferred_equalities = saved_deferred;
                state.subst = saved_subst;
                continue;
            }

            // Attempt unification of all determining positions.
            // Use a temporary substitution to avoid polluting the global state.
            let mut temp_subst = state.subst.clone();
            let mut all_match = true;

            for (inst_ty, query_ty) in instance_det_types.iter().zip(determining_types.iter()) {
                if unify(inst_ty, query_ty, &mut temp_subst, state, Span::origin()).is_err() {
                    all_match = false;
                    break;
                }
            }

            if all_match {
                // F1 FIX: On successful match, restore state but keep the successful temp_subst results.
                // The state mutations from this probe ARE valid, but we don't commit temp_subst to
                // state.subst — the caller will handle substitution propagation.
                // Restoring state.subst also restores name_counter.
                state.levels = saved_levels;
                state.constraints = saved_constraints;
                state.kind_env = saved_kind_env;
                state.deferred_equalities = saved_deferred;
                state.subst = saved_subst;

                // Return the freshened instance_type with temp_subst applied so that
                // determined positions resolve to concrete types (not raw instance TypeVars).
                let resolved_instance_type = temp_subst.apply(&freshened_instance_type);
                return Some(InstanceDecl {
                    class_name: inst.class_name.clone(),
                    instance_type: resolved_instance_type,
                    det_positions: inst.det_positions.clone(),
                    method_types: inst.method_types.clone(),
                });
            } else {
                // F1 FIX: Restore state after failed probe (discard leaked mutations).
                // Restoring state.subst also restores name_counter.
                state.levels = saved_levels;
                state.constraints = saved_constraints;
                state.kind_env = saved_kind_env;
                state.deferred_equalities = saved_deferred;
                state.subst = saved_subst;
            }
        }

        // No match in current frame — walk parent chain.
        if let Some(parent) = &self.parent {
            return parent.lookup_mptc(class, determining_types, state);
        }

        None
    }

    /// Reverse MPTC lookup: given a class name, the determined positions, and the ground
    /// determined types, find an instance whose determined-position types unify with the
    /// given types. If found, return the determining-position types extracted from that instance
    /// alongside the determining-position indices.
    ///
    /// This implements the reverse functional dependency improvement needed for injective
    /// resolvers (T-913): if we know the output of an injective resolver, we can infer the inputs.
    ///
    /// Returns `Some((determining_types, det_positions))` where `determining_types[i]` is
    /// the type at `det_positions[i]` in the matched instance.
    ///
    /// Returns `None` if no instance matches or if the class has no registered instances.
    ///
    /// Like `lookup_mptc`, this is a pure probe: all state mutations from unification
    /// attempts are discarded on failure (save/restore pattern). On success, the caller
    /// is responsible for unifying the returned determining types with the constraint vars.
    pub fn reverse_lookup_mptc(
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

            // Save state before probe — restore on failure.
            let saved_levels = state.levels.clone();
            let saved_constraints = state.constraints.clone();
            let saved_kind_env = state.kind_env.clone();
            let saved_deferred = state.deferred_equalities.clone();
            let saved_subst = state.subst.clone();

            // Freshen the entire instance type at once so shared type variables
            // across positions map to the same fresh names.
            let freshened_instance_type = instantiate_at_level(&inst.instance_type, state);

            // Extract the determined-position types from the freshened instance.
            // For multi-param instances, instance_type is a Record with numbered fields.
            let instance_ded_types: Vec<Type> = match &freshened_instance_type {
                Type::Record(row) => ded_positions
                    .iter()
                    .filter_map(|&pos| row.fields.get(&pos.to_string()).cloned())
                    .collect(),
                _ => {
                    // Single-parameter class or malformed: no determined positions to match.
                    // Restore and skip.
                    state.levels = saved_levels;
                    state.constraints = saved_constraints;
                    state.kind_env = saved_kind_env;
                    state.deferred_equalities = saved_deferred;
                    state.subst = saved_subst;
                    continue;
                }
            };

            // Arity check: must have the same number of determined types.
            if instance_ded_types.len() != ded_types.len() {
                state.levels = saved_levels;
                state.constraints = saved_constraints;
                state.kind_env = saved_kind_env;
                state.deferred_equalities = saved_deferred;
                state.subst = saved_subst;
                continue;
            }

            // Probe: attempt to unify all determined positions with the query types.
            let mut temp_subst = state.subst.clone();
            let mut all_match = true;

            for (inst_ded_ty, query_ded_ty) in instance_ded_types.iter().zip(ded_types.iter()) {
                if unify(
                    inst_ded_ty,
                    query_ded_ty,
                    &mut temp_subst,
                    state,
                    Span::origin(),
                )
                .is_err()
                {
                    all_match = false;
                    break;
                }
            }

            if all_match {
                // Extract the determining-position types from the matched instance,
                // applying temp_subst so that type variables in the determining positions
                // are resolved to concrete types derived from the determined-position unification.
                //
                // Example: instance `Seq a b c` with det_positions=[0,1] and ded_positions=[2].
                // If temp_subst binds `a_fresh → Int`, the determining type at pos 0 is `Int`.
                let det_position_indices: Vec<usize> = inst.det_positions.clone();
                let determining_types: Vec<Type> = match &freshened_instance_type {
                    Type::Record(row) => det_position_indices
                        .iter()
                        .filter_map(|&pos| row.fields.get(&pos.to_string()).cloned())
                        .map(|ty| temp_subst.apply(&ty))
                        .collect(),
                    _ => {
                        // No determining positions to back-propagate for single-param classes.
                        state.levels = saved_levels;
                        state.constraints = saved_constraints;
                        state.kind_env = saved_kind_env;
                        state.deferred_equalities = saved_deferred;
                        state.subst = saved_subst;
                        continue;
                    }
                };

                // Restore state — the caller handles the actual unification of determining vars.
                state.levels = saved_levels;
                state.constraints = saved_constraints;
                state.kind_env = saved_kind_env;
                state.deferred_equalities = saved_deferred;
                state.subst = saved_subst;

                return Some((determining_types, det_position_indices));
            } else {
                // Restore state after failed probe.
                state.levels = saved_levels;
                state.constraints = saved_constraints;
                state.kind_env = saved_kind_env;
                state.deferred_equalities = saved_deferred;
                state.subst = saved_subst;
            }
        }

        // No match in current frame — walk parent chain.
        if let Some(parent) = &self.parent {
            return parent.reverse_lookup_mptc(class, ded_positions, ded_types, state);
        }

        None
    }

    /// Iterate over all locally registered instance declarations (does not traverse parent chain).
    pub fn iter_instances(&self) -> impl Iterator<Item = &InstanceDecl> {
        self.instances.values()
    }

    /// Return the number of registered instances.
    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }

    /// Look up an instance by class name and TyCon name, walking the parent chain.
    ///
    /// This is a fast, InferState-free lookup for the common case where the instance head is
    /// a bare `TyCon` (e.g., `[instance [Monad Result] ...]` → `instance_type = TyCon("Result")`).
    /// Returns the innermost matching instance (most local scope wins).
    ///
    /// Available for `[do ...]` desugaring to resolve monad instances without a live
    /// `InferState`, unlike the full unification-based `lookup_mptc`.
    /// Currently has no external callers — provided for future use by the do-inference path.
    ///
    /// Note: only matches instances whose `instance_type` is a bare `Type::TyCon(n)` —
    /// parameterized instances (e.g., `Seq[T]`) are not matched by this method.
    pub fn lookup_scoped(&self, class_name: &str, tycon_name: &str) -> Option<&InstanceDecl> {
        // Check local frame first — inner scope wins.
        for ((cname, _), inst) in &self.instances {
            if cname == class_name {
                if let Type::TyCon(n) = &inst.instance_type {
                    if n == tycon_name {
                        return Some(inst);
                    }
                }
            }
        }
        // Walk parent chain if not found in this frame.
        self.parent
            .as_ref()
            .and_then(|p| p.lookup_scoped(class_name, tycon_name))
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
    /// equals the number of declared type variables in the instance head. `[Seq Int]` → 0
    /// (most specific); `[Seq a]` → 1 (less specific).
    ///
    /// **Two-pass algorithm**:
    /// - Pass 1: probe each candidate with a temporary substitution, collect `(score, inst)` for
    ///   all matches, always restore state (save/restore pattern identical to `check_structural_overlap`).
    /// - Pass 2: find minimum score, check for ties, re-run unification for the unique winner
    ///   to build the final resolved `InstanceDecl` with method types applied.
    ///
    /// Returns:
    /// - `Ok(Some(inst))` — unique most-specific match found
    /// - `Ok(None)` — no instance matches the target
    /// - `Err(msg)` — two or more equally-specific instances match (ambiguity error)
    pub fn resolve_instance(
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
                return parent.resolve_instance(class_name, target_type, state);
            }
            return Ok(None);
        }

        // Pass 1: probe each candidate; collect (specificity_score, instance) for all that match.
        //
        // Specificity is measured on the ORIGINAL (un-freshened) instance type using temp_subst.
        // Original TypeVar names (e.g., "a") are never in temp_subst (which binds freshened names
        // like "_t7"), so every TypeVar in inst.instance_type counts as unresolved. This gives
        // the count of declared type variables in the instance head, correctly ranking
        // `[Seq Int]` (0) above `[Seq a]` (1).
        //
        // All state mutations from each probe are discarded; only the peak name_counter is kept
        // to prevent _tN name reuse across candidates (F1 fix).
        let mut matches: Vec<(usize, &InstanceDecl)> = Vec::new();

        for inst in &candidates {
            // F1 FIX: Save state before candidate probe to prevent leakage from failed matches.
            // unify() mutates state.levels, state.constraints, state.kind_env, state.deferred_equalities,
            // and state.subst.name_counter (via instantiate_at_level). Failed candidates must not leak
            // levels/constraints/kind_env/deferred, but the name_counter must be preserved at its peak
            // value (not rolled back) to prevent _tN name reuse across candidates.
            //
            // B-325: Also save state.subst because FD-improvement bindings during the probe must not
            // survive if the probe ultimately fails (e.g., unify succeeds but FD constraints fail).
            let saved_levels = state.levels.clone();
            let saved_constraints = state.constraints.clone();
            let saved_kind_env = state.kind_env.clone();
            let saved_deferred = state.deferred_equalities.clone();
            let saved_subst = state.subst.clone();
            let saved_counter = state.subst.name_counter.get();

            // Freshen the instance type to prevent variable leakage across resolution attempts.
            let freshened_instance_type = instantiate_at_level(&inst.instance_type, state);

            // Use a temporary substitution so the global state is not polluted.
            let mut temp_subst = state.subst.clone();

            let unify_ok = unify(
                &freshened_instance_type,
                target_type,
                &mut temp_subst,
                state,
                Span::origin(),
            )
            .is_ok();

            // Always restore state after the probe; preserve peak name_counter (F1 fix).
            let peak_counter = state.subst.name_counter.get();
            state.levels = saved_levels;
            state.constraints = saved_constraints;
            state.kind_env = saved_kind_env;
            state.deferred_equalities = saved_deferred;
            state.subst = saved_subst;
            state
                .subst
                .name_counter
                .set(saved_counter.max(peak_counter));

            if unify_ok {
                // Compute specificity from the ORIGINAL instance type using temp_subst.
                // Original TypeVar names are not in temp_subst (freshened names are), so
                // every TypeVar in inst.instance_type is counted as unresolved — giving
                // the number of declared type variables in the instance head.
                let score = count_unresolved_vars(&inst.instance_type, &temp_subst);
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
        // Pass 1 discarded all state mutations — we need the temp_subst from the winning
        // instance to apply to method types, so we unify once more.
        let winner = winners[0];

        let saved_levels = state.levels.clone();
        let saved_constraints = state.constraints.clone();
        let saved_kind_env = state.kind_env.clone();
        let saved_deferred = state.deferred_equalities.clone();
        let saved_subst = state.subst.clone();
        let saved_counter = state.subst.name_counter.get();

        let freshened_instance_type = instantiate_at_level(&winner.instance_type, state);
        let mut temp_subst = state.subst.clone();

        // This unification must succeed — we confirmed it in Pass 1.
        let _ = unify(
            &freshened_instance_type,
            target_type,
            &mut temp_subst,
            state,
            Span::origin(),
        );

        // Apply the unification substitution to method types.
        let freshened_method_types: HashMap<String, Type> = winner
            .method_types
            .iter()
            .map(|(name, ty)| {
                let freshened_ty = instantiate_at_level(ty, state);
                (name.clone(), temp_subst.apply(&freshened_ty))
            })
            .collect();

        // Restore state after resolution; preserve peak name_counter (F1 fix).
        let peak_counter = state.subst.name_counter.get();
        state.levels = saved_levels;
        state.constraints = saved_constraints;
        state.kind_env = saved_kind_env;
        state.deferred_equalities = saved_deferred;
        state.subst = saved_subst;
        state
            .subst
            .name_counter
            .set(saved_counter.max(peak_counter));

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

/// Count TypeVars in `ty` that are not resolved (bound) in `subst`.
///
/// A TypeVar is "unresolved" if applying `subst` to it still yields a TypeVar.
/// This measures how polymorphic an instance head remains after unification with a
/// target type: a fully concrete instance head (`[Seq Int]`) scores 0, while a
/// fully polymorphic head (`[Seq a]`) scores 1 for each free type variable.
///
/// Used by `resolve_instance` to select the most specific matching instance —
/// the one with the fewest unresolved TypeVars after unification.
///
/// Note: The `subst` argument is only meaningful when scoring the freshened instance
/// head after unification (Pass 1, line 844). When scoring the ORIGINAL instance head
/// (as is done in resolve_instance), `subst` contains only bindings for freshened
/// variable names like `_t7`, while the original head contains universally quantified
/// names like `a`. Because `a` is not bound in `subst`, applying `subst` returns a
/// TypeVar unchanged, so every original-head TypeVar is counted — giving the total
/// number of declared type parameters in the instance, which is the correct specificity
/// measure (e.g., `[Seq a]` scores 1, `[Seq Int]` scores 0).
fn count_unresolved_vars(ty: &Type, subst: &crate::types::Substitution) -> usize {
    match ty {
        Type::TypeVar(name, level) => {
            // Apply the substitution: if still a TypeVar, it is unresolved.
            match subst.apply(&Type::TypeVar(name.clone(), *level)) {
                Type::TypeVar(_, _) => 1,
                _ => 0,
            }
        }
        Type::App(f, a) => count_unresolved_vars(f, subst) + count_unresolved_vars(a, subst),
        Type::TyCon(_) => 0, // TyCon has no vars
        Type::Record(row) => row
            .fields
            .values()
            .map(|field_ty| count_unresolved_vars(field_ty, subst))
            .sum(),
        Type::Function {
            params,
            ret,
            variadic: _,
        } => {
            let param_count: usize = params
                .iter()
                .map(|(_, p_ty)| count_unresolved_vars(p_ty, subst))
                .sum();
            param_count + count_unresolved_vars(ret, subst)
        }
        Type::Union(members) => members
            .iter()
            .map(|m| count_unresolved_vars(m, subst))
            .sum(),
        Type::Intersection(members) => members
            .iter()
            .map(|m| count_unresolved_vars(m, subst))
            .sum(),
        Type::Negation(inner) => count_unresolved_vars(inner, subst),
        Type::TypeStageApp { fn_name: _, args } => {
            args.iter().map(|a| count_unresolved_vars(a, subst)).sum()
        }
        Type::NominalVariant { tag: _, fields } => fields
            .fields
            .values()
            .map(|field_ty| count_unresolved_vars(field_ty, subst))
            .sum(),
        // S-860: equirecursive-types-core — recurse into the body.
        Type::Recursive { var: _, body } => count_unresolved_vars(body, subst),
        // Concrete types: Int, Float, Str, Bool, Number, Unknown, Top, Error, etc.
        _ => 0,
    }
}

/// Normalize a type to a canonical string for use as an instance lookup key.
///
/// Promotes `IntLiteral` to `"Int"` and `StringLiteral` to `"Str"` so that
/// literal types resolve to the same instance as their parent types.  All other
/// types use their `Display` representation unchanged.
///
/// This function mirrors the normalization performed by `type_key` in `type_unify.rs`
/// for the hardcoded arithmetic instances.
pub fn type_to_string_key(ty: &Type) -> String {
    match ty {
        Type::IntLiteral(_) => "Int".to_string(),
        Type::StringLiteral(_) => "Str".to_string(),
        _ => ty.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type_infer::InferState;
    use std::collections::HashMap;

    fn make_seq_a() -> Type {
        Type::seq(Type::TypeVar("a".to_string(), 0))
    }

    fn make_seq_int() -> Type {
        Type::seq(Type::Int)
    }

    fn make_appendable_instance(instance_type: Type) -> InstanceDecl {
        InstanceDecl {
            class_name: "Appendable".to_string(),
            instance_type,
            det_positions: vec![],
            method_types: HashMap::new(),
        }
    }

    /// Two disjoint concrete instances (Int vs Str) must NOT be reported as overlapping.
    #[test]
    fn test_no_overlap_disjoint_concrete() {
        let mut state = InferState::new();
        let mut env = InstanceEnv::new();

        let int_inst = make_appendable_instance(Type::Int);
        let str_inst = make_appendable_instance(Type::Str);

        env.insert(int_inst).unwrap();

        // Str does not overlap with Int — should be Ok
        assert!(
            env.check_structural_overlap(&str_inst, &mut state).is_ok(),
            "Int and Str instances should not overlap"
        );
    }

    /// `[Seq a]` and `[Seq Int]` overlap: substituting a=Int satisfies both.
    #[test]
    fn test_overlap_seq_a_vs_seq_int() {
        let mut state = InferState::new();
        let mut env = InstanceEnv::new();

        let seq_a_inst = make_appendable_instance(make_seq_a());
        let seq_int_inst = make_appendable_instance(make_seq_int());

        env.insert(seq_a_inst).unwrap();

        // [Seq Int] overlaps with [Seq a] — must detect overlap
        let result = env.check_structural_overlap(&seq_int_inst, &mut state);
        assert!(
            result.is_err(),
            "Seq[a] and Seq[Int] should be detected as overlapping instances"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("overlapping instances"),
            "Error message should mention overlapping instances, got: {msg}"
        );
        assert!(
            msg.contains("Appendable"),
            "Error message should mention the class name, got: {msg}"
        );
    }

    /// `[Seq a]` and `[Seq b]` overlap: both accept any Seq, so they are universally overlapping.
    #[test]
    fn test_overlap_seq_a_vs_seq_b() {
        let mut state = InferState::new();
        let mut env = InstanceEnv::new();

        let seq_a_inst = make_appendable_instance(Type::seq(Type::TypeVar("a".to_string(), 0)));
        let seq_b_inst = make_appendable_instance(Type::seq(Type::TypeVar("b".to_string(), 0)));

        env.insert(seq_a_inst).unwrap();

        // [Seq b] overlaps with [Seq a] — they are structurally equivalent
        let result = env.check_structural_overlap(&seq_b_inst, &mut state);
        assert!(
            result.is_err(),
            "Seq[a] and Seq[b] should be detected as overlapping (both accept any Seq)"
        );
    }

    /// Checking overlap against an empty registry never reports overlap.
    #[test]
    fn test_no_overlap_empty_registry() {
        let mut state = InferState::new();
        let env = InstanceEnv::new();
        let inst = make_appendable_instance(make_seq_a());
        assert!(
            env.check_structural_overlap(&inst, &mut state).is_ok(),
            "Empty registry should never report overlap"
        );
    }

    /// check_structural_overlap is side-effect-free: state must not change.
    #[test]
    fn test_overlap_check_is_side_effect_free() {
        let mut state = InferState::new();
        let mut env = InstanceEnv::new();

        env.insert(make_appendable_instance(make_seq_a())).unwrap();

        let counter_before = state.subst.name_counter.get();
        let levels_before = state.levels.clone();
        let constraints_before = state.constraints.clone();

        // This will detect overlap and return Err — but state must be restored.
        let _ = env.check_structural_overlap(&make_appendable_instance(make_seq_int()), &mut state);

        assert_eq!(
            state.subst.name_counter.get(),
            counter_before,
            "name_counter must be restored after overlap check"
        );
        assert_eq!(
            state.levels, levels_before,
            "levels must be restored after overlap check"
        );
        assert_eq!(
            state.constraints, constraints_before,
            "constraints must be restored after overlap check"
        );
    }

    /// When both `[Seq Int]` and `[Seq a]` are registered, resolving against `Seq[Int]`
    /// must select `[Seq Int]` (score 0) over `[Seq a]` (score 1) by specificity.
    ///
    /// Note: We insert directly (bypassing `check_structural_overlap`) to simulate the
    /// scenario T-914 addresses — `resolve_instance` must handle this case even when both
    /// instances are registered (e.g., from different scopes or via built-in seeding).
    #[test]
    fn test_resolve_instance_specificity_concrete_wins_over_polymorphic() {
        let mut state = InferState::new();
        let mut env = InstanceEnv::new();

        // Insert both instances directly (bypassing structural overlap check).
        // [Seq a] — polymorphic, score 1 when matched against Seq[Int]
        env.insert(make_appendable_instance(make_seq_a())).unwrap();
        // [Seq Int] — concrete, score 0 when matched against Seq[Int]
        env.insert(make_appendable_instance(make_seq_int()))
            .unwrap();

        let target = make_seq_int(); // Seq[Int]
        let resolved = env
            .resolve_instance("Appendable", &target, &mut state)
            .expect("should not be ambiguous — [Seq Int] is strictly more specific than [Seq a]");

        assert!(
            resolved.is_some(),
            "should find a matching instance for Seq[Int]"
        );
        let resolved = resolved.unwrap();

        // The winner must be the concrete [Seq Int] instance, not the polymorphic [Seq a].
        // The freshened instance_type of [Seq Int] has no TypeVars, so it should be Seq[Int].
        assert!(
            !resolved.instance_type.has_inference_vars(),
            "resolved instance should be [Seq Int] (no TypeVars), got: {}",
            resolved.instance_type
        );
        if let Some(elem) = resolved.instance_type.as_seq() {
            assert!(
                matches!(elem, Type::Int),
                "element of resolved Seq should be Int, got: {}",
                elem
            );
        } else {
            panic!(
                "resolved instance type should be Seq[Int], got: {}",
                resolved.instance_type
            );
        }
    }

    /// When `[Seq a]` and `[Seq b]` are both registered (equally polymorphic), resolving
    /// against `Seq[Int]` must report ambiguity — both score 1, so neither wins.
    #[test]
    fn test_resolve_instance_ambiguity_equally_specific() {
        let mut state = InferState::new();
        let mut env = InstanceEnv::new();

        // Two equally polymorphic instances — both score 1 for any Seq target.
        let seq_a_inst = make_appendable_instance(Type::seq(Type::TypeVar("a".to_string(), 0)));
        let seq_b_inst = make_appendable_instance(Type::seq(Type::TypeVar("b".to_string(), 0)));

        env.insert(seq_a_inst).unwrap();
        env.insert(seq_b_inst).unwrap();

        let target = make_seq_int();
        let result = env.resolve_instance("Appendable", &target, &mut state);

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

    /// When only `[Seq a]` is registered, it resolves for `Seq[Int]` without ambiguity —
    /// single match always wins regardless of polymorphism score.
    #[test]
    fn test_resolve_instance_single_match_no_ambiguity() {
        let mut state = InferState::new();
        let mut env = InstanceEnv::new();

        // Only the polymorphic instance — no competition.
        env.insert(make_appendable_instance(make_seq_a())).unwrap();

        let target = make_seq_int();
        let resolved = env
            .resolve_instance("Appendable", &target, &mut state)
            .expect("single match should not be ambiguous");

        assert!(
            resolved.is_some(),
            "should find a matching instance for Seq[Int] via [Seq a]"
        );
    }

    /// check_structural_overlap walks the parent chain (T-1031): an instance in a parent frame
    /// must be detected as overlapping with a candidate inserted into a child frame.
    #[test]
    fn test_overlap_check_parent_chain() {
        let mut state = InferState::new();

        // Parent frame: register Appendable(Int).
        let mut parent_env = InstanceEnv::new();
        let int_inst = make_appendable_instance(Type::Int);
        parent_env.insert(int_inst).unwrap();

        // Child frame: no instances yet, parent contains Appendable(Int).
        let child_env = InstanceEnv::child(Arc::new(parent_env));

        // Attempting to add another Appendable(Int) to child must detect overlap with parent.
        let duplicate_int = make_appendable_instance(Type::Int);
        let result = child_env.check_structural_overlap(&duplicate_int, &mut state);
        assert!(
            result.is_err(),
            "Appendable(Int) in child should overlap with Appendable(Int) in parent frame"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("overlapping instances"),
            "Error message should mention overlapping instances, got: {msg}"
        );

        // A disjoint instance (Appendable(Str)) must NOT be reported as overlapping.
        let str_inst = make_appendable_instance(Type::Str);
        assert!(
            child_env
                .check_structural_overlap(&str_inst, &mut state)
                .is_ok(),
            "Appendable(Str) should not overlap with Appendable(Int) in parent frame"
        );
    }

    /// Resolving against a target that matches no instance returns Ok(None).
    #[test]
    fn test_resolve_instance_no_match_returns_none() {
        let mut state = InferState::new();
        let mut env = InstanceEnv::new();

        // Only Seq[Int] registered.
        env.insert(make_appendable_instance(make_seq_int()))
            .unwrap();

        // Target is Seq[Str] — does not match Seq[Int].
        let target = Type::seq(Type::Str);
        let resolved = env
            .resolve_instance("Appendable", &target, &mut state)
            .expect("no match should not yield an error");

        assert!(
            resolved.is_none(),
            "Seq[Str] should not match [Seq Int] instance"
        );
    }
}
