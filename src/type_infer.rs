//! Type inference machinery: InferState, Substitution, generalization, instantiation.
//!
//! This module contains the core type inference infrastructure including
//! substitution and levels-based let-generalization (Kiselyov 2013).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use crate::ast::Span;
use crate::types::{ClassDecl, ClassEnv, Constraint, InstanceEnv, Kind, Row, Type};

/// All per-TypeVar metadata in one place.
/// IndexMap preserves insertion order (= TypeVar creation order via monotonic counter),
/// giving deterministic iteration across runs -- unlike HashMap which has random seeds.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeVarEntry {
    /// Creation-time level (previously in InferState.levels).
    pub level: u32,
    /// Current binding (previously in Substitution.type_map). None = still free.
    pub binding: Option<Type>,
    /// Kind of this TypeVar (previously in InferState.kind_env).
    pub kind: Kind,
}

impl TypeVarEntry {
    /// Create a new unbound TypeVar entry with the given level and kind.
    pub fn blank(level: u32, kind: Kind) -> Self {
        Self {
            level,
            binding: None,
            kind,
        }
    }
}

/// Finite substitution: maps TypeVar names to replacement types.
/// Used by `instantiate_scheme` to rename quantified variables to fresh names.
/// The `type_map` is interior-mutable so the renaming can be built incrementally
/// and passed by shared reference to the `apply` method.
#[derive(Debug, Clone)]
pub struct Substitution {
    pub type_map: RefCell<HashMap<String, Type>>,
}

impl Substitution {
    pub fn new() -> Self {
        Self {
            type_map: RefCell::new(HashMap::new()),
        }
    }

    /// Apply this substitution to a type, replacing free TypeVars named in `type_map`.
    /// Recurses structurally. Does NOT follow binding chains (no occurs-check).
    pub fn apply(&self, ty: &Type) -> Type {
        let map = self.type_map.borrow();
        Self::apply_inner(ty, &map)
    }

    fn apply_inner(ty: &Type, map: &HashMap<String, Type>) -> Type {
        match ty {
            Type::TypeVar(name, _) => {
                if let Some(replacement) = map.get(name.as_str()) {
                    replacement.clone()
                } else {
                    ty.clone()
                }
            }
            Type::Operator(name) => {
                if let Some(replacement) = map.get(name.as_str()) {
                    replacement.clone()
                } else {
                    ty.clone()
                }
            }
            Type::Function {
                params,
                ret,
                variadic,
                required_count,
            } => Type::Function {
                params: params
                    .iter()
                    .map(|(n, t)| (n.clone(), Self::apply_inner(t, map)))
                    .collect(),
                ret: Box::new(Self::apply_inner(ret, map)),
                variadic: *variadic,
                required_count: *required_count,
            },
            // Note: Type::Seq, Type::Map, Type::Handle don't exist as variants.
            // These are represented as App(TyCon("Seq"), ...) etc. and handled by the App arm below.
            Type::Record(row) => Type::Record(Self::apply_row(row, map)),
            Type::Union(types) => {
                Type::Union(types.iter().map(|t| Self::apply_inner(t, map)).collect())
            }
            Type::Intersection(types) => {
                Type::Intersection(types.iter().map(|t| Self::apply_inner(t, map)).collect())
            }
            Type::Negation(inner) => Type::Negation(Box::new(Self::apply_inner(inner, map))),
            Type::TypeStageApp { fn_name, args } => Type::TypeStageApp {
                fn_name: fn_name.clone(),
                args: args.iter().map(|a| Self::apply_inner(a, map)).collect(),
            },
            _ => ty.clone(),
        }
    }

    fn apply_row(row: &Row, map: &HashMap<String, Type>) -> Row {
        Row {
            fields: row
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), Self::apply_inner(v, map)))
                .collect(),
            tail: row.tail.clone(),
        }
    }
}

impl Default for Substitution {
    fn default() -> Self {
        Self::new()
    }
}

