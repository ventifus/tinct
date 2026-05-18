//! Type inference machinery: InferState, Substitution, generalization, instantiation.
//!
//! This module contains the core type inference infrastructure including
//! substitution, levels-based let-generalization (Kiselyov 2013), and kind inference.

use std::collections::{HashMap, HashSet};

use crate::ast::Span;
use crate::types::{ClassDecl, ClassEnv, Constraint, InstanceEnv, Kind, KindError, Type};

/// Bounds for a type variable in algebraic subtyping.
/// A type variable α is satisfiable iff join(lower) <: meet(upper).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeVarBounds {
    /// Lower bounds: types that are subtypes of this variable (positive positions).
    /// Multiple lower bounds compact to a union: α ⊇ Int, α ⊇ Str → α = Int | Str
    pub lower: Vec<Type>,
    /// Upper bounds: types that are supertypes of this variable (negative positions).
    /// Multiple upper bounds compact to an intersection: α ⊆ Number, α ⊆ Equatable → α = Number & Equatable
    pub upper: Vec<Type>,
}

impl TypeVarBounds {
    pub fn new() -> Self {
        Self {
            lower: vec![],
            upper: vec![],
        }
    }

    /// Add a lower bound: `ty <: var`
    #[allow(dead_code)] // Scaffolding for algebraic subtyping migration
    pub fn add_lower(&mut self, ty: Type) {
        self.lower.push(ty);
    }

    /// Add an upper bound: `var <: ty`
    #[allow(dead_code)] // Scaffolding for algebraic subtyping migration
    pub fn add_upper(&mut self, ty: Type) {
        self.upper.push(ty);
    }
}

impl Default for TypeVarBounds {
    fn default() -> Self {
        Self::new()
    }
}

/// Provenance for a subtyping constraint — tracks why the constraint was generated.
/// Used for error messages when bounds are unsatisfiable.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // Scaffolding for algebraic subtyping — wired in a future sprint
pub struct ConstraintSource {
    pub span: Span,
    pub reason: String,
}

/// Polymorphic type scheme: ∀ type_vars. constraints => body
/// Used for let-bound polymorphism (Damas-Milner) and type class constraints.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeScheme {
    /// Quantified type variables (e.g., ["a", "b"])
    pub type_vars: Vec<String>,
    /// Type class constraints on quantified variables (e.g., [Equatable a, Numeric b])
    pub constraints: Vec<Constraint>,
    /// Body type (may contain type_vars)
    pub body: Type,
    /// Quantified label variables (for HasField constraints with label polymorphism)
    pub label_vars: Vec<String>,
    /// Optional documentation string (extracted from doc: annotations)
    pub doc: Option<String>,
    /// Nested schemes for function parameters (used for higher-rank types)
    pub inner_schemes: Option<HashMap<String, TypeScheme>>,
}

impl TypeScheme {
    /// Create a monomorphic type scheme (no quantified variables or constraints).
    pub fn mono(ty: Type) -> Self {
        Self {
            type_vars: Vec::new(),
            constraints: Vec::new(),
            body: ty,
            label_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        }
    }
}

/// Maps expression spans `(start_offset, end_offset)` to the TypeScheme of the variable
/// referenced there. Only populated for `VarRef` expressions that resolve to a polymorphic
/// scheme (schemes with constraints or type variables). Used by LSP hover to display
/// constraints (e.g., `Equatable a => Fn@Bool [a a]`).
///
/// Stored in `InferState.scheme_map` during inference, then extracted and returned as part
/// of the type-checking result for LSP consumers.
pub type SchemeMap = HashMap<(usize, usize), TypeScheme>;

