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
    /// Used in built-in environment construction where the full `ClassDecl` is not yet available.
    /// KNOWN ISSUE (T6): Constraint::new_by_name creates minimal ClassDecl with empty determines.
    /// Any code path creating arithmetic constraints via new_by_name instead of full ClassDecl
    /// from state.class_env produces constraints where improve_functional_dependency finds no FDs
    /// and silently skips. Audit all new_by_name call sites to verify they are not used for
    /// FD-bearing classes (Add/Sub/Mul/Div). Currently only used in tests and for non-FD classes.
    /// Deferred to chr-new-by-name-audit sprint.
    pub fn new_by_name(name: impl Into<String>, var: impl Into<String>) -> Self {
        let name = name.into();
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
    /// If true, the type checker can use the resolver result to refine the determining types.
    /// Field is fully wired through parser → AST → typecheck → ClassDecl (chr-instances-gaps done).
    ///
    /// KNOWN ISSUE (T4): Reverse FD (resolver_injective) not implemented — this field exists
    /// but is dead code. Parser can parse `injective: true` and threads it through to ClassDecl,
    /// but FD improvement only fires forward (determining→determined). Backward inference
    /// (determined→determining) requires congruence-based unification of TypeStageApp nodes,
    /// which is blocked on stuck-app deferral and unification semantics. Deferred to chr-reverse-fd sprint.
    ///
    /// The read site is future CHR congruence work (bidirectional FD refinement from resolver output).
    #[allow(dead_code)] // read site is future CHR congruence work, not yet implemented
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

/// Class environment: global registry of type class declarations.
#[derive(Debug, Clone)]
pub struct ClassEnv {
    classes: HashMap<String, ClassDecl>,
}

impl ClassEnv {
    pub fn new() -> Self {
        Self {
            classes: HashMap::new(),
        }
    }

    /// Look up a class declaration by name.
    pub fn get(&self, name: &str) -> Option<&ClassDecl> {
        self.classes.get(name)
    }

    pub fn insert(&mut self, class_decl: ClassDecl) {
        self.classes.insert(class_decl.name.clone(), class_decl);
    }

    /// Insert a class declaration only if no class with that name is already registered.
    /// Used when seeding from the prelude cache to avoid overwriting user-defined classes.
    pub fn insert_if_absent(&mut self, class_decl: ClassDecl) {
        self.classes
            .entry(class_decl.name.clone())
            .or_insert(class_decl);
    }

    /// Iterate over all locally registered class declarations (does not traverse parent chain).
    pub fn iter_classes(&self) -> impl Iterator<Item = &ClassDecl> {
        self.classes.values()
    }
}

impl Default for ClassEnv {
    fn default() -> Self {
        Self::new()
    }
}

/// Instance environment: global registry of type class instances.
///
/// Instances are stored with a key of `(class_name, determining_type_strings)` for fast
/// exact-match lookup, where `determining_type_strings` is the vec of string-formatted types
/// at the class's determining (LHS of functional dependency) positions. For single-parameter
/// classes with no functional dependencies the key vec has one element: the string
/// representation of the sole instance type.
///
/// This representation supports both single-parameter and multi-parameter type class (MPTC)
/// instances with functional dependencies. The `lookup_mptc` query API uses structural
/// unification to match instances, correctly handling HKT instance heads with type variables.
#[derive(Debug, Clone)]
pub struct InstanceEnv {
    instances: HashMap<(String, Vec<String>), InstanceDecl>,
}

impl InstanceEnv {
    pub fn new() -> Self {
        Self {
            instances: HashMap::new(),
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
        for ((cname, _), existing) in &self.instances {
            if cname != &candidate.class_name {
                continue;
            }

            // Save ALL fields BEFORE freshening — instantiate_at_level increments name_counter
            // and extends state.levels with fresh type variable entries. Saving before the call
            // means both the freshening allocations and the unification probe are fully rolled back,
            // making this check completely side-effect-free (mirrors patterns_overlap in typecheck.rs).
            let saved_levels = state.levels.clone();
            let saved_constraints = state.constraints.clone();
            let saved_kind_env = state.kind_env.clone();
            let saved_deferred = state.deferred_equalities.clone();
            let saved_subst = state.subst.clone();
            let saved_name_counter = state.name_counter;

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
            state.levels = saved_levels;
            state.constraints = saved_constraints;
            state.kind_env = saved_kind_env;
            state.deferred_equalities = saved_deferred;
            state.subst = saved_subst;
            state.name_counter = saved_name_counter;

            if overlaps {
                return Err(format!(
                    "overlapping instances for class '{}': new instance '{}' overlaps with existing instance '{}'",
                    candidate.class_name,
                    candidate.instance_type,
                    existing.instance_type,
                ));
            }
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
            // and state.name_counter (via instantiate_at_level). Failed candidates must not leak these.
            let saved_levels = state.levels.clone();
            let saved_constraints = state.constraints.clone();
            let saved_kind_env = state.kind_env.clone();
            let saved_deferred = state.deferred_equalities.clone();
            let saved_name_counter = state.name_counter;

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
                        state.levels = saved_levels;
                        state.constraints = saved_constraints;
                        state.kind_env = saved_kind_env;
                        state.deferred_equalities = saved_deferred;
                        state.name_counter = saved_name_counter;
                        continue;
                    }
                }
            };

            // Check arity
            if instance_det_types.len() != determining_types.len() {
                state.levels = saved_levels;
                state.constraints = saved_constraints;
                state.kind_env = saved_kind_env;
                state.deferred_equalities = saved_deferred;
                state.name_counter = saved_name_counter;
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
                state.levels = saved_levels;
                state.constraints = saved_constraints;
                state.kind_env = saved_kind_env;
                state.deferred_equalities = saved_deferred;
                state.name_counter = saved_name_counter;

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
                state.levels = saved_levels;
                state.constraints = saved_constraints;
                state.kind_env = saved_kind_env;
                state.deferred_equalities = saved_deferred;
                state.name_counter = saved_name_counter;
            }
        }

        None
    }

    /// Iterate over all locally registered instance declarations (does not traverse parent chain).
    pub fn iter_instances(&self) -> impl Iterator<Item = &InstanceDecl> {
        self.instances.values()
    }

    /// Resolve an instance for the given class and target type.
    /// Attempts to unify each registered instance's head type with the target type.
    /// Returns a freshened instance declaration if found, with method types substituted
    /// by the unification, or None if no match.
    ///
    /// This performs the following steps for each candidate instance:
    /// 1. Freshen all type variables in the instance type using `instantiate_at_level`
    ///    (prevents type variable leakage across instance resolutions)
    /// 2. Attempt unification of the freshened instance type with the target type
    /// 3. If successful, apply the resulting substitution to the instance's method types
    ///    and return the freshened instance
    ///
    /// This is a simple unification-based resolution: it tries each instance in order
    /// and returns the first that unifies with the target type. More sophisticated
    /// resolution (with backtracking, overlapping instance detection, or instance
    /// selection based on specificity) is deferred to future work.
    pub fn resolve_instance(
        &self,
        class_name: &str,
        target_type: &Type,
        state: &mut InferState,
    ) -> Option<InstanceDecl> {
        // Collect all instances for this class
        let mut candidates = Vec::new();

        for ((cname, _), inst) in &self.instances {
            if cname == class_name {
                candidates.push(inst);
            }
        }

        // Try to unify with each candidate
        for inst in candidates {
            // F1 FIX: Save state before candidate probe to prevent leakage from failed matches.
            // unify() mutates state.levels, state.constraints, state.kind_env, state.deferred_equalities,
            // and state.name_counter (via instantiate_at_level). Failed candidates must not leak these.
            let saved_levels = state.levels.clone();
            let saved_constraints = state.constraints.clone();
            let saved_kind_env = state.kind_env.clone();
            let saved_deferred = state.deferred_equalities.clone();
            let saved_name_counter = state.name_counter;

            // 1. Freshen the instance type to prevent variable leakage
            //    (e.g., `b` in `AppendableSeq [Seq b]` must be fresh for each resolution attempt)
            let freshened_instance_type = instantiate_at_level(&inst.instance_type, state);

            // 2. Create a fresh substitution for this unification attempt
            let mut temp_subst = state.subst.clone();

            // 3. Attempt unification
            if unify(
                &freshened_instance_type,
                target_type,
                &mut temp_subst,
                state,
                Span::origin(),
            )
            .is_ok()
            {
                // 4. Apply the substitution to method types
                //    This threads concrete types from the unification into the methods
                let freshened_method_types: HashMap<String, Type> = inst
                    .method_types
                    .iter()
                    .map(|(name, ty)| {
                        let freshened_ty = instantiate_at_level(ty, state);
                        (name.clone(), temp_subst.apply(&freshened_ty))
                    })
                    .collect();

                // F1 FIX: Restore state after successful probe — we return the instance but don't
                // commit the probe's state mutations (they were exploratory).
                // BUT: preserve peak name_counter to prevent _tN name reuse across candidates.
                let peak_counter = state.name_counter;
                state.levels = saved_levels;
                state.constraints = saved_constraints;
                state.kind_env = saved_kind_env;
                state.deferred_equalities = saved_deferred;
                state.name_counter = saved_name_counter.max(peak_counter);

                return Some(InstanceDecl {
                    class_name: inst.class_name.clone(),
                    instance_type: freshened_instance_type,
                    det_positions: inst.det_positions.clone(),
                    method_types: freshened_method_types,
                });
            } else {
                // F1 FIX: Restore state after failed probe (discard leaked mutations).
                // BUT: preserve peak name_counter to prevent _tN name reuse across candidates.
                let peak_counter = state.name_counter;
                state.levels = saved_levels;
                state.constraints = saved_constraints;
                state.kind_env = saved_kind_env;
                state.deferred_equalities = saved_deferred;
                state.name_counter = saved_name_counter.max(peak_counter);
            }
        }

        None
    }
}

impl Default for InstanceEnv {
    fn default() -> Self {
        Self::new()
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
        Type::Seq(Box::new(Type::TypeVar("a".to_string(), 0)))
    }

    fn make_seq_int() -> Type {
        Type::Seq(Box::new(Type::Int))
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

        let seq_a_inst =
            make_appendable_instance(Type::Seq(Box::new(Type::TypeVar("a".to_string(), 0))));
        let seq_b_inst =
            make_appendable_instance(Type::Seq(Box::new(Type::TypeVar("b".to_string(), 0))));

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

        let counter_before = state.name_counter;
        let levels_before = state.levels.clone();
        let constraints_before = state.constraints.clone();

        // This will detect overlap and return Err — but state must be restored.
        let _ = env.check_structural_overlap(&make_appendable_instance(make_seq_int()), &mut state);

        assert_eq!(
            state.name_counter, counter_before,
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
}