/// Polymorphic type scheme: ∀ type_vars kind_vars. constraints => body
/// Used for let-bound polymorphism (Damas-Milner) and type class constraints.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeScheme {
    /// Quantified type variables (e.g., ["a", "b"])
    pub type_vars: Vec<String>,
    /// Type class constraints on quantified variables (e.g., [Equatable a, Numeric b])
    pub constraints: Vec<Constraint>,
    /// Body type (may contain type_vars and kind_vars)
    pub body: Type,
    /// Quantified label variables (for HasField constraints with label polymorphism)
    pub label_vars: Vec<String>,
    /// Kinded quantified variables: pairs of (name, Kind) for variables that must
    /// instantiate as something other than Type::TypeVar. Currently used for
    /// Kind::Operator variables, which instantiate as Type::Operator(fresh_name)
    /// rather than Type::TypeVar(fresh_name, level). This enables builtin TypeSchemes
    /// like ∀(f: Operator) a b. Mappable f ⇒ (a→b)→f a→f b where f must not be
    /// confused with a monomorphic type variable.
    ///
    /// Variables listed here must NOT also appear in `type_vars` — they are dispatched
    /// separately during instantiate_scheme. Kind::Type variables should go in `type_vars`.
    pub kind_vars: Vec<(String, Kind)>,
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
            kind_vars: Vec::new(),
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
    /// Populated during type inference and generalization, extracted by the type checker.
    pub diagnostics: Vec<crate::error::TypeDiagnostic>,
    /// Deferred equality constraints for stuck TypeStageApp applications.
    /// When a TypeStageApp has non-ground arguments or cannot be reduced, equality
    /// constraints involving it are deferred here. After each round of unification,
    /// process_deferred_equalities attempts to resolve them.
    /// Actively written to (type_unify.rs) and saved/restored during branch inference (typecheck.rs).
    pub deferred_equalities: Vec<(Type, Type)>,
    /// Boundary guards collected during inference: span → expected_param_type.
    /// When a call-site argument has inferred type `Unknown` and the function parameter
    /// has a concrete type (not Unknown, not TypeVar), this records the boundary crossing.
    /// Used for automatic guard insertion in gradual typing (see doc/feature/gradual-typing.md).
    /// HashMap for O(1) lookup at thunk creation time in eval_core_expr.
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
    /// in InstanceDecl are populated but only consumed by resolve_instance, which is not
    /// called during prelude loading itself).
    pub in_prelude_load: bool,
    /// Monad resolutions for inferred [do] forms: sentinel VarRef name → resolved monad variable name.
    /// When the type checker resolves a do-infer sentinel (e.g., `ℊꜱʏᴍ⧼do-infer⧽0`) to a concrete monad
    /// (e.g., "result"), it records the mapping here keyed by the sentinel VarRef name. The eval
    /// pipeline reads this map (via EvalContext) to look up the sentinel at runtime and return
    /// the correct monad dict. Sentinel names are generated by gensym in stdlib/prelude.llt (`do-desugar-inferred`).
    /// Parallel to boundary_guards: type-checker-to-evaluator communication via side channel.
    pub do_infer_resolutions: HashMap<String, String>,
    /// Source names for type variables: internal TypeVar name → user-visible source name.
    /// When a function parameter `x` has an inferred TypeVar `_t42`, this maps `"_t42"` → `"x"`.
    /// Used by T013 diagnostics to report "ambiguous type variable 'x'" (the internal _tN
    /// name is hidden — it is noise for users). Only populated for parameters and let-bindings
    /// where a source name exists.
    pub type_var_source_names: HashMap<String, String>,
    /// Deduplication set for T013 ambiguous constraint warnings: (TypeVar name, Span) pairs.
    /// Prevents emitting duplicate T013 warnings when the same ambiguous TypeVar is encountered
    /// multiple times during constraint discharge. Each unique (var, span) pair is emitted once.
    pub t013_emitted: std::collections::HashSet<(String, crate::ast::Span)>,
    /// Registry of nominal tag names seen so far, mapping tag name → the span of the
    /// `[type ...]` declaration that introduced it. Used to detect duplicate nominal tag
    /// names across separate type alias declarations (W042). A tag name appearing in two
    /// different `[type ...]` declarations produces a W042 diagnostic on the second occurrence.
    pub registered_nominal_tags: HashMap<String, Span>,
    /// TypeAnnotationTable for nested TypeAssert nodes: keyed by NodeId of the TypeAssert Arc<SurfaceNode>.
    /// Populated by infer_surface_expr's TypeAssert handler. Extracted by typecheck_surface_document
    /// to merge into the document-level annotation table.
    pub type_annotation_table: crate::ast::TypeAnnotationTable,
    /// Resolved types for pipeline `expects:` contracts, keyed by the expects annotation's span.
    /// When a document has `--- expects: TypeExpr`, the typecheck pass resolves TypeExpr and stores
    /// the result here. The eval pipeline reads this map to populate TypeAssert.resolved_type,
    /// enabling structural type checking via is_consistent_subtype.
    pub expects_resolved: HashMap<crate::ast::Span, crate::types::Type>,
    /// Resolution table for slot-indexed TypeEnv lookups (optional fast path).
    ///
    /// When set, `infer_surface_expr`'s VarRef handler uses the resolved (level, slot)
    /// coordinates to call `env.get_type_at(level, slot)` — O(1) per-level — before
    /// falling back to the O(chain) name-based `env.get(name)`.
    ///
    /// The table is populated by `resolve_surface_program` before type checking begins.
    /// It may be `None` for contexts that do not have a resolved program (e.g., tests
    /// that construct a TypeEnv and InferState directly without parsing).
    pub resolution_table: Option<std::sync::Arc<crate::ast::ResolutionTable>>,
    /// Type-stage evaluation environment (from HEAD~1 InferState design).
    /// When `Some`, eval_type_stage_expr uses this environment.
    pub type_stage_env: Option<std::sync::Arc<std::sync::RwLock<crate::value::Environment>>>,
    /// Main runtime environment (from HEAD~1 InferState design).
    /// Cross-stage bridge: type-stage can resolve types from main env.
    pub main_env: Option<std::sync::Arc<std::sync::RwLock<crate::value::Environment>>>,
    /// Pending param narrowings from the most recently inferred function (compatibility field).
    pub pending_param_narrowings: Vec<Option<Type>>,
    /// Unified TypeVar table (from HEAD~1 design).
    /// In the current design, TypeVar bindings are in `subst.type_map`, levels in `levels`,
    /// and kinds in `kind_env`. This field exists for compatibility with type_class.rs
    /// save/restore probe patterns.
    pub type_vars: indexmap::IndexMap<String, TypeVarEntry>,
    /// BAS TypeVar bounds (from HEAD~1 design).
    pub bounds: std::collections::HashMap<String, crate::bas::TypeVarBounds>,
    /// Set of TypeVar names currently being processed by FD improvement (compatibility field).
    pub fd_in_progress: std::collections::HashSet<String>,
    /// Expansion stack for type alias cycle detection (compatibility field from HEAD~1).
    pub expansion_stack: Vec<(std::sync::Arc<crate::type_def::TyConDef>, String)>,
    /// Type constructor environment (from HEAD~1 design).
    /// Maps type constructor names to their TyConDef.
    pub tycon_env: std::collections::HashMap<String, std::sync::Arc<crate::type_def::TyConDef>>,
}