/// Inference state for levels-based let-generalization
#[derive(Debug, Clone)]
pub struct InferState {
    /// Monotonic counter for fresh type/row variable names (_t0, _t1, ...).
    /// Uses u32 instead of u64 — assumes no single inference run creates >4B type variables.
    /// (In practice, programs with >1M type variables exhaust memory first via substitution map growth.)
    pub name_counter: u32,
    pub level: u32,
    pub levels: HashMap<String, u32>,
    /// Global accumulated substitution: collects constraints from access-chain inference
    /// and other constraint generators. Applied when resolving type variables during
    /// inference, so that constraints from `$x.field1` are visible when processing
    /// `$x.field2` in the same expression. See doc/07-type-extensions.md Part 5.
    pub subst: Substitution,
    /// Accumulated type class constraints on type variables.
    /// Constraints are generated when overloaded builtins are called with type variables.
    /// During generalization, constraints on generalized variables are included in the TypeScheme.
    pub constraints: Vec<Constraint>,
    /// Type variable bounds for algebraic subtyping.
    /// Maps type variable names to their lower/upper bounds. Used alongside `subst` during
    /// the migration from unification to constraint-based typing. Eventually, bounds will
    /// replace equality-based substitution for all type variables.
    #[allow(dead_code)] // Scaffolding for algebraic subtyping migration
    pub bounds: HashMap<String, TypeVarBounds>,
    /// Kind environment: maps TypeVar names to their kinds.
    /// Populated during class method processing (Kind::Operator) and when `key@"k"` annotations
    /// are resolved (Kind::Label). Used to prevent promotion of label-kinded TypeVars and to
    /// enforce kind checking (e.g., reject `Seq(TypeVar(l, Label))`).
    pub kind_env: HashMap<String, Kind>,
    /// Type class environment: registry of class declarations.
    /// Dict-scoped: class declarations are visible in the dict and children.
    pub class_env: ClassEnv,
    /// Type class instance environment: registry of instance declarations.
    /// Globally registered: coherence requires global uniqueness.
    pub instance_env: InstanceEnv,
    /// Names of bindings that failed type inference, mapping to the span of the failed binding.
    /// Used to annotate downstream T002 "undefined variable" errors with a "caused by" note
    /// that points to the failed definition site instead of just saying "not in scope".
    pub failed_bindings: HashMap<String, Span>,
    /// Span-keyed map from VarRef sites to the TypeScheme of the variable they reference.
    /// Only populated when the caller enables scheme collection (non-None). Used by LSP hover
    /// to display type class constraints alongside the instantiated type.
    ///
    /// Enabled by setting this to `Some(SchemeMap::new())` before running inference.
    pub scheme_map: Option<SchemeMap>,
    /// Name of the function currently being inferred (for polymorphic recursion detection).
    /// Set by infer_fn when entering a function body, cleared when exiting.
    pub current_function: Option<String>,
    /// Expected return type of the currently-inferring function (if annotated).
    /// Set by infer_fn when entering a function body with an explicit return annotation,
    /// cleared when exiting. Used for inferred [do] macro to determine which monad to use.
    pub expected_return: Option<Type>,
    /// Accumulated type diagnostics (warnings, hints).
    /// Populated during type inference and generalization, extracted by typecheck_file.
    pub diagnostics: Vec<crate::error::TypeDiagnostic>,
    /// Deferred equality constraints for stuck TypeStageApp applications.
    /// When a TypeStageApp has non-ground arguments or cannot be reduced, equality
    /// constraints involving it are deferred here. After each round of unification,
    /// process_deferred_equalities attempts to resolve them.
    /// (Unused until chr-prelude sprint implements resolvers that produce TypeStageApp)
    #[allow(dead_code)]
    pub deferred_equalities: Vec<(Type, Type)>,
    /// Boundary guards collected during inference: span → expected_param_type.
    /// When a call-site argument has inferred type `Unknown` and the function parameter
    /// has a concrete type (not Unknown, not TypeVar), this records the boundary crossing.
    /// Used for automatic guard insertion in gradual typing (see doc/feature/gradual-typing.md).
    /// HashMap for O(1) lookup at thunk creation time in eval_recursive.
    pub boundary_guards: HashMap<Span, Type>,
    /// Current functional dependency improvement recursion depth.
    /// Prevents infinite loops through the improve_functional_dependency → unify →
    /// check_constraints_on_var → improve_functional_dependency cycle. Incremented when
    /// entering improve_functional_dependency, decremented when exiting.
    pub fd_depth: usize,
    /// Instance resolution recursion depth.
    /// Prevents infinite loops through the check_constraints_on_var → resolve_instance →
    /// unify → check_constraints_on_var cycle. Incremented when entering resolve_instance,
    /// decremented when exiting. Matches GHC's -freduction-depth semantics (Sulzmann et al. 2007 §3.2).
    pub instance_resolution_depth: u32,
    /// Flag indicating whether we are currently type-checking the prelude.
    /// When true, instance method body inference is skipped (optimization — method types
    /// are unused during prelude loading as they are #[allow(dead_code)] in InstanceDecl).
    pub in_prelude_load: bool,
}

