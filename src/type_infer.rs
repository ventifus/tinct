//! Type inference machinery: InferState, Substitution, generalization, instantiation.
//!
//! This module contains the core type inference infrastructure including
//! substitution and levels-based let-generalization (Kiselyov 2013).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use crate::ast::Span;
use crate::type_def::TyConDef;
use crate::types::{ClassDecl, ClassEnv, Constraint, InstanceEnv, Kind, Type};

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
    pub level: u32,
    pub levels: HashMap<String, u32>,
    /// Global accumulated substitution: collects constraints from access-chain inference
    /// and other constraint generators. Applied when resolving type variables during
    /// inference, so that constraints from `$x.field1` are visible when processing
    /// `$x.field2` in the same expression. See doc/07-type-extensions.md Part 5.
    pub subst: Substitution,
    /// Accumulated type class constraints on type variables.
    /// Constraints are generated when overloaded builtins are called with type variables.
    ///
    /// **Scoping contract**: On the `infer_dict` path, `state.constraints` is cleared via
    /// `std::mem::take` after each entry's inference. The collected constraints are stored
    /// per-entry in `typecheck_dict.rs` and passed explicitly to `generalize_with_doc` —
    /// `generalize_with_doc` does NOT read this field. This field is used by coherence probes
    /// in `type_class.rs` (which save/restore it around probes) and by `type_unify.rs`
    /// (constraint transfer during TypeVar→TypeVar binding).
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
    /// Lexically scoped: inner instances shadow outer, frame-local coherence enforced.
    /// See `InstanceEnv` in `src/type_class.rs`.
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
    /// Type constructor environment: maps type constructor names to their definitions.
    /// Populated by the type checker when processing `[type ...]` declarations (T-942).
    /// Present here so that TyConDef is "used" and to allow type-checking passes to
    /// propagate TyCon definitions through the inference pipeline.
    pub tycon_env: HashMap<String, Arc<TyConDef>>,
    /// Current functional dependency improvement recursion depth.
    /// Prevents infinite loops through the improve_functional_dependency → unify →
    /// check_constraints_on_var → improve_functional_dependency cycle. Incremented when
    /// entering improve_functional_dependency, decremented when exiting.
    pub fd_depth: usize,
    /// Set of TypeVar names currently being processed by FD improvement (either forward or
    /// reverse direction). When FD improvement tries to bind a determined/determining variable
    /// that is already in this set, it skips the unification — the variable is already being
    /// bound in an outer FD improvement call, so re-binding it would be a no-op (idempotent)
    /// and attempting it causes the mutual-recursion cycle: reverse(t1→t0) → forward(t0→t1)
    /// → reverse(t1→t0) → … hitting MAX_FD_DEPTH.
    pub fd_in_progress: std::collections::HashSet<String>,
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
    /// Active type parameter scope for TypeAlias body resolution (T-951).
    ///
    /// When `Some(params)`: `resolve_type_name` enforces that lowercase names are TypeVars
    /// ONLY if they are in `params`. Unknown lowercase names are a type error rather than
    /// silently creating a fresh TypeVar. This implements the "explicit type params" requirement
    /// from `doc/whatif/user-type-constructors.md` §Unified [type ...] Syntax rule 1.
    ///
    /// Set to `Some(param_names)` before resolving a TypeAlias body and cleared immediately after.
    /// All other inference code leaves this as `None` (no scope enforcement).
    ///
    /// TODO(T-1022): Refactor to explicit parameter threaded through resolve_annotation,
    /// resolve_type_expr, and resolve_type_dict instead of mutable state.
    pub type_params_scope: Option<std::collections::HashSet<String>>,
    /// Type-stage evaluation environment extending the prelude type-stage env with user file's
    /// type-stage sections. When `Some(env)`, `eval_type_stage_expr` uses this env instead of
    /// calling `build_type_stage_env()` (which only returns prelude bindings). This allows user
    /// files to define type-stage functions in `--- stage: type` sections and use them in
    /// annotations in runtime sections (T-1175).
    ///
    /// Set by `typecheck_surface_program_with_env` after evaluating the file's type-stage
    /// documents. `None` when type-stage env building fails or when no type-stage sections exist.
    pub type_stage_env: Option<Arc<RwLock<crate::value::Environment>>>,
}