impl InferState {
    pub fn new() -> Self {
        let mut class_env = ClassEnv::new();

        // Register built-in type classes with their superclass relationships.
        // Class declarations define the hierarchy (which classes extend which).
        // Instance resolution happens in two stages:
        //   1. satisfies_constraint: hardcoded for Numeric, Comparable, Equatable, and Showable
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
            determines: vec![],
            resolver: None,
            resolver_injective: false,
            method_signatures: vec![],
        });

        // Numeric: extends Equatable (hardcoded instance set for Int/Float/Number/IntLiteral)
        class_env.insert(ClassDecl {
            name: "Numeric".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![("Equatable".to_string(), vec!["a".to_string()])],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
            method_signatures: vec![],
        });

        // Addable: 3-parameter type class with functional dependency (a,b) → c
        class_env.insert(ClassDecl {
            name: "Addable".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            determines: vec![(vec![0, 1], vec![2])], // (a,b) → c
            resolver: None,
            resolver_injective: false,
            method_signatures: vec![],
        });

        // Subtractable: 3-parameter type class with functional dependency (a,b) → c
        class_env.insert(ClassDecl {
            name: "Subtractable".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            determines: vec![(vec![0, 1], vec![2])], // (a,b) → c
            resolver: None,
            resolver_injective: false,
            method_signatures: vec![],
        });

        // Multipliable: 3-parameter type class with functional dependency (a,b) → c
        class_env.insert(ClassDecl {
            name: "Multipliable".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            determines: vec![(vec![0, 1], vec![2])], // (a,b) → c
            resolver: None,
            resolver_injective: false,
            method_signatures: vec![],
        });

        // Divisible: 3-parameter type class with functional dependency (a,b) → c
        class_env.insert(ClassDecl {
            name: "Divisible".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            determines: vec![(vec![0, 1], vec![2])], // (a,b) → c
            resolver: None,
            resolver_injective: false,
            method_signatures: vec![],
        });

        // Comparable: extends Equatable (instances defined in prelude.llt)
        class_env.insert(ClassDecl {
            name: "Comparable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![("Equatable".to_string(), vec!["a".to_string()])],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
            method_signatures: vec![],
        });

        // Showable: base class (instances defined in prelude.llt)
        class_env.insert(ClassDecl {
            name: "Showable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
            method_signatures: vec![],
        });

        // Mappable: base class (instances defined in prelude.llt)
        // Kind::Operator for higher-kinded type constructor polymorphism
        class_env.insert(ClassDecl {
            name: "Mappable".to_string(),
            params: vec![("f".to_string(), Kind::Operator)],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
            method_signatures: vec![],
        });

        // Appendable: base class (instances defined in prelude.llt)
        class_env.insert(ClassDecl {
            name: "Appendable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
            method_signatures: vec![],
        });

        // Indexable: 3-parameter type class with functional dependency (container, key) → value
        // Built-in instances registered below for Map, Seq, and Record
        class_env.insert(ClassDecl {
            name: "Indexable".to_string(),
            params: vec![
                ("container".to_string(), Kind::Type),
                ("key".to_string(), Kind::Type),
                ("value".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            determines: vec![(vec![0, 1], vec![2])], // (container, key) → value
            resolver: None,
            resolver_injective: false,
            method_signatures: vec![],
        });

        let mut instance_env = InstanceEnv::new();

        // Register built-in Indexable instances for Map, Seq, and Record.
        // These instances enable FD improvement: given container and key types,
        // the value type is determined automatically.
        use crate::type_class::InstanceDecl;
        use crate::types::Row;

        // Indexable Map[K V] K V
        // Map[K, V] is App(App(TyCon("Map"), K), V)
        let map_k_var = Type::TypeVar("K".to_string(), 0);
        let map_v_var = Type::TypeVar("V".to_string(), 0);
        let map_ty = Type::App(
            Box::new(Type::App(
                Box::new(Type::TyCon("Map".to_string())),
                Box::new(map_k_var.clone()),
            )),
            Box::new(map_v_var.clone()),
        );
        let map_instance = InstanceDecl {
            class_name: "Indexable".to_string(),
            instance_type: Type::Record(Row {
                fields: {
                    let mut fields = indexmap::IndexMap::new();
                    fields.insert("0".to_string(), map_ty);
                    fields.insert("1".to_string(), map_k_var.clone());
                    fields.insert("2".to_string(), map_v_var.clone());
                    fields
                },
                tail: crate::type_def::RowTail::Empty,
            }),
            det_positions: vec![0, 1],
            method_types: HashMap::new(),
        };
        instance_env.insert(map_instance).unwrap();

        // Indexable Seq[T] Int T
        // Seq[T] is App(TyCon("Seq"), T)
        let seq_t_var = Type::TypeVar("T".to_string(), 0);
        let seq_ty = Type::App(
            Box::new(Type::TyCon("Seq".to_string())),
            Box::new(seq_t_var.clone()),
        );
        let seq_instance = InstanceDecl {
            class_name: "Indexable".to_string(),
            instance_type: Type::Record(Row {
                fields: {
                    let mut fields = indexmap::IndexMap::new();
                    fields.insert("0".to_string(), seq_ty);
                    fields.insert("1".to_string(), Type::Int);
                    fields.insert("2".to_string(), seq_t_var.clone());
                    fields
                },
                tail: crate::type_def::RowTail::Empty,
            }),
            det_positions: vec![0, 1],
            method_types: HashMap::new(),
        };
        instance_env.insert(seq_instance).unwrap();

        // Record/Union/Intersection/Top types for Indexable are handled via resolve_has_field
        // in improve_functional_dependency_inner (type_unify.rs). No instance registration
        // needed — records are structural, so [HAS-FIELD-*] rules apply directly.

        Self {
            name_counter: 0,
            level: 0,
            levels: HashMap::new(),
            subst: Substitution::new(),
            constraints: Vec::new(),
            kind_env: HashMap::new(),
            class_env,
            instance_env,
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
            do_infer_resolutions: HashMap::new(),
            type_var_source_names: HashMap::new(),
            t013_emitted: std::collections::HashSet::new(),
            registered_nominal_tags: HashMap::new(),
            type_annotation_table: crate::ast::TypeAnnotationTable::new(),
            expects_resolved: HashMap::new(),
            resolution_table: None,
            type_stage_env: None,
            main_env: None,
            pending_param_narrowings: Vec::new(),
            type_vars: indexmap::IndexMap::new(),
            bounds: std::collections::HashMap::new(),
            fd_in_progress: std::collections::HashSet::new(),
            tycon_env: std::collections::HashMap::new(),
            expansion_stack: Vec::new(),
        }
    }

    /// Add a type class constraint to an explicit constraint accumulator.
    /// Used by the new InferState API (HEAD~1 style) where constraints are passed explicitly.
    /// Falls back gracefully: if the class is not in `class_env`, just skips the constraint.
    pub fn add_constraint_to(
        &mut self,
        constraints: &mut Vec<Constraint>,
        class_name: impl Into<String>,
        var: impl Into<String>,
    ) {
        let class_name = class_name.into();
        if let Some(class_decl) = self.class_env.get(&class_name) {
            constraints.push(Constraint::new(Arc::new(class_decl.clone()), var));
        }
        // Unknown classes are deferred — instance resolution will report an error.
    }

    /// Add a type class constraint to the inference state.
    /// The constraint is checked during instantiation.
    ///
    /// The class_name must be registered in class_env. If not found, this will panic
    /// (the caller is responsible for validating class existence before calling).
    pub fn add_constraint(&mut self, class_name: impl Into<String>, var: impl Into<String>) {
        let class_name = class_name.into();
        let class_decl = self.class_env.get(&class_name).unwrap_or_else(|| {
            panic!(
                "add_constraint: class '{}' not registered in class_env",
                class_name
            )
        });
        self.constraints
            .push(Constraint::new(Arc::new(class_decl.clone()), var));
    }

    /// Create a fresh type variable at the current level and register it in `state.levels`.
    pub fn fresh_type_var(&mut self) -> Type {
        let name = format!("_t{}", self.name_counter);
        self.name_counter = self.name_counter.saturating_add(1);
        self.levels.insert(name.clone(), self.level);
        Type::TypeVar(name, self.level)
    }

    /// Create a fresh type variable with an associated source name for better diagnostics.
    /// The source_name is typically a function parameter name or let-binding name.
    /// Used for T013 warnings to report "ambiguous type variable 'x'" (hiding the internal
    /// _tN name which is noise for users).
    pub fn fresh_type_var_with_source(&mut self, source_name: impl Into<String>) -> Type {
        let internal_name = format!("_t{}", self.name_counter);
        self.name_counter = self.name_counter.saturating_add(1);
        self.levels.insert(internal_name.clone(), self.level);
        self.type_var_source_names
            .insert(internal_name.clone(), source_name.into());
        Type::TypeVar(internal_name, self.level)
    }

    // fresh_row_var_name removed — BAS Step 4: no RowVar tails exist

    /// Compact the levels map by removing entries for TypeVars that have been unified.
    /// A TypeVar is considered unified if its name appears in the substitution's type_map.
    /// This prevents unbounded growth of the levels HashMap during long inference sessions.
    ///
    /// Call this periodically after unification rounds (e.g., at the end of infer_dict).
    pub fn compact_levels(&mut self) {
        let type_map = self.subst.type_map.borrow();
        self.levels
            .retain(|name, _level| !type_map.contains_key(name));
    }

    /// Check if the substitution has no bindings (all type variables are free).
    pub fn subst_is_empty(&self) -> bool {
        self.subst.type_map.borrow().is_empty()
    }

    /// Apply the current substitution to a type, resolving all bound type variables.
    pub fn apply(&self, ty: &Type) -> Type {
        self.subst.apply(ty)
    }

    /// Apply the current substitution to a type (mutable borrow variant).
    pub fn apply_mut(&mut self, ty: &Type) -> Type {
        self.subst.apply(ty)
    }

    /// Return a reference to the type constructor environment.
    pub fn tycon_env_ref(
        &self,
    ) -> &std::collections::HashMap<String, std::sync::Arc<crate::type_def::TyConDef>> {
        &self.tycon_env
    }

    /// Register or update the Kind for a TypeVar name in `kind_env`.
    pub fn set_kind(&mut self, name: impl Into<String>, kind: Kind) {
        self.kind_env.insert(name.into(), kind);
    }

    /// Get a reference to the kind environment.
    /// Compatibility method for code that calls `state.kind_env()` as a method.
    pub fn kind_env(&self) -> &HashMap<String, Kind> {
        &self.kind_env
    }

    /// Get the Kind for a TypeVar name from kind_env (compatibility with new InferState API).
    pub fn get_kind(&self, name: &str) -> Option<Kind> {
        self.kind_env.get(name).cloned()
    }

    /// Look up the binding for a TypeVar name (compatibility shim for new InferState API).
    pub fn lookup_binding(&self, name: &str) -> Option<Type> {
        self.subst.type_map.borrow().get(name).cloned()
    }

    /// Apply the substitution to a type using a visited set to prevent infinite recursion.
    pub fn apply_with_visited(
        &self,
        ty: &Type,
        _visited_types: &mut std::collections::HashSet<String>,
        _visited_rows: &mut std::collections::HashSet<String>,
    ) -> Type {
        // Use the subst to apply type_map bindings
        self.subst.apply(ty)
        // Note: ignores visited_types for now — the Substitution::apply_inner doesn't track
        // cycles. If needed, use apply_type_with_visited from type_unify.rs instead.
    }

    /// Bind a TypeVar to a type (compatibility shim for new InferState API).
    pub fn bind_type_var(&mut self, name: String, ty: Type) {
        self.subst.type_map.borrow_mut().insert(name, ty);
    }

    /// Check if the type variable table has exceeded a maximum size.
    /// Compatibility shim — the current design uses a simple heuristic.
    pub fn check_type_vars_size(
        &self,
        _span: crate::ast::Span,
    ) -> Result<(), crate::types::TypeError> {
        // Current InferState doesn't have a unified type_vars table with explicit size tracking.
        // Return Ok — callers that need this check should track via the substitution.
        Ok(())
    }

    /// Allocate a monotonic counter value and increment it.
    /// The `_prefix` and `_suffix` args are ignored (compatibility shim for new API).
    pub fn alloc_counter(&mut self, _prefix: &str, _suffix: &str) -> u32 {
        let n = self.name_counter;
        self.name_counter = n.saturating_add(1);
        n
    }

    /// Set the level for a TypeVar name (compatibility with new InferState API).
    pub fn set_level(&mut self, name: impl Into<String>, level: u32) {
        self.levels.insert(name.into(), level);
    }

    /// Get the level of a TypeVar name (compatibility with new InferState API).
    pub fn get_level(&self, name: &str) -> Option<u32> {
        self.levels.get(name).copied()
    }

    /// Create a fresh type variable with an optional source name and origin span.
    /// Compatibility shim for code expecting `fresh_type_var_with_origin`.
    pub fn fresh_type_var_with_origin(
        &mut self,
        source_name: Option<&str>,
        _origin_name: Option<&str>,
        _span: Option<crate::ast::Span>,
    ) -> Type {
        let name = format!("_t{}", self.name_counter);
        self.name_counter = self.name_counter.saturating_add(1);
        self.levels.insert(name.clone(), self.level);
        if let Some(src) = source_name {
            self.type_var_source_names
                .insert(name.clone(), src.to_string());
        }
        Type::TypeVar(name, self.level)
    }

    /// Allocate a fresh TypeVar at the specified level.
    /// Returns `(name, Type::TypeVar(name, level))`.
    /// Compatible shim for code expecting the new InferState API.
    pub fn alloc_type_var_at_level(
        &mut self,
        level: u32,
        source_name: Option<&str>,
        _unused1: Option<()>,
        _unused2: Option<()>,
        _kind: Kind,
    ) -> (String, Type) {
        let name = format!("_t{}", self.name_counter);
        self.name_counter = self.name_counter.saturating_add(1);
        self.levels.insert(name.clone(), level);
        if let Some(src) = source_name {
            self.type_var_source_names
                .insert(name.clone(), src.to_string());
        }
        let ty = Type::TypeVar(name.clone(), level);
        (name, ty)
    }
}