impl InferState {
    pub fn new() -> Self {
        let mut class_env = ClassEnv::new();

        // Register built-in type classes with their superclass relationships.
        // Class declarations define the hierarchy (which classes extend which).
        // Instance resolution happens in two stages:
        //   1. satisfies_constraint: hardcoded for Numeric only
        //   2. InstanceEnv::resolve_instance: dynamic resolution from prelude.llt instances
        //
        // These pre-registrations ensure the class hierarchy is available before prelude.llt
        // is type-checked. When prelude.llt is loaded, it will register instances (not classes)
        // which will be used by resolve_instance for constraint checking.

        // Equatable: base class (instances defined in prelude.llt)
        class_env.insert(ClassDecl {
            name: "Equatable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![],
            methods: HashMap::new(),
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Numeric: extends Equatable (hardcoded instance set for Int/Float/Number/IntLiteral)
        class_env.insert(ClassDecl {
            name: "Numeric".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![("Equatable".to_string(), vec!["a".to_string()])],
            methods: HashMap::new(),
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Add: 3-parameter type class with functional dependency (a,b) → c
        // determines will be populated with FD data in chr-normalization sprint
        class_env.insert(ClassDecl {
            name: "Add".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            methods: HashMap::new(),
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Sub: 3-parameter type class with functional dependency (a,b) → c
        class_env.insert(ClassDecl {
            name: "Sub".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            methods: HashMap::new(),
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Mul: 3-parameter type class with functional dependency (a,b) → c
        class_env.insert(ClassDecl {
            name: "Mul".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            methods: HashMap::new(),
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Div: 3-parameter type class with functional dependency (a,b) → c
        class_env.insert(ClassDecl {
            name: "Div".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            methods: HashMap::new(),
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Comparable: extends Equatable (instances defined in prelude.llt)
        class_env.insert(ClassDecl {
            name: "Comparable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![("Equatable".to_string(), vec!["a".to_string()])],
            methods: HashMap::new(),
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Showable: base class (instances defined in prelude.llt)
        class_env.insert(ClassDecl {
            name: "Showable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![],
            methods: HashMap::new(),
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Mappable: base class (instances defined in prelude.llt)
        // Kind::Operator for higher-kinded type constructor polymorphism
        class_env.insert(ClassDecl {
            name: "Mappable".to_string(),
            params: vec![("f".to_string(), Kind::Operator)],
            superclasses: vec![],
            methods: HashMap::new(),
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Appendable: base class (instances defined in prelude.llt)
        class_env.insert(ClassDecl {
            name: "Appendable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![],
            methods: HashMap::new(),
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        Self {
            name_counter: 0,
            level: 0,
            levels: HashMap::new(),
            subst: Substitution::new(),
            constraints: Vec::new(),
            bounds: HashMap::new(),
            kind_env: HashMap::new(),
            class_env,
            instance_env: InstanceEnv::new(),
            failed_bindings: HashMap::new(),
            scheme_map: None,
            current_function: None,
            expected_return: None,
            diagnostics: Vec::new(),
            deferred_equalities: Vec::new(),
            boundary_guards: HashMap::new(),
            fd_depth: 0,
            instance_resolution_depth: 0,
            in_prelude_load: false,
        }
    }

    /// Add a type class constraint to the inference state.
    /// The constraint is checked during instantiation.
    pub fn add_constraint(&mut self, class: impl Into<String>, var: impl Into<String>) {
        self.constraints.push(Constraint::new(class, var));
    }

    /// Create a fresh type variable at the current level and register it in `state.levels`.
    pub fn fresh_type_var(&mut self) -> Type {
        let name = format!("_t{}", self.name_counter);
        self.name_counter = self.name_counter.saturating_add(1);
        self.levels.insert(name.clone(), self.level);
        Type::TypeVar(name, self.level)
    }

    // fresh_row_var_name removed — BAS Step 4: no RowVar tails exist

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn fresh_var(&mut self) -> Type {
        self.fresh_type_var()
    }
}

impl Default for InferState {
    fn default() -> Self {
        Self::new()
    }
}

/// State for kind inference and unification (Jones 1993)
///
/// Kind inference assigns kinds to type constructors and validates their usage.
/// For example, `Seq` has kind `* -> *` (takes a type, returns a type), while
/// `Int` has kind `*` (is a proper type). Kind variables (`Kind::Var`) are used
/// for type constructors whose kind is not yet known.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Scaffolding for type class implementation
pub struct KindState {
    /// Monotonic counter for fresh kind variable IDs
    pub next_var: u32,
    /// Substitution from kind variables to kinds
    pub substitution: HashMap<u32, Kind>,
}

impl KindState {
    /// Create a new kind inference state
    pub fn new() -> Self {
        Self {
            next_var: 0,
            substitution: HashMap::new(),
        }
    }

    /// Generate a fresh kind variable
    #[allow(dead_code)] // Scaffolding for kind inference
    pub fn fresh_var(&mut self) -> Kind {
        let id = self.next_var;
        self.next_var = self.next_var.saturating_add(1);
        Kind::Var(id)
    }

    /// Apply the current substitution to a kind (chase bindings to fixpoint)
    pub fn apply(&self, kind: &Kind) -> Kind {
        match kind {
            Kind::Type => Kind::Type,
            Kind::Arrow(k1, k2) => Kind::Arrow(Box::new(self.apply(k1)), Box::new(self.apply(k2))),
            Kind::Operator => Kind::Operator,
            Kind::Label => Kind::Label,
            Kind::Var(id) => {
                if let Some(k) = self.substitution.get(id) {
                    self.apply(k) // Chase transitive bindings
                } else {
                    Kind::Var(*id)
                }
            }
        }
    }

    /// Default all unresolved kind variables to `Kind::Type` (Jones 1993, §4)
    ///
    /// After kind inference completes, any remaining kind variables represent unconstrained
    /// type constructors. By convention (and for simplicity), we default them to `*` (proper types).
    /// This is sound because:
    /// - If a type constructor has no applied arguments, it must have kind `*`
    /// - If it has arguments but no kind constraints, defaulting to `*` is the most permissive choice
    ///
    /// Example: `[@Seq<$T> [1 2 3]]` infers `Seq: ?k0 -> *`, then defaults `?k0` to `*`, giving `Seq: * -> *`.
    #[allow(dead_code)] // Scaffolding for kind inference
    pub fn default_remaining(&mut self) {
        // Collect all kind variables that appear in the substitution (transitively)
        let mut all_vars = HashSet::new();
        for k in self.substitution.values() {
            self.collect_kind_vars(k, &mut all_vars);
        }

        // Default any unbound kind variables to Type
        for id in 0..self.next_var {
            if !self.substitution.contains_key(&id) && !all_vars.contains(&id) {
                self.substitution.insert(id, Kind::Type);
            }
        }
    }

    /// Collect all kind variables appearing in a kind (for defaulting)
    #[allow(dead_code)] // Scaffolding for kind inference
    fn collect_kind_vars(&self, kind: &Kind, vars: &mut HashSet<u32>) {
        match kind {
            Kind::Type => {}
            Kind::Arrow(k1, k2) => {
                self.collect_kind_vars(k1, vars);
                self.collect_kind_vars(k2, vars);
            }
            Kind::Operator => {}
            Kind::Label => {}
            Kind::Var(id) => {
                if vars.insert(*id) {
                    // Only recurse if we haven't seen this variable before
                    if let Some(k) = self.substitution.get(id) {
                        self.collect_kind_vars(k, vars);
                    }
                }
            }
        }
    }
}

impl Default for KindState {
    fn default() -> Self {
        Self::new()
    }
}

/// Occurs check for kind unification — does kind variable `v` appear in kind `k`?
///
/// Prevents infinite kinds like `?k0 = ?k0 -> *`, which would create cycles in the kind structure.
#[allow(dead_code)] // Scaffolding for type class implementation
fn occurs_in_kind(v: u32, k: &Kind, state: &KindState) -> bool {
    match state.apply(k) {
        Kind::Type => false,
        Kind::Arrow(k1, k2) => occurs_in_kind(v, &k1, state) || occurs_in_kind(v, &k2, state),
        Kind::Operator => false,
        Kind::Label => false,
        Kind::Var(id) => id == v,
    }
}

/// Unify two kinds, updating the kind substitution (Robinson's Algorithm U for kinds)
///
/// Kind unification is simpler than type unification because kinds don't have rows,
/// literals, or subtyping. It's pure structural unification:
/// - `*` unifies with `*`
/// - `k1 -> k2` unifies with `k3 -> k4` if `k1 ~ k3` and `k2 ~ k4`
/// - `?k` unifies with any kind `k` (if occurs check passes)
#[allow(dead_code)] // Scaffolding for type class implementation
pub fn unify_kind(k1: &Kind, k2: &Kind, state: &mut KindState) -> Result<(), KindError> {
    let k1 = state.apply(k1);
    let k2 = state.apply(k2);

    match (&k1, &k2) {
        (Kind::Type, Kind::Type) => Ok(()),
        (Kind::Arrow(a1, r1), Kind::Arrow(a2, r2)) => {
            unify_kind(a1, a2, state)?;
            unify_kind(r1, r2, state)
        }
        (Kind::Var(v), k) | (k, Kind::Var(v)) => {
            // Occurs check: prevent infinite kinds
            if occurs_in_kind(*v, k, state) {
                return Err(KindError::InfiniteKind);
            }
            state.substitution.insert(*v, k.clone());
            Ok(())
        }
        _ => Err(KindError::Mismatch(k1, k2)),
    }
}

// Substitution is defined in type_unify.rs and re-exported here so that
// type_infer.rs callers can use it without a separate import.
pub use crate::types::Substitution;
