//! Type inference machinery: InferState, generalization, instantiation.
//!
//! This module contains the core type inference infrastructure including
//! the unified TypeVar table and levels-based let-generalization (Kiselyov 2013).

use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::ast::Span;
use crate::type_def::TyConDef;
use crate::types::{ClassEnv, Constraint, InstanceEnv, Kind, Type};

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
    /// Per-parameter narrowing types for type predicates.
    /// When param i has an `@[is: T]` annotation in its declaration,
    /// param_narrowings[i] = Some(T). This enables any predicate function
    /// to narrow its argument's type in the then-branch of an if, without
    /// hardcoding function names in the type checker.
    /// Empty vec means no narrowing annotations.
    pub param_narrowings: Vec<Option<Type>>,
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
            param_narrowings: Vec::new(),
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
    /// Unified TypeVar table: creation level, binding, and kind for each TypeVar.
    /// IndexMap preserves insertion order (TypeVar creation order) for deterministic iteration.
    pub type_vars: IndexMap<String, TypeVarEntry>,
    /// Counter for fresh TypeVar names. Previously Substitution.name_counter.
    pub name_counter: u32,
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
    // boundary_guards removed: type guards are now written inline on SurfaceNode.type_guard
    // (TypeAnnotation OnceLock) by the type checker, and the lowerer wraps them in
    // CoreExpr::TypeAssert during lowering. No side-channel HashMap needed.
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
    // do_infer_resolutions removed: was never populated from any pipeline path.
    // The evaluator's CoreExpr::Var path handles do-infer sentinels via normal variable lookup.
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
    /// Instance binding name → slot within its definition scope frame.
    /// Populated from the resolver's `instance_binding_slots` export in the eval pipeline.
    /// Used by the type checker to write `call_dispatch` coordinates when dispatching a
    /// typeclass method call: level comes from the class method stub VarRef's resolution,
    /// slot comes from this map. Empty in typecheck-only paths (no lowering, so unused).
    pub instance_binding_slots: std::collections::HashMap<String, u32>,
    // type_annotation_table removed: TypeAssert types and call_dispatch are now written
    // inline on AST nodes (TypeAnnotation OnceLock fields) rather than accumulated here.
    /// Resolved types for pipeline `expects:` contracts, keyed by the expects annotation's span.
    /// When a document has `--- expects: TypeExpr`, the typecheck pass resolves TypeExpr and stores
    /// the result here. The eval pipeline reads this map to populate TypeAssert.resolved_type,
    /// enabling structural type checking via is_consistent_subtype.
    pub expects_resolved: HashMap<crate::ast::Span, crate::types::Type>,
    /// Active type parameter scope for TypeAlias body resolution (T-951).
    ///
    /// When `Some(params)`: `resolve_type_name` enforces that lowercase names are TypeVars
    /// Type-stage evaluation environment for this inference pass.
    /// When `Some(env)`, `eval_type_stage_expr` uses this env to evaluate type-stage expressions.
    /// When `None`, type-stage evaluation fails with a type error (no fallback).
    ///
    /// Set by the TypeContext system during stdlib/prelude loading. The type-stage env
    /// contains type-level builtins (TypeNode, object-map, etc.) but no IO/caps/runtime API.
    pub type_stage_env: Option<Arc<RwLock<crate::value::Environment>>>,
    /// Main runtime environment from the eval pipeline. When set, `eval_type_stage_expr`
    /// chains this as the parent of the type-stage env so that type constructor names
    /// (type constructors, etc.) defined in the main env are visible during annotation evaluation.
    /// This is the cross-stage bridge: type-stage can resolve types from the main env.
    /// Only set in the eval path (builtin_eval_types); None in typecheck-only paths.
    pub main_env: Option<Arc<RwLock<crate::value::Environment>>>,
    /// Pending method scheme injections from `infer_class_decl_from_surface`.
    ///
    /// When a `[class ...]` declaration is processed, `infer_class_decl_from_surface` builds
    /// a `TypeScheme` for each method signature and pushes them here instead of returning them
    /// in the return value. The caller drains this vec after each class declaration and injects
    /// the schemes into the active `TypeEnv` (the document `env` or `dict_env`).
    ///
    /// This ensures method schemes are always visible to subsequent entries regardless of
    /// which call path reaches `infer_class_decl_from_surface`, including class declarations
    /// in dict-entry position (infer_dict Pass 0c) and in top-level document position
    /// (typecheck_surface_document).
    pub pending_scheme_injections: Vec<(String, crate::types::TypeScheme)>,
    /// Expansion stack for named type alias cycle detection (equirecursive types).
    ///
    /// Shared across `expand_named` calls within a single top-level inference pass.
    /// `expand_named` pushes an entry when it begins expanding a named alias and pops
    /// it when it finishes. If the same `Arc<TyConDef>` is already on the stack (tested
    /// via `Arc::ptr_eq`), the alias is recursive and `expand_named` returns a `TypeVar`
    /// sentinel without recursing further.
    ///
    /// Always empty between top-level declarations. Cycles are safe to unwind because
    /// the stack is borrowed mutably through the entire expansion call tree — any attempt
    /// to re-enter a stack entry produces the TypeVar sentinel rather than infinite recursion.
    pub expansion_stack: crate::typecheck::typecheck_annot::ExpansionStack,
    /// Pending param narrowings from the most recently inferred function.
    /// When `infer_fn` processes a function with `@[is: T]` annotations on parameters,
    /// it stores the narrowing types here. The caller (e.g., `infer_dict`) consumes this
    /// vec and attaches it to the TypeScheme. Reset to empty after consumption.
    pub pending_param_narrowings: Vec<Option<Type>>,
    /// BAS TypeVar bounds: lower and upper bounds accumulated by C-Var1/2 constraint
    /// rewriting during unification. Each TypeVar name maps to its TypeVarBounds.
    ///
    /// - C-Var1 (`τ₁ ≤ τ₂ ∨ α` → `τ₁ & ~τ₂ ≤ α`): adds `τ₁ & ~τ₂` as lower bound of α
    /// - C-Var2 (`α ∧ τ₁ ≤ τ₂` → `α ≤ ~τ₁ ∨ τ₂`): adds `~τ₁ ∨ τ₂` as upper bound of α
    ///
    /// At generalization time, bounds are compacted: if they determine a unique type for α,
    /// α is substituted; otherwise α remains free in the TypeScheme.
    ///
    /// Coexists with type_vars bindings — type_vars handles equational bindings (α = T),
    /// bounds handle inequality constraints (L ≤ α ≤ U).
    pub bounds: std::collections::HashMap<String, crate::bas::TypeVarBounds>,
}

