//! Type class declarations, constraints, and class/instance environments.
//!
//! This module contains the type class system infrastructure including
//! `ClassDecl`, `Constraint`, `ClassEnv`, and `InstanceEnv`.

use std::collections::HashMap;
use std::fmt;

use crate::ast::Span;
use crate::types::{instantiate_at_level, unify, InferState, Kind, Label, Type, TypeScheme};

/// Constraint on a type variable (type class membership or structural property)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Constraint {
    /// Type class constraint: `class vars` (e.g., `Numeric a` or `Add a b c`)
    ///
    /// `vars`: Type variable names in the constraint (e.g., ["a"] for single-param, ["a", "b", "c"] for MPTC)
    /// `fundeps`: Functional dependencies as (determining positions, determined positions) pairs.
    ///            Each pair is (Vec<usize>, Vec<usize>) indexing into `vars`.
    ///            For `Add a b c` with FD `(a,b) → c`: fundeps = vec![(vec![0,1], vec![2])]
    Class {
        class: String,
        vars: Vec<String>,
        fundeps: Vec<(Vec<usize>, Vec<usize>)>,
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
    pub fn new(class: impl Into<String>, var: impl Into<String>) -> Self {
        Self::Class {
            class: class.into(),
            vars: vec![var.into()],
            fundeps: vec![],
        }
    }
}

impl fmt::Display for Constraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Constraint::Class { class, vars, .. } => {
                write!(f, "{}", class)?;
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
#[derive(Debug, Clone)]
pub struct ClassDecl {
    /// Class name (e.g., "Equatable")
    pub name: String,
    /// Type parameters with their kinds (e.g., [("a", Kind::Type)])
    #[allow(dead_code)]
    // Written during registration, read during constraint solving (future work)
    pub params: Vec<(String, Kind)>,
    /// Superclass constraints as (class_name, Vec<param_names>) tuples.
    /// Example: ("Functor", vec!["f"]) means this class extends Functor with parameter f.
    /// Updated from Vec<(String, String)> to Vec<(String, Vec<String>)> for multi-param support.
    #[allow(dead_code)]
    // Written during registration, read during constraint solving (future work)
    pub superclasses: Vec<(String, Vec<String>)>,
    /// Method signatures: method_name -> type scheme
    #[allow(dead_code)]
    // Written during registration, read during method type checking (future work)
    pub methods: HashMap<String, TypeScheme>,
    /// Functional dependencies: (determining_positions, determined_positions) pairs.
    /// Each pair is (Vec<usize>, Vec<usize>) indexing into `params`.
    /// Example: for Add a b c with FD (a,b) → c: determines = vec![(vec![0,1], vec![2])]
    #[allow(dead_code)]
    // Written during class declaration, read during FD constraint generation (chr-normalization sprint)
    pub(crate) determines: Vec<(Vec<usize>, Vec<usize>)>,
    /// Type-stage resolver function name (e.g., "AddResult" for Add class).
    /// When Some, the resolver is called at type-check time to compute determined types from determining types.
    #[allow(dead_code)]
    // Written during class declaration, read during FD resolution (chr-normalization sprint)
    pub(crate) resolver: Option<String>,
    /// Whether the resolver is injective (one-to-one mapping).
    /// If true, the type checker can use the resolver result to refine the determining types.
    #[allow(dead_code)]
    // Written during class declaration, read during FD resolution (chr-normalization sprint)
    pub(crate) resolver_injective: bool,
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
    #[allow(dead_code)]
    // Written during registration, read during dictionary construction (future work)
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
/// Key is `(class_name, determining_type_strings)` where `determining_type_strings` is the
/// vec of string-formatted types at the class's determining (LHS of functional dependency)
/// positions.  For single-parameter classes with no functional dependencies the key vec has
/// one element: the string representation of the sole instance type.
///
/// This representation supports both single-parameter and multi-parameter type class (MPTC)
/// instances with functional dependencies.  See `lookup_mptc` for the query API.
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
                            .map(|ty| type_to_string_key(ty))
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
    /// Note: this function does NOT detect or reject overlapping instances whose keys differ
    /// but whose patterns overlap. Overlap checking is deferred to future work.
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
    /// Builds a key from `determining_types` by normalizing each type to a canonical string via
    /// `type_to_string_key` (promotes `IntLiteral` to `"Int"`, etc.), then delegates to the
    /// internal `instances` map.  Traverses the parent chain so scoped instances are visible.
    ///
    /// Returns `Some(&InstanceDecl)` if an exact key match is found, `None` otherwise.
    ///
    /// This is the query API for MPTC functional-dependency resolution: the caller supplies the
    /// ground types at the determining positions of the class's FD, and this method returns the
    /// registered instance whose key matches.
    pub fn lookup_mptc(&self, class: &str, determining_types: &[Type]) -> Option<&InstanceDecl> {
        let key = (
            class.to_string(),
            determining_types
                .iter()
                .map(type_to_string_key)
                .collect::<Vec<String>>(),
        );
        self.instances.get(&key)
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

                return Some(InstanceDecl {
                    class_name: inst.class_name.clone(),
                    instance_type: freshened_instance_type,
                    det_positions: inst.det_positions.clone(),
                    method_types: freshened_method_types,
                });
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
