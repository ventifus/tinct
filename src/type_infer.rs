//! Type inference machinery: InferState, Substitution, generalization, instantiation.
//!
//! This module contains the core type inference infrastructure including
//! substitution and levels-based let-generalization (Kiselyov 2013).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::ast::Span;
use crate::types::{Constraint, Kind, Row, RowTail, Type};

/// A deferred typeclass dispatch: connects a constraint TypeVar to the call-site VarRef.
/// When check_constraints_on_var resolves the TypeVar to a concrete type, it uses this to
/// set call_dispatch on the VarRef with the resolved VarAddr (via debruijn_to_var_addr).
#[derive(Clone, Debug)]
pub struct DispatchObligation {
    /// The TypeVar name (at a determining position) that must resolve before dispatch fires.
    pub typevar_name: String,
    /// The call-site VarRef node. call_dispatch.set() is called on this.
    pub varref_node: std::sync::Arc<crate::ast::SurfaceNode>,
    /// Class name (e.g., "Addable")
    pub class_name: String,
    /// Method name (e.g., "+")
    pub method_name: String,
    /// Instantiated constraint vars (all positions). Applied with state.subst at resolution time.
    pub constraint_vars: Vec<crate::type_class::ConstraintArg>,
    /// Determining positions from the class's functional dependency.
    /// For single-param classes with no FD: use vec![0] (all positions are determining).
    pub det_positions: Vec<usize>,
}

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
            Type::Var(name, _) => {
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
                typed_variadics,
                rest,
                required_count,
            } => Type::Function {
                params: params
                    .iter()
                    .map(|(n, t)| (n.clone(), Self::apply_inner(t, map)))
                    .collect(),
                typed_variadics: typed_variadics
                    .iter()
                    .map(|(n, t)| (n.clone(), Self::apply_inner(t, map)))
                    .collect(),
                rest: rest
                    .as_ref()
                    .map(|boxed| Box::new((boxed.0.clone(), Self::apply_inner(&boxed.1, map)))),
                ret: Box::new(Self::apply_inner(ret, map)),
                required_count: *required_count,
            },
            // App(f, a): type constructor application — substitute into both sides.
            // App(TyCon("Seq"), TypeVar("_t0")) with {_t0 → Int} → App(TyCon("Seq"), Int).
            Type::App(f, a) => Type::App(
                Box::new(Self::apply_inner(f, map)),
                Box::new(Self::apply_inner(a, map)),
            ),
            // Recursive(μvar.body): substitute through the body WITHOUT touching the binder name.
            // The binder is a μ-name (not a type variable in the substitution domain),
            // so it must be left unchanged. Only free type variables inside the body are substituted.
            Type::Recursive { var, body } => Type::Recursive {
                var: var.clone(),
                body: Box::new(Self::apply_inner(body, map)),
            },
            Type::Dict(row) => Type::Dict(Self::apply_row(row, map)),
            Type::Union(types) => {
                Type::Union(types.iter().map(|t| Self::apply_inner(t, map)).collect())
            }
            Type::Intersection(types) => {
                Type::Intersection(types.iter().map(|t| Self::apply_inner(t, map)).collect())
            }
            Type::Negation(inner) => Type::Negation(Box::new(Self::apply_inner(inner, map))),
            Type::StageApp { fn_name, args } => Type::StageApp {
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
            tail: match &row.tail {
                RowTail::Uniform { key, value } => RowTail::Uniform {
                    key: key.as_ref().map(|k| Box::new(Self::apply_inner(k, map))),
                    value: Box::new(Self::apply_inner(value, map)),
                },
                other => other.clone(),
            },
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
    /// instantiate as something other than Type::Var. Currently used for
    /// Kind::Operator variables, which instantiate as Type::Operator(fresh_name)
    /// rather than Type::Var(fresh_name, level). This enables builtin TypeSchemes
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
    /// Per-parameter narrowing types declared via `@[narrows: T]` annotation on the binding.
    ///
    /// When a predicate function `foo?@[narrows: Int] [fn [let x] ...]` is defined, this
    /// field holds `vec![Some(Type::Int)]`. `extract_narrowings` reads this to produce
    /// `Narrowing::TypeOf` constraints when the function appears as a condition in an `if`
    /// or match guard. Any function — not just prelude predicates — can declare narrowing.
    ///
    /// Index parallel to the function's parameter list. `None` means the parameter has no
    /// declared narrowing; `Some(T)` means `[foo? x]` being true narrows `x` to `T`.
    pub param_narrowings: Vec<Option<crate::type_def::Type>>,
    /// Source span where this binding was defined (if known).
    /// Populated by `generalize_with_doc` with the binding's span. Used by lost-binding
    /// warnings to point at the definition site. `None` for synthetic/builtin schemes.
    pub definition_span: Option<crate::ast::Span>,
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
            definition_span: None,
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
pub type SchemeMap = HashMap<(u32, u32, u32, u32), TypeScheme>;

/// Type-stage entry: either a resolved type, a function, a type variable kind, or a class.
#[derive(Debug, Clone)]
pub enum TypeStageEntry {
    /// Fully materialized type — no further evaluation needed.
    Resolved(Type),
    /// Function thunk that must be called to produce a type — used for parameterized
    /// type constructors (e.g., Seq, Result) where the type-stage function takes type
    /// parameters and returns a TypeNode.
    Function(std::sync::Arc<crate::value::Thunk>),
    /// Annotation type variable of this kind — produces a fresh TypeVar when resolved.
    TypeVar(crate::type_def::Kind),
    /// Class constraint — produces a fresh TypeVar with a class constraint when resolved.
    Class(crate::type_class::ClassDecl),
}

/// Inference state for levels-based let-generalization
#[derive(Debug, Clone)]
pub struct InferState {
    pub level: u32,
    pub levels: HashMap<String, u32>,
    /// Cached InstanceEnv snapshot. Invalidated by `invalidate_env_caches` after any
    /// `insert_instance` call. Rebuilt lazily by `build_instance_env_snapshot`.
    cached_instance_env: Option<crate::types::InstanceEnv>,
    /// Working copy of InstanceEnv for async constraint resolution. Cloned once per inference
    /// pass from `cached_instance_env`, wrapped in Arc for cheap sharing across constraint checks.
    /// Cleared by `invalidate_env_caches`.
    working_instance_env: Option<std::sync::Arc<crate::types::InstanceEnv>>,
    /// Cached ClassEnv snapshot. Invalidated by `invalidate_env_caches` after any
    /// `insert_class` call. Rebuilt lazily by `build_class_env_snapshot`.
    cached_class_env: Option<crate::types::ClassEnv>,
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
    /// Unified environment: the canonical store for classes, instances, type schemes, and values.
    /// Class/instance lookups go through `state.env.read().unwrap().get_class(name)` and
    /// `state.env.read().unwrap().get_instance(mangled)`.
    pub env: Arc<RwLock<crate::env::Env>>,
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
    /// Expected return type of the currently-inferring function (if annotated).
    /// Set by `infer_fn_push_cont` (CEK) when entering a function body with an explicit return annotation,
    /// cleared when exiting. Used for inferred [do] macro to determine which monad to use.
    pub expected_return: Option<Type>,
    /// Expected parameter types for the next `fn` expression to be inferred.
    /// Set by `infer_instance_decl_from_surface` before calling `run_typecheck` on an instance
    /// method body, using the class method's specialized signature. Consumed and cleared by
    /// `infer_fn_push_cont` when it processes the params — it is single-use per fn invocation.
    ///
    /// Index i = expected type for the i-th fixed (non-variadic) parameter.
    /// When `Some`, unannotated params use `expected_fn_params[i]` instead of `Type::Unknown`.
    /// When `None` (the default), unannotated params fall back to `Type::Unknown` as before.
    pub expected_fn_params: Option<Vec<Type>>,
    /// Accumulated type diagnostics (warnings, hints).
    /// Populated during type inference and generalization, extracted by the type checker.
    pub diagnostics: Vec<crate::error::TypeDiagnostic>,
    /// Deferred equality constraints for stuck TypeStageApp applications.
    /// When a TypeStageApp has non-ground arguments or cannot be reduced, equality
    /// constraints involving it are deferred here. After each round of unification,
    /// process_deferred_equalities attempts to resolve them.
    /// Actively written to (type_unify.rs) and saved/restored during branch inference (typecheck.rs).
    pub deferred_equalities: Vec<(Type, Type)>,
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
    /// TypeAnnotationTable for nested TypeAssert nodes: keyed by NodeId of the TypeAssert Arc<SurfaceNode>.
    /// Populated by the CEK machine's TypeAssert handler. Extracted by process_document
    /// to merge into the document-level annotation table.
    pub type_annotation_table: crate::ast::TypeAnnotationTable,
    /// Resolution table for slot-indexed TypeEnv lookups.
    ///
    /// The CEK machine's VarRef handler uses the resolved (level, slot) coordinates to
    /// call `env.get_scheme_at(level, slot)` — O(1) per-level. This is the single
    /// authority for VarRef type resolution (no name-based fallback for user bindings).
    ///
    /// Always populated: entry points run the resolver before type-checking. Tests that
    /// construct InferState directly get an empty table (no VarRef resolution → all lookups
    /// fall through to extras).
    pub resolution_table: std::sync::Arc<crate::ast::ResolutionTable>,
    /// Scope frames from the resolver pass: one frame per document-level intermediate dict.
    /// Each frame maps binding names to their absolute resolver-assigned slot numbers.
    /// Populated from `resolve_surface_program`; retained for future use or introspection.
    /// Dict bindings now go to extras via `insert_scheme_named_only` (not slot-indexed).
    pub resolver_frames: Vec<indexmap::IndexMap<String, u32>>,
    /// EvalContext from tinct's evaluation pipeline — passed in when type-checking runs
    /// within a program evaluation (e.g. via builtin-typecheck). Used by resolve_type_head
    /// to materialize type-stage thunks without ambient filesystem access. Never created
    /// inside the type checker; always provided by the caller that has proper capabilities.
    pub eval_ctx: Option<std::sync::Arc<crate::eval::EvalContext>>,
    /// Type-stage scope chain: pre-computed types from type-stage evaluation.
    /// Vec[0] = innermost (highest priority); Vec[N-1] = outermost.
    /// Each frame is a HashMap of type names to their TypeStageEntry.
    /// Populated by builtin-tc-update-type-stage-env (T-1803) and builtin_typecheck_doc
    /// write-back. Empty Vec means no type-stage types are available.
    pub type_stage_scope: Vec<std::collections::HashMap<String, TypeStageEntry>>,
    /// Unified TypeVar table (from HEAD~1 design).
    /// In the current design, TypeVar bindings are in `subst.type_map`, levels in `levels`,
    /// and kinds in `kind_env`. This field exists for compatibility with type_class.rs
    /// save/restore probe patterns.
    pub type_vars: indexmap::IndexMap<String, TypeVarEntry>,
    /// BAS TypeVar bounds (from HEAD~1 design).
    pub bounds: std::collections::HashMap<String, crate::bas::TypeVarBounds>,
    /// Set of TypeVar names currently being processed by FD improvement (compatibility field).
    pub fd_in_progress: std::collections::HashSet<String>,
    /// Type constructor environment (from HEAD~1 design).
    /// Maps type constructor names to their TyConDef.
    pub tycon_env: std::collections::HashMap<String, std::sync::Arc<crate::type_def::TyConDef>>,
    /// Scope frames derived from parent_scope_id. Used by check_constraints_on_var
    /// to resolve typeclass dispatch decisions to (level, slot) coordinates.
    /// Set by builtin-typecheck-doc before inference begins (T-1730).
    pub scope_frames: Option<Vec<indexmap::IndexMap<String, u32>>>,
    /// Deferred typeclass dispatch obligations, keyed by TypeVar name.
    /// Added by T-1731 at VarRef instantiation time; drained by check_constraints_on_var
    /// when the TypeVar resolves to a concrete type.
    pub dispatch_obligations: Vec<DispatchObligation>,
    /// Eval-stage GroupSpine for type-stage expression evaluation.
    /// When set, `eval_type_stage_expr` uses this as the root scope frame instead of
    /// `EvalFrame::empty()`. Built from the doc-env thunks passed to `builtin-typecheck-doc`
    /// so that type-stage expressions (e.g. `@Integer`) can resolve names from the
    /// accumulated environment.
    pub type_stage_eval_group: Option<std::sync::Arc<crate::value::GroupSpine>>,
}

impl InferState {
    pub fn new() -> Self {
        Self::with_env(Arc::new(RwLock::new(crate::env::Env::new())))
    }

    /// Create a new InferState with the given unified environment.
    ///
    /// All class/instance lookups go through `state.env`. The env must already contain
    /// all classes and instances visible during this type-checking run (seeded from parent
    /// environments via `Env::with_parent` chains).
    pub fn with_env(env: Arc<RwLock<crate::env::Env>>) -> Self {
        Self {
            level: 0,
            levels: HashMap::new(),
            cached_instance_env: None,
            working_instance_env: None,
            cached_class_env: None,
            subst: Substitution::new(),
            constraints: Vec::new(),
            kind_env: HashMap::new(),
            env,
            failed_bindings: HashMap::new(),
            scheme_map: None,
            expected_return: None,
            expected_fn_params: None,
            diagnostics: Vec::new(),
            deferred_equalities: Vec::new(),
            fd_depth: 0,
            instance_resolution_depth: 0,
            type_var_source_names: HashMap::new(),
            t013_emitted: std::collections::HashSet::new(),
            type_annotation_table: crate::ast::TypeAnnotationTable::new(),
            resolution_table: std::sync::Arc::new(std::collections::HashMap::new()),
            resolver_frames: Vec::new(),
            eval_ctx: None,
            type_stage_scope: Vec::new(),
            type_vars: indexmap::IndexMap::new(),
            bounds: std::collections::HashMap::new(),
            fd_in_progress: std::collections::HashSet::new(),
            tycon_env: std::collections::HashMap::new(),
            scope_frames: None,
            dispatch_obligations: Vec::new(),
            type_stage_eval_group: None,
        }
    }

    /// Add a type class constraint to an explicit constraint accumulator.
    /// Used by the new InferState API (HEAD~1 style) where constraints are passed explicitly.
    /// Falls back gracefully: if the class is not in `env`, just skips the constraint.
    pub fn add_constraint_to(
        &mut self,
        constraints: &mut Vec<Constraint>,
        class_name: impl Into<String>,
        var: impl Into<String>,
    ) {
        let class_name = class_name.into();
        let env_arc = Arc::clone(&self.env);
        let env_guard = env_arc.read().unwrap();
        if let Some(class_decl) = env_guard.get_class(&class_name) {
            constraints.push(Constraint::Class {
                class: Arc::new(class_decl.clone()),
                vars: vec![crate::type_class::ConstraintArg::Var(var.into())],
                origin_name: None,
                origin_span: None,
            });
        }
        // Unknown classes are deferred — instance resolution will report an error.
    }

    // ── TypeVar name generation ──────────────────────────────────────────────────

    /// Generate a TypeVar name from a source name, kind, and source span.
    ///
    /// Format:
    ///   Kind::Type:  `{source}⧼{file}:{line}:{col}⧽`   e.g. `a⧼main.llt:42:7⧽`
    ///   Kind::Label: `ʟᴀʙᴇʟ∷{source}⧼{file}:{line}:{col}⧽`
    ///
    /// The span MUST always have file/line/col information — tinct source positions come from
    /// the parsed AST; Rust-internal creation sites use `rust_span!()` to embed the Rust
    /// source location. No 0:0 or empty-file spans are permitted.
    pub fn typevar_name(source: &str, kind: &Kind, span: &Span) -> String {
        let file = span.file.as_ref();
        let line = span.start_line;
        let col = span.start_col;
        match kind {
            Kind::Label => format!("ʟᴀʙᴇʟ∷{}⧼{}:{}:{}⧽", source, file, line, col),
            _ => format!("{}⧼{}:{}:{}⧽", source, file, line, col),
        }
    }

    /// Extract only the raw source name from a TypeVar name, stripping both the kind prefix
    /// and the position suffix. Used when deriving scheme variable names for instantiation.
    ///
    /// `a⧼main.llt:42:7⧽`          → `a`
    /// `ʟᴀʙᴇʟ∷k⧼main.llt:5:3⧽`   → `k`  (strips kind prefix AND position suffix)
    /// `a`                           → `a`  (already bare)
    pub fn typevar_source_only(name: &str) -> &str {
        // Strip position suffix first (everything from ⧼ onward)
        let without_pos = if let Some(bracket) = name.find('⧼') {
            &name[..bracket]
        } else {
            name
        };
        // Strip kind prefix (ʟᴀʙᴇʟ∷ for Label kind)
        if let Some(rest) = without_pos.strip_prefix("ʟᴀʙᴇʟ∷") {
            rest
        } else {
            without_pos
        }
    }

    /// The single TypeVar creation entry point.
    ///
    /// Creates a fresh TypeVar at the given (or current) level with the specified kind.
    /// The name is derived from `source_name` and the call site `span` — no monotonic
    /// counter. Every call site must supply a real span: tinct-source sites pass the
    /// AST node span; Rust-internal sites use `rust_span!()`.
    ///
    /// Returns `(name, Type::Var(name, level))`.
    pub fn fresh_type_var_with(
        &mut self,
        source_name: Option<&str>,
        level: Option<u32>,
        kind: Kind,
        span: &Span,
    ) -> (String, Type) {
        let src = source_name.unwrap_or("?");
        let lvl = level.unwrap_or(self.level);
        let name = Self::typevar_name(src, &kind, span);
        self.levels.insert(name.clone(), lvl);
        self.type_vars
            .entry(name.clone())
            .or_insert_with(|| TypeVarEntry::blank(lvl, kind));
        let ty = Type::Var(name.clone(), lvl);
        (name, ty)
    }

    /// Convenience: fresh Kind::Type TypeVar using the current level. Pass a real span.
    pub fn fresh_type_var(&mut self, span: &Span) -> Type {
        self.fresh_type_var_with(None, None, Kind::Type, span).1
    }

    // fresh_row_var_name removed — BAS Step 4: no RowVar tails exist

    /// Invalidate the cached InstanceEnv and ClassEnv snapshots.
    ///
    /// Must be called after every `insert_instance` or `insert_class` call so that
    /// subsequent `build_instance_env_snapshot` / `build_class_env_snapshot` calls
    /// rebuild against the updated env rather than serving stale data.
    pub fn invalidate_env_caches(&mut self) {
        self.cached_instance_env = None;
        self.working_instance_env = None;
        self.cached_class_env = None;
    }

    /// Build a temporary `ClassEnv` from `self.env` for backward-compatible callers.
    ///
    /// This is a bridge for code that still needs a `ClassEnv` reference (e.g., `entails`,
    /// `satisfies_constraint`). The returned ClassEnv is a snapshot — it does not update
    /// when `self.env` changes. Use sparingly; prefer direct `self.env.read().get_class()`.
    ///
    /// The result is cached: repeated calls with no intervening `insert_class` return
    /// a reference to the same snapshot without rebuilding. Call `invalidate_env_caches`
    /// after any `insert_class` to flush the cache.
    pub fn build_class_env_snapshot(&mut self) -> &crate::types::ClassEnv {
        if self.cached_class_env.is_none() {
            let mut class_env = crate::types::ClassEnv::new();
            let env_guard = self.env.read().unwrap();
            for decl in env_guard.all_classes() {
                class_env.insert(decl);
            }
            self.cached_class_env = Some(class_env);
        }
        self.cached_class_env.as_ref().unwrap()
    }

    /// Build a temporary `InstanceEnv` from `self.env` for backward-compatible callers.
    ///
    /// This is a bridge for code that still calls `InstanceEnv::resolve_instance`,
    /// `lookup_mptc`, or `reverse_lookup_mptc`. The returned InstanceEnv is a snapshot.
    ///
    /// The result is cached: repeated calls with no intervening `insert_instance` return
    /// a reference to the same snapshot without rebuilding. Call `invalidate_env_caches`
    /// after any `insert_instance` to flush the cache.
    ///
    /// When you need an owned copy for async consumers, use `get_working_instance_env` instead
    /// to avoid cloning on every call site.
    pub(crate) fn build_instance_env_snapshot(&mut self) -> &crate::types::InstanceEnv {
        if self.cached_instance_env.is_none() {
            let mut inst_env = crate::types::InstanceEnv::new();
            let env_guard = self.env.read().unwrap();
            for (_mangled, decl) in env_guard.all_instances() {
                inst_env
                    .insert(decl)
                    .expect("duplicate instance during env cache rebuild — env invariant violated");
            }
            self.cached_instance_env = Some(inst_env);
        }
        self.cached_instance_env.as_ref().unwrap()
    }

    /// Get a working copy of InstanceEnv for async constraint resolution.
    /// Clones from cached_instance_env ONCE per inference pass, wraps in Arc, then returns
    /// Arc clones (cheap - just increments ref count) on subsequent calls. This avoids 2500+
    /// full InstanceEnv clones per document when checking constraints.
    /// Call this instead of `build_instance_env_snapshot().clone()` at constraint check sites.
    ///
    /// Returns an Arc<InstanceEnv> that can be moved into async functions across await points.
    pub fn get_working_instance_env(&mut self) -> std::sync::Arc<crate::types::InstanceEnv> {
        if self.working_instance_env.is_none() {
            self.working_instance_env = Some(std::sync::Arc::new(
                self.build_instance_env_snapshot().clone(),
            ));
        }
        std::sync::Arc::clone(self.working_instance_env.as_ref().unwrap())
    }

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

    /// Get a filtered view of the kind environment that excludes `Kind::Type` entries.
    ///
    /// `Kind::Type` is the default kind for regular type variables and is therefore not
    /// stored explicitly — callers that enumerate the kind environment expect to see only
    /// *non-default* kinds (`Kind::Operator`, `Kind::Label`, `Kind::Arrow`, …).  Returning
    /// `Kind::Type` entries would cause `test_kind_env_view` to fail and would confuse
    /// callers that use the kind environment to identify operator-kinded variables.
    pub fn kind_env(&self) -> HashMap<String, Kind> {
        self.kind_env
            .iter()
            .filter(|(_, k)| !matches!(k, Kind::Type))
            .map(|(name, kind)| (name.clone(), kind.clone()))
            .collect()
    }

    /// Get the Kind for a TypeVar name.
    ///
    /// Returns the explicitly-set kind from `kind_env` when present; returns
    /// `Some(Kind::Type)` as the default when the variable exists in `levels` (i.e.
    /// has been registered as a TypeVar) but has no explicit kind entry; returns
    /// `None` only when the variable is completely unknown.
    ///
    /// This matches the semantics described in `TypeVarEntry.kind`: every registered
    /// TypeVar has a kind, and `Kind::Type` is the default for ordinary type variables.
    pub fn get_kind(&self, name: &str) -> Option<Kind> {
        if let Some(kind) = self.kind_env.get(name) {
            return Some(kind.clone());
        }
        // Default: if the variable is registered (has a level), its kind is Type.
        if self.levels.contains_key(name) {
            return Some(Kind::Type);
        }
        None
    }

    /// Look up the binding for a TypeVar name from the unified `type_vars` table.
    ///
    /// Returns `Some(ty)` if the variable has been bound (via `bind_type_var`), `None` if
    /// the variable is unbound or not registered.  `type_vars` is the single canonical
    /// source for this lookup — the separate `subst.type_map` is NOT consulted.  This
    /// means that `state.type_vars = saved_snapshot` correctly hides bindings added after
    /// the snapshot was taken (see `test_type_vars_snapshot_restore_pattern`).
    ///
    /// All TypeVar creation paths (`fresh_type_var`, `fresh_type_var_with_source`,
    /// `fresh_type_var_with_origin`, `alloc_type_var_at_level`, `set_level`,
    /// and `instantiate_scheme`) register the variable in `type_vars`, so every
    /// TypeVar that can be bound is visible here.
    pub fn lookup_binding(&self, name: &str) -> Option<Type> {
        self.type_vars.get(name).and_then(|e| e.binding.clone())
    }

    /// Bind a TypeVar to a type.
    ///
    /// Writes the binding to both `type_vars[name].binding` (so that snapshot/restore
    /// patterns on `type_vars` correctly capture and discard bindings) and to
    /// `subst.type_map` (for the existing unification and substitution application paths).
    pub fn bind_type_var(&mut self, name: String, ty: Type) {
        if let Some(entry) = self.type_vars.get_mut(&name) {
            entry.binding = Some(ty.clone());
        }
        self.subst.type_map.borrow_mut().insert(name, ty);
    }

    /// Set the level for a TypeVar name and ensure it exists in `type_vars`.
    ///
    /// Writes to `levels` (for backward-compatible callers) and also inserts a blank entry
    /// into `type_vars` if one does not already exist.  This ensures that `type_vars` is
    /// the canonical unified table: any TypeVar registered via `set_level` can later be
    /// found and bound via `bind_type_var`, and its binding will be preserved through
    /// snapshot/restore patterns on `type_vars`.
    pub fn set_level(&mut self, name: impl Into<String>, level: u32) {
        let name = name.into();
        self.levels.insert(name.clone(), level);
        self.type_vars
            .entry(name)
            .or_insert_with(|| TypeVarEntry::blank(level, Kind::Type));
    }

    /// Get the level of a TypeVar name (compatibility with new InferState API).
    pub fn get_level(&self, name: &str) -> Option<u32> {
        self.levels.get(name).copied()
    }

    /// Returns the level of a TypeVar, or 0 if the name is not registered.
    ///
    /// Level 0 is the correct default for unregistered TypeVar names. These occur as
    /// binder variables in `Type::Recursive { var, body }` — the binder name is a
    /// string, not a fresh TypeVar created by `fresh_type_var_with`, so it is never in
    /// `state.levels`. The correct level for a binder is 0 (outermost scope), which
    /// causes `lower_levels_check_occurs` to correctly cap any TypeVars in the body
    /// to the binder's scope. Level 0 is also a safe default for TypeVars created
    /// directly in tests without going through `fresh_type_var_with`.
    pub fn get_level_for_occurs_check(&self, name: &str) -> u32 {
        self.levels.get(name).copied().unwrap_or(0)
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
        use crate::ast::Span;
        let mut state = InferState::new();

        // Create two fresh TypeVars using span-based names.
        let span_a = Span::rust_source(file!(), line!());
        let span_b = Span::rust_source(file!(), line!() + 1);
        let tv0 = state.fresh_type_var(&span_a); // registers name in levels at level 0
        let tv1 = state.fresh_type_var(&span_b); // registers name in levels at level 0

        let name0 = match &tv0 {
            Type::Var(n, _) => n.clone(),
            _ => panic!("not a TypeVar"),
        };
        let name1 = match &tv1 {
            Type::Var(n, _) => n.clone(),
            _ => panic!("not a TypeVar"),
        };

        assert!(
            state.levels.contains_key(&name0),
            "tv0 should be in levels before compaction"
        );
        assert!(
            state.levels.contains_key(&name1),
            "tv1 should be in levels before compaction"
        );

        // Bind tv0 → Int by inserting it into the substitution's type_map.
        // This simulates what unification does when it solves a TypeVar.
        state
            .subst
            .type_map
            .borrow_mut()
            .insert(name0.clone(), Type::Int);

        // compact_levels() should remove tv0 (now in type_map) but keep tv1 (unbound).
        state.compact_levels();

        assert!(
            !state.levels.contains_key(&name0),
            "tv0 should be removed from levels after compaction (it is unified)"
        );
        assert!(
            state.levels.contains_key(&name1),
            "tv1 should remain in levels after compaction (it is still unbound)"
        );
    }

    /// `compact_levels()` is a no-op when no TypeVars have been unified.
    /// All registered TypeVars remain in `levels`.
    #[test]
    fn test_compact_levels_preserves_unbound_vars() {
        use crate::ast::Span;
        let mut state = InferState::new();
        let span_a = Span::rust_source(file!(), line!());
        let span_b = Span::rust_source(file!(), line!() + 1);
        state.fresh_type_var(&span_a);
        state.fresh_type_var(&span_b);

        let count_before = state.levels.len();
        state.compact_levels();
        let count_after = state.levels.len();

        assert_eq!(
            count_before, count_after,
            "compact_levels() must not remove unbound TypeVars"
        );
    }

    /// Substitution::apply_row must substitute through RowTail::Uniform
    /// to prevent TypeVars from leaking out of their scope during generalization.
    #[test]
    fn test_apply_row_substitutes_uniform_tail() {
        use crate::type_def::{Row, RowTail};
        use indexmap::IndexMap;
        use std::collections::HashMap;

        // Create a substitution: _t7 → Type::Str
        let mut type_map = HashMap::new();
        type_map.insert("_t7".to_string(), Type::Str);
        let subst = Substitution {
            type_map: std::cell::RefCell::new(type_map),
        };

        // Create a Row with RowTail::Uniform { key: Type::Str, value: TypeVar("_t7") }
        let row = Row {
            fields: IndexMap::new(),
            tail: RowTail::Uniform {
                key: None,
                value: Box::new(Type::Var("_t7".to_string(), 0)),
            },
        };

        // Apply the substitution
        let result_row = Substitution::apply_row(&row, &subst.type_map.borrow());

        // Verify the result has value: Type::Str (substitution applied)
        match &result_row.tail {
            RowTail::Uniform { key, value } => {
                assert_eq!(key, &None, "key should remain None");
                assert_eq!(
                    **value,
                    Type::Str,
                    "value should be Type::Str after substitution"
                );
            }
            _ => panic!("tail should be RowTail::Uniform after substitution"),
        }
    }
}