impl InferState {
    pub fn new() -> Self {
        // class_env and instance_env start empty. They are populated by callers via:
        //   state.class_env = env.build_class_env();
        //   state.instance_env = env.build_instance_env();
        // TypeEnv is the canonical persistent store for class/instance declarations.
        // Prelude is responsible for declaring its own classes (Indexable, Concatable, etc.)
        // via [class ...] and [instance ...] forms — Rust does not hardcode them.
        let class_env = ClassEnv::new();
        let instance_env = InstanceEnv::new();

        // Builtin type constructors — pre-registered in type_vars so resolve_type_dict
        // uses the general kind lookup path instead of string matching.
        let mut type_vars = IndexMap::new();
        type_vars.insert("Seq".to_string(), TypeVarEntry {
            level: 0,
            binding: None,
            kind: Kind::Operator,
        });
        type_vars.insert("Map".to_string(), TypeVarEntry {
            level: 0,
            binding: None,
            kind: Kind::Arrow(Box::new(Kind::Type), Box::new(Kind::Operator)),
        });
        type_vars.insert("Handle".to_string(), TypeVarEntry {
            level: 0,
            binding: None,
            kind: Kind::Operator,
        });

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
                constructor_constants: indexmap::IndexMap::new(),
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
                constructor_constants: indexmap::IndexMap::new(),
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
                constructor_constants: indexmap::IndexMap::new(),
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
        // bare TypeConstructor leaf without structural expansion (same treatment as builtin TyCons).
        // `body` holds the concrete primitive `Type` — not `Unknown` — so that callers which read
        // `TyConDef.body` directly (e.g., type display) see the correct underlying type.
        // `params: vec![]` — zero type parameters; `variance: vec![]` — no parameters to vary.
        for (name, body) in [
            ("Int", crate::type_def::Type::Int),
            ("Float", crate::type_def::Type::Float),
            // User annotation name is "String"; runtime alias is "Str". Both names resolve
            // to Type::Str via resolve_type_name; we register the canonical annotation name.
            ("String", crate::type_def::Type::Str),
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
                    constructor_constants: indexmap::IndexMap::new(),
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
                    fields: indexmap::IndexMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }),
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: Some("Absent".to_string()),
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
            }),
        );

        Self {
            level: 0,
            type_vars,
            name_counter: 0,
            class_env,
            instance_env,
            failed_bindings: HashMap::new(),
            scheme_map: None,
            current_function: None,
            expected_return: None,
            diagnostics: Vec::new(),
            deferred_equalities: Vec::new(),
            tycon_env,
            fd_depth: 0,
            fd_in_progress: std::collections::HashSet::new(),
            instance_resolution_depth: 0,
            type_var_source_names: HashMap::new(),
            t013_emitted: std::collections::HashSet::new(),
            registered_nominal_tags: HashMap::new(),
            instance_binding_slots: std::collections::HashMap::new(),
            main_env: None,
            expects_resolved: HashMap::new(),
            type_stage_env: None,
            pending_scheme_injections: Vec::new(),
            expansion_stack: Vec::new(),
            pending_param_narrowings: Vec::new(),
            bounds: HashMap::new(),
        }
    }

    /// Add a type class constraint to the given constraint accumulator.
    /// The constraint is checked during instantiation.
    ///
    /// The class_name must be registered in class_env. If not found, this will panic
    /// (the caller is responsible for validating class existence before calling).
    pub fn add_constraint(
        &self,
        constraints: &mut Vec<Constraint>,
        class_name: impl Into<String>,
        var: impl Into<String>,
    ) {
        let class_name = class_name.into();
        let class_decl = self.class_env.get(&class_name).unwrap_or_else(|| {
            panic!(
                "add_constraint: class '{}' not registered in class_env",
                class_name
            )
        });
        constraints.push(Constraint::new(Arc::new(class_decl.clone()), var));
    }

    /// Create a fresh type variable at the current level and register it in `state.type_vars`.
    ///
    /// TypeVar names are globally unique via the monotonic `name_counter` (Barendregt convention).
    pub fn fresh_type_var(&mut self) -> Type {
        let n = self.name_counter;
        self.name_counter = n.saturating_add(1);
        let name = format!("_t{}", n);
        self.type_vars.insert(name.clone(), TypeVarEntry {
            level: self.level,
            binding: None,
            kind: Kind::Type,
        });
        Type::TypeVar(name, self.level)
    }

    /// Create a fresh type variable with an associated source name for better diagnostics.
    /// The source_name is typically a function parameter name or let-binding name.
    /// Used for T013 warnings to report "ambiguous type variable 'x'" (hiding the internal
    /// _tN name which is noise for users).
    pub fn fresh_type_var_with_source(&mut self, source_name: impl Into<String>) -> Type {
        let n = self.name_counter;
        self.name_counter = n.saturating_add(1);
        let internal_name = format!("_t{}", n);
        self.type_vars.insert(internal_name.clone(), TypeVarEntry {
            level: self.level,
            binding: None,
            kind: Kind::Type,
        });
        self.type_var_source_names
            .insert(internal_name.clone(), source_name.into());
        Type::TypeVar(internal_name, self.level)
    }

    /// Compact the type_vars map by removing entries for TypeVars that have been bound.
    /// This prevents unbounded growth during long inference sessions.
    ///
    /// Call this periodically after unification rounds (e.g., at the end of infer_dict).
    pub fn compact_levels(&mut self) {
        self.type_vars.retain(|_name, entry| entry.binding.is_none());
    }

    /// Look up a TypeVar binding, following chains. Equivalent to old Substitution::apply for a single var.
    pub fn lookup_binding(&self, name: &str) -> Option<Type> {
        self.type_vars.get(name).and_then(|e| e.binding.clone())
    }

    /// Bind a TypeVar to a type in the unified table.
    pub fn bind_type_var(&mut self, name: String, ty: Type) {
        if let Some(entry) = self.type_vars.get_mut(&name) {
            entry.binding = Some(ty);
        } else {
            // TypeVar not yet registered (e.g., from annotation); register at level 0
            self.type_vars.insert(name, TypeVarEntry {
                level: 0,
                binding: Some(ty),
                kind: Kind::Type,
            });
        }
    }

    /// Get the level of a TypeVar.
    pub fn get_level(&self, name: &str) -> Option<u32> {
        self.type_vars.get(name).map(|e| e.level)
    }

    /// Set the level of a TypeVar. If not registered, inserts it.
    pub fn set_level(&mut self, name: String, level: u32) {
        if let Some(entry) = self.type_vars.get_mut(&name) {
            entry.level = level;
        } else {
            self.type_vars.insert(name, TypeVarEntry {
                level,
                binding: None,
                kind: Kind::Type,
            });
        }
    }

    /// Get the kind of a TypeVar.
    pub fn get_kind(&self, name: &str) -> Option<&Kind> {
        self.type_vars.get(name).map(|e| &e.kind)
    }

    /// Set the kind of a TypeVar. If not registered, inserts it.
    pub fn set_kind(&mut self, name: String, kind: Kind) {
        if let Some(entry) = self.type_vars.get_mut(&name) {
            entry.kind = kind;
        } else {
            self.type_vars.insert(name, TypeVarEntry {
                level: self.level,
                binding: None,
                kind,
            });
        }
    }

    /// Check if the type_vars map has exceeded the maximum allowed size.
    pub fn check_type_vars_size(&self, span: Span) -> Result<(), crate::type_errors::TypeErrorTyped> {
        let len = self.type_vars.len();
        if len > MAX_TYPE_VARS_SIZE {
            Err(crate::type_errors::TypeErrorTyped::Generic(crate::type_errors::GenericTypeError {
                message: format!(
                    "type inference resource limit exceeded (type_vars size {} > {}) -- use fewer chained dot-accesses or add explicit type annotations to break constraint chains",
                    len, MAX_TYPE_VARS_SIZE
                ),
                span,
                notes: vec![], call_stack: vec![],
            }))
        } else {
            Ok(())
        }
    }

    /// Check if the substitution is empty (no bindings).
    pub fn subst_is_empty(&self) -> bool {
        self.type_vars.values().all(|e| e.binding.is_none())
    }

    /// Apply substitution to a type: resolve all bound TypeVars.
    pub fn apply(&self, ty: &Type) -> Type {
        crate::types::apply_substitution(ty, &self.type_vars)
    }

    /// Apply substitution with an externally-supplied visited set.
    pub fn apply_with_visited(
        &self,
        ty: &Type,
        visited_types: &mut std::collections::HashSet<String>,
        _visited_rows: &mut std::collections::HashSet<String>,
    ) -> Type {
        crate::types::apply_type_with_visited(ty, &self.type_vars, 0, visited_types).into_owned()
    }

    /// Build a HashMap<String, Type> of all currently bound TypeVars.
    /// Used by `emit_ambiguous_constraint_diagnostics` which needs a binding snapshot.
    pub fn binding_snapshot(&self) -> HashMap<String, Type> {
        self.type_vars
            .iter()
            .filter_map(|(name, entry)| {
                entry
                    .binding
                    .as_ref()
                    .map(|ty| (name.clone(), ty.clone()))
            })
            .collect()
    }

    /// Build a HashMap<String, Kind> view of type_vars for callsites that need it
    /// (e.g., check_kind_wellformed). Only includes entries with non-default kinds
    /// (Kind::Label, Kind::Operator) since Kind::Type is the default and omitting it
    /// matches the old kind_env semantics (only non-Type kinds were registered).
    pub fn kind_env(&self) -> HashMap<String, Kind> {
        self.type_vars
            .iter()
            .filter(|(_, entry)| entry.kind != Kind::Type)
            .map(|(name, entry)| (name.clone(), entry.kind.clone()))
            .collect()
    }
}