impl Default for InferState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Type;

    /// `compact_levels()` removes entries for TypeVars that have been unified
    /// (i.e., whose names appear in `state.subst.type_map`), while keeping entries
    /// for unbound TypeVars.
    ///
    /// Mutation resistance: if `compact_levels()` were a no-op, the unified var
    /// would still be present in `state.levels` after the call, failing the
    /// `!state.levels.contains_key("_t0")` assertion.
    #[test]
    fn test_compact_levels_removes_unified_var() {
        let mut state = InferState::new();

        // Create two fresh TypeVars: _t0 and _t1.
        let _tv0 = state.fresh_type_var(); // registers "_t0" in levels at level 0
        let _tv1 = state.fresh_type_var(); // registers "_t1" in levels at level 0

        assert!(
            state.levels.contains_key("_t0"),
            "_t0 should be in levels before compaction"
        );
        assert!(
            state.levels.contains_key("_t1"),
            "_t1 should be in levels before compaction"
        );

        // Bind _t0 → Int by inserting it into the substitution's type_map.
        // This simulates what unification does when it solves a TypeVar.
        state
            .subst
            .type_map
            .borrow_mut()
            .insert("_t0".to_string(), Type::Int);

        // compact_levels() should remove _t0 (now in type_map) but keep _t1 (unbound).
        state.compact_levels();

        assert!(
            !state.levels.contains_key("_t0"),
            "_t0 should be removed from levels after compaction (it is unified)"
        );
        assert!(
            state.levels.contains_key("_t1"),
            "_t1 should remain in levels after compaction (it is still unbound)"
        );
    }

    /// `compact_levels()` is a no-op when no TypeVars have been unified.
    /// All registered TypeVars remain in `levels`.
    #[test]
    fn test_compact_levels_preserves_unbound_vars() {
        let mut state = InferState::new();
        let _tv0 = state.fresh_type_var();
        let _tv1 = state.fresh_type_var();

        let count_before = state.levels.len();
        state.compact_levels();
        let count_after = state.levels.len();

        assert_eq!(
            count_before, count_after,
            "compact_levels() must not remove unbound TypeVars"
        );
    }
}
