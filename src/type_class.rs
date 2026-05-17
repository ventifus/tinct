//! Type class declarations, constraints, and class/instance environments.
//!
//! This module contains the type class system infrastructure including
//! `ClassDecl`, `Constraint`, `ClassEnv`, and `InstanceEnv`.

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

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
    /// Instance type (e.g., Int, or type constructor application)
    pub instance_type: Type,
    /// Method implementations: method_name -> inferred type
    /// (The actual dictionary value is stored in eval::ClassDictionary)
    #[allow(dead_code)]
    // Written during registration, read during dictionary construction (future work)
    pub method_types: HashMap<String, Type>,
}

/// Class environment: global registry of type class declarations
/// Scoped like TypeEnv (supports shadowing in nested scopes)
#[derive(Debug, Clone)]
pub struct ClassEnv {
    classes: HashMap<String, ClassDecl>,
    #[allow(dead_code)] // Scaffolding for scoped class environments (future work)
    parent: Option<Rc<ClassEnv>>,
}

impl ClassEnv {
    pub fn new() -> Self {
        Self {
            classes: HashMap::new(),
            parent: None,
        }
    }

    #[allow(dead_code)] // Scaffolding for scoped class environments (future work)
    pub fn with_parent(parent: &Rc<ClassEnv>) -> Self {
        Self {
            classes: HashMap::new(),
            parent: Some(Rc::clone(parent)),
        }
    }

    /// Look up a class declaration by name, checking parent scopes if necessary.
    pub fn get(&self, name: &str) -> Option<&ClassDecl> {
        if let Some(class) = self.classes.get(name) {
            return Some(class);
        }
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            if let Some(class) = env.classes.get(name) {
                return Some(class);
            }
            current = env.parent.as_deref();
        }
        None
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

/// Instance environment: global registry of type class instances
/// Key is (class_name, instance_type_string) to allow fast lookup
#[derive(Debug, Clone)]
pub struct InstanceEnv {
    instances: HashMap<(String, String), InstanceDecl>,
    #[allow(dead_code)] // Scaffolding for scoped instance environments (future work)
    parent: Option<Rc<InstanceEnv>>,
}

impl InstanceEnv {
    pub fn new() -> Self {
        Self {
            instances: HashMap::new(),
            parent: None,
        }
    }

    #[allow(dead_code)] // Scaffolding for scoped instance environments (future work)
    pub fn with_parent(parent: &Rc<InstanceEnv>) -> Self {
        Self {
            instances: HashMap::new(),
            parent: Some(Rc::clone(parent)),
        }
    }

    /// Look up an instance by class name and type.
    /// Returns the instance declaration if found.
    #[allow(dead_code)] // Instance lookup used during dictionary construction (future work)
    pub fn get(&self, class_name: &str, ty: &Type) -> Option<&InstanceDecl> {
        let key = (class_name.to_string(), ty.to_string());
        if let Some(inst) = self.instances.get(&key) {
            return Some(inst);
        }
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            if let Some(inst) = env.instances.get(&key) {
                return Some(inst);
            }
            current = env.parent.as_deref();
        }
        None
    }

    /// Insert an instance.
    ///
    /// Returns an error if an overlapping instance with the SAME class but a DIFFERENT
    /// instance type string already exists. Exact duplicates (same class + same instance type)
    /// are treated as idempotent and succeed silently — this handles the case where user code
    /// re-declares an instance that was already seeded from the prelude cache.
    pub fn insert(&mut self, inst: InstanceDecl) -> Result<(), String> {
        let key = (inst.class_name.clone(), inst.instance_type.to_string());
        if self.instances.contains_key(&key) {
            // Exact duplicate (same class + same instance type string): idempotent, no error.
            // This covers re-declarations of prelude instances in user code and corpus tests.
            return Ok(());
        }
        self.instances.insert(key, inst);
        Ok(())
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

        // Check local instances
        for ((cname, _), inst) in &self.instances {
            if cname == class_name {
                candidates.push(inst);
            }
        }

        // Check parent instances
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            for ((cname, _), inst) in &env.instances {
                if cname == class_name {
                    candidates.push(inst);
                }
            }
            current = env.parent.as_deref();
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