impl Default for InferState {
    fn default() -> Self {
        Self::new()
    }
}

/// Maximum size of the type_vars map.
/// Prevents resource exhaustion from quadratic growth in pathological cases.
pub const MAX_TYPE_VARS_SIZE: usize = 50_000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Type;

    /// `compact_levels()` removes entries for TypeVars that have been bound,
    /// while keeping entries for unbound TypeVars.
    #[test]
    fn test_compact_levels_removes_unified_var() {
        let mut state = InferState::new();

        // Create two fresh TypeVars: _t0 and _t1.
        let _tv0 = state.fresh_type_var();
        let _tv1 = state.fresh_type_var();

        assert!(
            state.type_vars.contains_key("_t0"),
            "_t0 should be in type_vars before compaction"
        );
        assert!(
            state.type_vars.contains_key("_t1"),
            "_t1 should be in type_vars before compaction"
        );

        // Bind _t0 -> Int by setting its binding.
        state.bind_type_var("_t0".to_string(), Type::Int);

        // compact_levels() should remove _t0 (now bound) but keep _t1 (unbound).
        state.compact_levels();

        assert!(
            !state.type_vars.contains_key("_t0"),
            "_t0 should be removed from type_vars after compaction (it is bound)"
        );
        assert!(
            state.type_vars.contains_key("_t1"),
            "_t1 should remain in type_vars after compaction (it is still unbound)"
        );
    }

    /// `compact_levels()` is a no-op when no TypeVars have been unified.
    /// All registered TypeVars remain in `type_vars`.
    #[test]
    fn test_compact_levels_preserves_unbound_vars() {
        let mut state = InferState::new();
        let _tv0 = state.fresh_type_var();
        let _tv1 = state.fresh_type_var();

        // Subtract 3 for the builtin TyCon entries
        let count_before = state.type_vars.len();
        state.compact_levels();
        let count_after = state.type_vars.len();

        assert_eq!(
            count_before, count_after,
            "compact_levels() must not remove unbound TypeVars"
        );
    }
}