impl InferState {
    pub fn new() -> Self {
        let mut class_env = ClassEnv::new();

        // Register built-in type classes with their superclass relationships.
        // Class declarations define the class hierarchy (which classes extend which).
        // Instance resolution goes through InstanceEnv::resolve_instance for all classes.
        // Primitive instances (Equatable Int, Comparable Float, etc.) are pre-seeded below
        // using primitive_satisfies_constraint (type_def.rs) as the single authoritative table.
        //
        // These pre-registrations ensure the class hierarchy is available before prelude.llt
        // is type-checked. Prelude.llt registers additional instances which are merged via
        // seed_infer_state_from_prelude_cache after the prelude loads.

        // Equatable: base class (primitive instances pre-seeded below; prelude also declares instances)
        class_env.insert(ClassDecl {
            name: "Equatable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Numeric: extends Equatable (primitive instances pre-seeded below for Int/Float/Number)
        class_env.insert(ClassDecl {
            name: "Numeric".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![("Equatable".to_string(), vec!["a".to_string()])],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
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
        });

        // Comparable: extends Equatable (primitive instances pre-seeded below; prelude also declares instances)
        class_env.insert(ClassDecl {
            name: "Comparable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![("Equatable".to_string(), vec!["a".to_string()])],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Showable: base class (primitive instances pre-seeded below; prelude also declares instances)
        class_env.insert(ClassDecl {
            name: "Showable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Mappable: base class (instances defined in prelude.llt; no primitive pre-seeding needed)
        // Kind::Operator for higher-kinded type constructor polymorphism
        class_env.insert(ClassDecl {
            name: "Mappable".to_string(),
            params: vec![("f".to_string(), Kind::Operator)],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Appendable: base class. Seq instance pre-seeded below.
        // Str handled via primitive_satisfies_constraint; Record via fast-path in satisfies_constraint_inner.
        // Prelude declares additional Appendable instances.
        class_env.insert(ClassDecl {
            name: "Appendable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
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
        });

        // Concatable: 3-parameter type class with functional dependency (a, b) → c
        // Models concatenation: Seq(T)++Seq(T)→Seq(T), Record++Record→Record,
        // Str++Str→Str, Bytes++Bytes→Bytes.
        // Built-in instances registered below in instance_env.
        class_env.insert(ClassDecl {
            name: "Concatable".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            determines: vec![(vec![0, 1], vec![2])], // (a, b) → c
            resolver: None,
            resolver_injective: false,
        });

        let mut instance_env = InstanceEnv::new();

        // Register built-in Indexable instances for Map, Seq, and Record.
        // These instances enable FD improvement: given container and key types,
        // the value type is determined automatically.
        use crate::type_class::InstanceDecl;
        use crate::types::Row;

        // Indexable Map[K V] K V
        // For a Map with key type K and value type V, indexing by K returns V.
        let map_k_var = Type::TypeVar("K".to_string(), 0);
        let map_v_var = Type::TypeVar("V".to_string(), 0);
        let map_instance = InstanceDecl {
            class_name: "Indexable".to_string(),
            instance_type: Type::Record(Row {
                fields: {
                    let mut fields = BTreeMap::new();
                    fields.insert(
                        "0".to_string(),
                        Type::map(map_k_var.clone(), map_v_var.clone()),
                    );
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
        // For a Seq with element type T, indexing by Int returns T.
        let seq_t_var = Type::TypeVar("T".to_string(), 0);
        let seq_instance = InstanceDecl {
            class_name: "Indexable".to_string(),
            instance_type: Type::Record(Row {
                fields: {
                    let mut fields = BTreeMap::new();
                    fields.insert("0".to_string(), Type::seq(seq_t_var.clone()));
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

        // ── Concatable instances ──────────────────────────────────────────────
        // Concatable Seq[T] Seq[T] Seq[T]
        // Concatenating two sequences of the same element type produces a sequence of that type.
        let concat_t_var = Type::TypeVar("T".to_string(), 0);
        let concatable_seq_instance = InstanceDecl {
            class_name: "Concatable".to_string(),
            instance_type: Type::Record(Row {
                fields: {
                    let mut fields = BTreeMap::new();
                    fields.insert("0".to_string(), Type::seq(concat_t_var.clone()));
                    fields.insert("1".to_string(), Type::seq(concat_t_var.clone()));
                    fields.insert("2".to_string(), Type::seq(concat_t_var.clone()));
                    fields
                },
                tail: crate::type_def::RowTail::Empty,
            }),
            det_positions: vec![0, 1],
            method_types: HashMap::new(),
        };
        instance_env.insert(concatable_seq_instance).unwrap();

        // Concatable Record Record Record
        // Concatenating (merging) two open records produces an open record.
        let concatable_record_instance = InstanceDecl {
            class_name: "Concatable".to_string(),
            instance_type: Type::Record(Row {
                fields: {
                    let mut fields = BTreeMap::new();
                    fields.insert(
                        "0".to_string(),
                        Type::Record(Row {
                            fields: BTreeMap::new(),
                            tail: crate::type_def::RowTail::Empty,
                        }),
                    );
                    fields.insert(
                        "1".to_string(),
                        Type::Record(Row {
                            fields: BTreeMap::new(),
                            tail: crate::type_def::RowTail::Empty,
                        }),
                    );
                    fields.insert(
                        "2".to_string(),
                        Type::Record(Row {
                            fields: BTreeMap::new(),
                            tail: crate::type_def::RowTail::Empty,
                        }),
                    );
                    fields
                },
                tail: crate::type_def::RowTail::Empty,
            }),
            det_positions: vec![0, 1],
            method_types: HashMap::new(),
        };
        instance_env.insert(concatable_record_instance).unwrap();

        // Concatable Str Str Str
        // Concatenating two strings produces a string.
        let concatable_str_instance = InstanceDecl {
            class_name: "Concatable".to_string(),
            instance_type: Type::Record(Row {
                fields: {
                    let mut fields = BTreeMap::new();
                    fields.insert("0".to_string(), Type::Str);
                    fields.insert("1".to_string(), Type::Str);
                    fields.insert("2".to_string(), Type::Str);
                    fields
                },
                tail: crate::type_def::RowTail::Empty,
            }),
            det_positions: vec![0, 1],
            method_types: HashMap::new(),
        };
        instance_env.insert(concatable_str_instance).unwrap();

        // Concatable Bytes Bytes Bytes
        // Concatenating two byte strings produces a byte string.
        let concatable_bytes_instance = InstanceDecl {
            class_name: "Concatable".to_string(),
            instance_type: Type::Record(Row {
                fields: {
                    let mut fields = BTreeMap::new();
                    fields.insert("0".to_string(), Type::Bytes);
                    fields.insert("1".to_string(), Type::Bytes);
                    fields.insert("2".to_string(), Type::Bytes);
                    fields
                },
                tail: crate::type_def::RowTail::Empty,
            }),
            det_positions: vec![0, 1],
            method_types: HashMap::new(),
        };
        instance_env.insert(concatable_bytes_instance).unwrap();

        // ── Parametric structural class instances ─────────────────────────────────────────────
        // Pre-seed InstanceEnv with parametric structural instances only. Primitive leaf
        // instances (Equatable Int, Comparable Float, Showable Str, etc.) are NOT pre-seeded
        // here — they are handled exclusively by the `primitive_satisfies_constraint` fast path
        // in `satisfies_constraint_inner` (type_unify.rs), which short-circuits before any
        // InstanceEnv lookup. Pre-seeding primitives would cause `check_structural_overlap` to
        // flag conflicts when user code declares instances of the same class for the same types
        // (e.g., `[instance Equatable [pattern [Int]]]` in corpus tests).
        //
        // Parametric structural instances DO need InstanceEnv entries because
        // `satisfies_constraint_inner` calls `resolve_instance` for compound types like
        // `Seq[concrete]` when the structural fast path does not apply.
        //
        // `InstanceEnv::insert` is idempotent (string-key dedup): re-seeding from
        // prelude cache via `seed_infer_state_from_prelude_cache` is safe.

        // Scoped block: seed_instance closure borrows instance_env mutably; the block
        // ensures the closure is dropped before instance_env is moved into Self { ... }.
        {
            // Helper: register a simple single-param instance (det_positions empty = single-param).
            let mut seed_instance = |class: &str, ty: Type| {
                instance_env
                    .insert(InstanceDecl {
                        class_name: class.to_string(),
                        instance_type: ty,
                        det_positions: vec![],
                        method_types: HashMap::new(),
                    })
                    .unwrap();
            };

            // Parametric structural instances: these require TypeVar patterns and cannot be
            // expressed via primitive_satisfies_constraint (which only covers leaf types).
            //
            // Showable Seq[T]: any sequence is showable (runtime has str() for all Seq)
            seed_instance("Showable", Type::seq(Type::TypeVar("T".to_string(), 0)));
            // Showable Map[K V]: any map is showable
            seed_instance(
                "Showable",
                Type::map(
                    Type::TypeVar("K".to_string(), 0),
                    Type::TypeVar("V".to_string(), 0),
                ),
            );
            // Showable Record: any record is showable (via structural propagation in
            // satisfies_constraint_inner for the fast path; this InstanceDecl covers the
            // resolve_instance path for compound record types).
            seed_instance(
                "Showable",
                Type::Record(Row {
                    fields: BTreeMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }),
            );
            // Appendable Seq[T]: concatenation of sequences of the same element type
            seed_instance("Appendable", Type::seq(Type::TypeVar("T".to_string(), 0)));
        } // seed_instance closure dropped here, releasing the mutable borrow of instance_env

        // Builtin type constructors — pre-registered so resolve_type_dict
        // uses the general kind_env path instead of string matching.
        let mut kind_env = HashMap::new();
        kind_env.insert("Seq".to_string(), Kind::Operator);
        kind_env.insert(
            "Map".to_string(),
            Kind::Arrow(Box::new(Kind::Type), Box::new(Kind::Operator)),
        );
        kind_env.insert("Handle".to_string(), Kind::Operator);

        // T-1018: Register builtin TyCons in tycon_env with their variance annotations.
        // This allows is_subtype to apply variance-directed subtyping for builtins.
        // Arc::new wraps each TyConDef so pointer identity is preserved for UNIFY-TYCON (B-343).
        let mut tycon_env = HashMap::new();
        tycon_env.insert(
            "Seq".to_string(),
            Arc::new(crate::type_def::TyConDef {
                params: vec!["a".to_string()],
                body: crate::type_def::Type::Unknown,
                constraints: vec![],
                variance: vec![crate::type_def::Variance::Covariant],
                constructors: vec![],
                builtin_type: Some("Seq".to_string()),
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
            }),
        );
        tycon_env.insert(
            "Map".to_string(),
            Arc::new(crate::type_def::TyConDef {
                params: vec!["k".to_string(), "v".to_string()],
                body: crate::type_def::Type::Unknown,
                constraints: vec![],
                variance: vec![
                    crate::type_def::Variance::Invariant,
                    crate::type_def::Variance::Covariant,
                ],
                constructors: vec![],
                builtin_type: Some("Map".to_string()),
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
            }),
        );
        tycon_env.insert(
            "Handle".to_string(),
            Arc::new(crate::type_def::TyConDef {
                params: vec!["cap".to_string()],
                body: crate::type_def::Type::Unknown,
                constraints: vec![],
                variance: vec![crate::type_def::Variance::Covariant],
                constructors: vec![],
                builtin_type: Some("Handle".to_string()),
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
            }),
        );

        // T-1068: Register primitive zero-param types as TyConDef entries.
        //
        // These are the 7 payload-free TypeNode leaf constructors. Registering them here enables
        // the future unified `expand_named` path (equirecursive-types sprint) to look up all named
        // types — primitives and structural aliases alike — through a single `tycon_env` lookup
        // instead of the `is_builtin_type_name` fast-path string-match in resolve_type_dict.
        //
        // `builtin_type: Some(name)` marks each entry as opaque: `expand_named` will return a
        // bare TypeConstructor leaf without structural expansion (same treatment as Seq/Map/Handle).
        // `body` holds the concrete primitive `Type` — not `Unknown` — so that callers which read
        // `TyConDef.body` directly (e.g., type display) see the correct underlying type.
        // `params: vec![]` — zero type parameters; `variance: vec![]` — no parameters to vary.
        for (name, body) in [
            ("Int", crate::type_def::Type::Int),
            ("Float", crate::type_def::Type::Float),
            // User annotation name is "String"; runtime alias is "Str". Both names resolve
            // to Type::Str via resolve_type_name; we register the canonical annotation name.
            ("String", crate::type_def::Type::Str),
            ("Bool", crate::type_def::Type::Bool),
            ("Unknown", crate::type_def::Type::Unknown),
            ("Never", crate::type_def::Type::Never),
        ] {
            tycon_env.insert(
                name.to_string(),
                Arc::new(crate::type_def::TyConDef {
                    params: vec![],
                    body,
                    constraints: vec![],
                    variance: vec![],
                    constructors: vec![],
                    builtin_type: Some(name.to_string()),
                    annotation: None,
                    field_annotations: indexmap::IndexMap::new(),
                }),
            );
        }
        // Absent is the empty closed record type (same as Null/[]). Registered separately because
        // its body is a Record, not a simple Type variant.
        tycon_env.insert(
            "Absent".to_string(),
            Arc::new(crate::type_def::TyConDef {
                params: vec![],
                body: crate::type_def::Type::Record(crate::type_def::Row {
                    fields: BTreeMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }),
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: Some("Absent".to_string()),
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
            }),
        );

        Self {
            level: 0,
            levels: HashMap::new(),
            subst: Substitution::new(),
            constraints: Vec::new(),
            kind_env,
            class_env,
            instance_env,
            failed_bindings: HashMap::new(),
            scheme_map: None,
            current_function: None,
            expected_return: None,
            diagnostics: Vec::new(),
            deferred_equalities: Vec::new(),
            boundary_guards: HashMap::new(),
            tycon_env,
            fd_depth: 0,
            fd_in_progress: std::collections::HashSet::new(),
            instance_resolution_depth: 0,
            in_prelude_load: false,
            do_infer_resolutions: HashMap::new(),
            type_var_source_names: HashMap::new(),
            t013_emitted: std::collections::HashSet::new(),
            registered_nominal_tags: HashMap::new(),
            type_annotation_table: crate::ast::TypeAnnotationTable::new(),
            expects_resolved: HashMap::new(),
            type_params_scope: None,
            type_stage_env: None,
        }
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
    ///
    /// The counter lives in `state.subst` (the current substitution frame). Child frames
    /// (created by `Substitution::child()`) inherit the parent counter value, so TypeVar
    /// names are globally unique across all active frames (Barendregt convention, T-927).
    /// Sibling dicts continue from the parent's counter at the time each child was created.
    pub fn fresh_type_var(&mut self) -> Type {
        let n = self.subst.name_counter.get();
        let name = format!("_t{}", n);
        self.subst.name_counter.set(n.saturating_add(1));
        self.levels.insert(name.clone(), self.level);
        Type::TypeVar(name, self.level)
    }

    /// Create a fresh type variable with an associated source name for better diagnostics.
    /// The source_name is typically a function parameter name or let-binding name.
    /// Used for T013 warnings to report "ambiguous type variable 'x'" (hiding the internal
    /// _tN name which is noise for users).
    pub fn fresh_type_var_with_source(&mut self, source_name: impl Into<String>) -> Type {
        let n = self.subst.name_counter.get();
        let internal_name = format!("_t{}", n);
        self.subst.name_counter.set(n.saturating_add(1));
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
}

impl Default for InferState {
    fn default() -> Self {
        Self::new()
    }
}

// Substitution is defined in type_unify.rs and re-exported here so that
// type_infer.rs callers can use it without a separate import.
pub use crate::types::Substitution;

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
