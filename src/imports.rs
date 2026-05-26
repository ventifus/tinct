//! Import resolution for the type checker.
//!
//! This module provides shared import resolution logic that seeds the type checker
//! with prelude function type signatures. It ensures that `typecheck_source`
//! knows about stdlib prelude functions, suppressing false "undefined variable" errors.
//!
//! The prelude environment is built once per thread and cached using thread-local
//! storage. Subsequent calls to `build_prelude_env()` return a cheap `Rc::clone`
//! of the cached environment.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, RwLock};

use crate::ast::{Span, SurfaceExpression, SurfaceNode, SurfaceProgram};
use crate::desugar; // TODO(parts-e): remove when desugar.rs is deleted (blocked on evaluator CoreExpr migration)
use crate::expand;
use crate::parser;
use crate::resolve;
use crate::typecheck::{typecheck_surface_program_with_env, TypeMap};
use crate::types::{ClassEnv, InferState, InstanceEnv, Row, Type, TypeAlias, TypeEnv};

/// Type alias for include bindings map: span → list of (name, type) pairs
type IncludeBindings = HashMap<Span, Vec<(String, Type)>>;

/// Depth limit for recursive include resolution (prevents infinite include cycles).
const MAX_INCLUDE_DEPTH: usize = 16;

thread_local! {
    /// Thread-local cache of the prelude type environment.
    /// Built once per thread on first access, then reused for all subsequent calls.
    static PRELUDE_CACHE: RefCell<Option<Rc<TypeEnv>>> = const { RefCell::new(None) };

    /// Thread-local cache of the prelude's class and instance environments.
    ///
    /// Populated after prelude.llt is type-checked (in `build_prelude_env_inner`).
    /// Consumed by `seed_infer_state_from_prelude_cache` to propagate prelude-registered
    /// instances (Equatable, Comparable, Showable, Mappable, Appendable) to user-code
    /// type-checking sessions. Without this, each fresh `InferState::new()` starts with
    /// an empty `instance_env`, so constraint checking for those classes always falls through
    /// to the hardcoded arms in `satisfies_constraint`.
    static PRELUDE_INSTANCE_CACHE: RefCell<Option<(ClassEnv, InstanceEnv)>> = const { RefCell::new(None) };

    /// Thread-local cache of the type-stage evaluation environment.
    ///
    /// Contains type dicts (Int, Str, etc.) and type-level functions (Seq, Map, union, all).
    /// Built once per thread on first access, then reused for annotation resolution.
    static TYPE_STAGE_ENV_CACHE: RefCell<Option<Arc<RwLock<crate::value::Environment>>>> = const { RefCell::new(None) };

    /// Recursion guard for type-stage env building (prevents infinite recursion when
    /// type-checking the prelude's type-stage sections).
    static BUILDING_TYPE_STAGE_ENV: RefCell<bool> = const { RefCell::new(false) };
}

/// Build or retrieve the prelude type environment.
///
/// This function parses the embedded prelude source, type-checks it, and extracts
/// all top-level binding types. The result is cached in thread-local storage, so
/// subsequent calls return a cheap `Rc::clone` of the cached environment.
///
/// If the prelude has type errors (e.g., unresolvable type variables), they are
/// silently ignored and the best-effort environment is returned.
pub fn build_prelude_env() -> Rc<TypeEnv> {
    PRELUDE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(ref env) = *cache {
            // Cache hit: return a clone of the cached environment
            return Rc::clone(env);
        }

        // Cache miss: build the prelude environment from scratch
        let env = build_prelude_env_inner();
        *cache = Some(Rc::clone(&env));
        env
    })
}

/// Helper function to type-check a stdlib module and extract its bindings into the given env.
/// Returns the final `InferState` on success (used by the prelude path to capture instance_env).
fn typecheck_and_merge_stdlib_module(
    source: &str,
    parent_env: &Rc<TypeEnv>,
    env: &mut TypeEnv,
    _source_path: Option<&str>,
) -> Result<InferState, ()> {
    // Parse the module source
    let mut program = {
        let parsed = parser::parse(source).map_err(|_| ())?;
        parsed.program.clone()
    };

    // Skip macro expansion for stdlib modules.
    //
    // Rationale: stdlib modules (prelude.llt, macros.llt) never use [defmacro ...],
    // so expand_surface_program is a no-op for them — but at depth 0 it triggers a full
    // create_stdlib_env() bootstrap (~20s in debug builds). Since build_prelude_env
    // is called once per test thread, this turns parallel test runs into a hang
    // when each of N threads pays the 20s bootstrap cost simultaneously under
    // memory pressure.
    //
    // The previous code called expand::expand_surface_program(file, true) here, which
    // recursively built the stdlib just to check for macros that don't exist.

    // Desugar $_ implicit lambdas on SurfaceProgram (after expansion, before resolve).
    // Correct pipeline order: parse → expand → desugar → resolve → typecheck.
    // Expansion is normally in the pipeline but skipped here for stdlib modules
    // (see rationale above) — desugar still runs to maintain correct ordering.
    desugar::desugar_surface_program(&mut program);

    // Variable resolution pass (Phase 1 of arena allocation strategy).
    let _res_table = resolve::resolve_surface_program(&program);

    // Type-check with the parent environment (builtins + prelude), capturing InferState
    // and the final TypeEnv (which holds properly generalized TypeSchemes for all prelude
    // bindings — no TypeVar erasure needed).
    //
    // `enable_scheme_map: false` — no LSP hover needed for stdlib modules.
    // `in_prelude_load: true` — skip instance method body inference (optimization).
    //
    // typecheck_surface_program_with_env bridges to the File-based path internally via
    // surface_program_to_file, so no manual conversion is needed here.
    let (
        _type_errors,
        _type_map,
        _doc_map,
        _scheme_map,
        _diagnostics,
        state,
        final_env,
        _annotation_table, // not needed for stdlib module type merging
    ) = typecheck_surface_program_with_env(&program, Rc::clone(parent_env), false, true);

    // Merge the generalized schemes from the final env into the output env.
    //
    // final_env contains fully generalized TypeSchemes for prelude bindings from successfully
    // typechecked documents. We use these preferentially because they preserve polymorphism
    // (e.g., `map` stays ∀a b. (a→b)→[a]→[b] instead of being erased to Fn@Unknown [Unknown]).
    //
    // For any prelude function that came from a document with type errors (and thus isn't in
    // final_env), we fall back to the TypeMap-based extraction with erase_type_vars — this is
    // the previous behavior: stale TypeVars become Unknown rather than being left unresolved.
    // The TypeMap-based fallback only inserts a binding if it's not already in env (i.e., not
    // already inserted by merge_env_bindings_into), so there's no double-insertion.
    merge_env_bindings_into(&final_env, parent_env, env);
    extract_bindings_from_program_with_fallback(&program, &_type_map, env);

    Ok(state)
}

/// Copy all bindings from `source_env` that were explicitly defined by the prelude into `target`.
///
/// "Prelude-defined" means the binding appears in at least one frame of `source_env` that is
/// NOT part of the `baseline_env` chain. This correctly captures names that the prelude
/// explicitly exports (e.g., `=`, `+`, `keys`, `any?`) while excluding raw builtin names that
/// the prelude never mentions (e.g., `connect`, `http2-session`).
///
/// For names that exist in BOTH the prelude's frames AND the baseline (e.g., `=`, `+`),
/// the prelude's scheme takes precedence — this is intentional: the prelude may add richer
/// type information (e.g., Equatable constraints on `=`) than the raw builtin scheme.
///
/// Algorithm: walk `source_env`'s frame chain collecting names, stopping at the frame
/// identified as the baseline root by pointer comparison with `baseline_env`. Names found
/// in frames above the baseline are "prelude-defined" and are included.
fn merge_env_bindings_into(source_env: &TypeEnv, baseline_env: &Rc<TypeEnv>, target: &mut TypeEnv) {
    // Collect names from source_env frames that are ABOVE the baseline.
    // We walk the frame chain, collecting names until we reach a frame that IS the baseline
    // (by pointer identity) or has no further parent.
    let mut prelude_names = std::collections::HashSet::new();
    collect_names_above_baseline(source_env, baseline_env, &mut prelude_names);

    // Insert the scheme for each prelude-defined name (using source_env.get for the full lookup).
    for name in prelude_names {
        if let Some(scheme) = source_env.get(&name) {
            target.insert_scheme(name, scheme.clone());
        }
    }
}

/// Collect all binding names from `env`'s frame chain that are above (not part of) `baseline`.
///
/// Walks the frame chain, adding names from each frame to `names`. Stops when it reaches
/// a frame pointer-equal to `baseline` (the frame IS the baseline) or when there is no parent.
/// This correctly collects all names introduced by the prelude without including the raw builtins.
fn collect_names_above_baseline(
    env: &TypeEnv,
    baseline: &Rc<TypeEnv>,
    names: &mut std::collections::HashSet<String>,
) {
    // Add names from this frame (own frame only, no parent walk)
    env.collect_own_names(names);

    // Walk to parent, stopping if we've reached the baseline
    if let Some(parent) = env.parent() {
        if !Rc::ptr_eq(parent, baseline) {
            collect_names_above_baseline(parent, baseline, names);
        }
        // If parent IS the baseline, stop — we've collected all prelude-defined names.
    }
}

/// Fallback extraction: insert TypeMap-derived bindings for names NOT already in `target`.
///
/// Used after `merge_env_bindings_into` so that prelude bindings from documents that had
/// type errors (and were therefore dropped from the final TypeEnv) still get inserted into
/// the output env. TypeVars are erased to Unknown (the previous behavior) since the TypeMap
/// holds monotype bodies from a stale InferState, not generalized schemes.
///
/// Skips any name already in `target` (already inserted by merge_env_bindings_into).
fn extract_bindings_from_program_with_fallback(
    program: &SurfaceProgram,
    type_map: &TypeMap,
    target: &mut TypeEnv,
) {
    for doc in &program.documents {
        for node in doc.node.expressions() {
            extract_bindings_fallback_from_node(node, type_map, target);
        }
    }
}

/// Recursively extract bindings from a surface node into `target`, skipping
/// names already present in `target`.
fn extract_bindings_fallback_from_node(
    node: &Arc<SurfaceNode>,
    type_map: &TypeMap,
    target: &mut TypeEnv,
) {
    match &node.expr {
        SurfaceExpression::Dict(entries) => {
            for entry in entries {
                if let Some(ref key_node) = entry.node.key {
                    let name = match &key_node.expr {
                        SurfaceExpression::Str(n) => Some(n.clone()),
                        SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                        SurfaceExpression::Annotated { name, .. } => Some(name.clone()),
                        _ => None,
                    };
                    if let Some(name) = name {
                        // Skip if already inserted by merge_env_bindings_into.
                        // get_own checks only the current frame (not parent chain)
                        // so we correctly detect only our own insertions.
                        if target.get_own(&name).is_some() {
                            continue;
                        }
                        let value_span = entry.node.value.span;
                        let key = (value_span.start.offset, value_span.end.offset);
                        if let Some(ty) = type_map.get(&key) {
                            let sanitized = erase_type_vars(ty);
                            target.insert(name, sanitized);
                        }
                    }
                }
            }
        }
        SurfaceExpression::Sequential(nodes) => {
            for child in nodes {
                extract_bindings_fallback_from_node(child, type_map, target);
            }
        }
        _ => {}
    }
}

/// Inner implementation of `build_prelude_env()`.
///
/// Parses the embedded prelude source, runs the type-checking pipeline
/// (desugar → resolve → typecheck), and extracts all top-level binding
/// types into a new `TypeEnv`. Returns the environment even if type errors occur
/// (best-effort approach).
fn build_prelude_env_inner() -> Rc<TypeEnv> {
    // Start with an empty TypeEnv.
    // The user-facing TypeEnv should contain ONLY what the prelude explicitly exports,
    // NOT the raw builtin registry. Builtins are visible during prelude type-checking
    // (via builtins_env below), but must not leak into user scope.
    // After typecheck_and_merge_stdlib_module runs, merge_env_bindings_into copies
    // only the prelude-added entries (filtered against builtins_env as baseline) into env.
    let mut env = TypeEnv::new();

    // Inject capability variable types that the CLI always provides.
    // These are runtime-injected by the CLI (see main.rs:905, 955, 934),
    // so the type checker needs to know about them to avoid false "undefined variable" errors.
    env.insert("%cwd".to_string(), crate::types::Type::DirCap);
    env.insert("%libdir".to_string(), crate::types::Type::DirCap);
    // %stdin is Handle[Readable Text]
    {
        let mut caps = HashMap::new();
        caps.insert("Readable".to_string(), Type::Bool);
        caps.insert("Text".to_string(), Type::Bool);
        env.insert(
            "%stdin".to_string(),
            Type::Handle(Box::new(Type::Record(Row { fields: caps }))),
        );
    }

    // Only prelude.llt is loaded at startup.
    // strings.llt, math.llt, and encoding.llt require explicit [include libdir "module.llt"].
    let prelude_source = include_str!("../stdlib/prelude.llt");

    // Type-check prelude
    let mut builtins_env = TypeEnv::with_builtins();
    // Inject capability types into builtins_env for prelude type-checking
    builtins_env.insert("%cwd".to_string(), crate::types::Type::DirCap);
    builtins_env.insert("%libdir".to_string(), crate::types::Type::DirCap);
    builtins_env.insert(
        "%stdin".to_string(),
        crate::types::Type::Handle(Box::new(Type::Unknown)),
    );
    // %rust injection: previously needed for [include %rust "..."] calls in prelude.llt.
    // include-decomp-prelude removed those calls; this entry is now a no-op but harmless.
    // Kept here to avoid type errors if any external code still references %rust.
    builtins_env.insert("%rust".to_string(), crate::types::Type::Unknown);
    // Inject builtin-* aliases for prelude type-checking only.
    // prelude.llt uses builtin-lt, builtin-eq, etc. to call Rust primitives
    // by stable names. These are NOT in user scope — inject_builtin_aliases()
    // must NOT be called on the user-facing env (line 222 above).
    builtins_env.inject_builtin_aliases();
    let builtins_env = Rc::new(builtins_env);

    match typecheck_and_merge_stdlib_module(
        prelude_source,
        &builtins_env,
        &mut env,
        Some("prelude.llt"),
    ) {
        Err(_) => {
            // Parse/expand error: return builtins-only environment (with capability bindings)
            return builtins_env;
        }
        Ok(prelude_state) => {
            // Cache the prelude's class and instance environments.
            // User-code InferState instances are seeded from this cache (via
            // `seed_infer_state_from_prelude_cache`) so that prelude-registered instances
            // (Equatable, Comparable, Showable, Mappable, Appendable) are visible during
            // constraint checking. Without this, `check_constraints_on_var` falls through
            // to the hardcoded arms in `satisfies_constraint` for all non-Numeric classes.
            PRELUDE_INSTANCE_CACHE.with(|cache| {
                *cache.borrow_mut() = Some((
                    prelude_state.class_env.clone(),
                    prelude_state.instance_env.clone(),
                ));
            });
        }
    }

    // Post-process: restore authoritative builtin schemes for constraint-annotated
    // operators whose prelude-inferred schemes may be degraded.
    //
    // Root cause: prelude type-checking runs in a single pass over a large letrec dict.
    // If any entry in the dict produces a type error (e.g., an instance pattern overlap),
    // `infer_dict` returns Err and the entire dict's generalized schemes are discarded
    // from `final_env`. The fallback path (`extract_bindings_from_program_with_fallback`)
    // extracts types from the TypeMap, but TypeVars are erased to Unknown — producing
    // `Fn@Bool [x: Unknown y: Unknown]` instead of `Equatable a => Fn@Bool [x: a y: a]`.
    //
    // Fix: if the prelude-inferred scheme for `=` or `<` is monomorphic (no type_vars),
    // replace it with the authoritative builtin scheme. The builtin schemes have the
    // correct Equatable/Comparable constraints and are structurally identical to the
    // prelude wrappers — the only difference is the constraint display in LSP hover.
    //
    // Note: if the prelude scheme IS polymorphic (has type_vars), it is kept as-is,
    // since it may carry additional information (doc strings, tighter return type, etc.).
    // The "==" and "<" operators require Equatable/Comparable constraints.
    // A degraded scheme is one that either has no type_vars OR has type_vars but no constraints
    // (the constraints were lost when the prelude dict had type errors and the scheme was
    // inferred without constraint propagation).
    // Same fix for `get` / `get?` — the prelude wrappers `get: [fn@[return: a] [let key@k
    // dict@d] [builtin-get key dict]]` lose the `Indexable c k v` functional dependency
    // constraint during SCC generalization. The SCC inference accumulates constraints across
    // multiple iterations and the discharged-variable detection in generalize_with_doc
    // inconsistently marks the FD constraint vars, causing it to be dropped as ambiguous.
    // Restoring the authoritative builtin scheme ensures `get 1 (Seq[String])` resolves
    // the return type to `String` via the Indexable FD machinery.
    for name in &["=", "<"] {
        let is_degraded = env
            .get_own(name)
            .map(|s| s.type_vars.is_empty() || s.constraints.is_empty())
            .unwrap_or(true); // missing from env → definitely degraded
        if is_degraded {
            if let Some(builtin_scheme) = builtins_env.get(name) {
                env.insert_scheme((*name).to_string(), builtin_scheme.clone());
            }
        }
    }

    // `get` / `get?` / `builtin-get`: always restore the authoritative Indexable-constrained
    // scheme. The prelude wrappers carry the Indexable constraint in their generalized scheme,
    // but the Indexable FD fails to fire at call sites due to SCC-interaction issues in the
    // constraint generalization machinery (the constraint is present but the discharged-variable
    // detection inconsistently marks vars, causing `improve_functional_dependency_inner` to see
    // a partially-ground constraint that fails the `all_det_ground` check).
    // The authoritative builtin scheme `Indexable c k v => k → c → v` passes through
    // `instantiate_scheme` correctly and the FD fires reliably. This mirrors the existing
    // `=`/`<` fallback for Equatable/Comparable constraints.
    for name in &["get", "get?", "builtin-get"] {
        if let Some(builtin_scheme) = builtins_env.get(name) {
            env.insert_scheme((*name).to_string(), builtin_scheme.clone());
        }
    }

    // Propagate capability type aliases from the builtins env to the user-facing env.
    // These are registered in TypeEnv::with_builtins() (e.g. NetCap, DirCap, Handle, Url)
    // and must be available for @NetCap / @DirCap / @Handle annotations in user code.
    // merge_env_bindings_into only copies value bindings; type aliases require this pass.
    let aliases: Vec<(String, TypeAlias)> = builtins_env
        .own_type_aliases()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    for (name, alias) in aliases {
        env.insert_type_alias(name, alias);
    }

    Rc::new(env)
}

/// Seed a fresh [`InferState`] with the prelude's class and instance environments.
///
/// Called at the start of every user-code type-checking session (in
/// `typecheck_file_with_types_and_env_and_source_returning_state`). This ensures
/// that class instances registered by `prelude.llt` (Equatable for Int/Float/Str/Bool,
/// Comparable for Int/Float/Str, Showable for all, Mappable for Record/Seq, Appendable
/// for Str/Record/Seq) are available to `check_constraints_on_var` without requiring
/// hardcoded arms in `satisfies_constraint`.
///
/// This is a no-op when:
/// - The prelude has not yet been type-checked (cache is empty). This handles the case
///   where we are currently type-checking the prelude itself.
/// - The cache is empty due to a prelude parse/expand error.
pub fn seed_infer_state_from_prelude_cache(state: &mut InferState) {
    PRELUDE_INSTANCE_CACHE.with(|cache| {
        if let Some((class_env, instance_env)) = &*cache.borrow() {
            // Merge prelude classes into state (or_insert: don't overwrite user-defined classes)
            for class_decl in class_env.iter_classes() {
                state.class_env.insert_if_absent(class_decl.clone());
            }
            // Merge prelude instances into state (skip overlapping instances silently)
            for inst_decl in instance_env.iter_instances() {
                let _ = state.instance_env.insert(inst_decl.clone());
            }
        }
    });
}

/// Build or retrieve the type-stage evaluation environment.
///
/// This environment contains type dicts (Int, Str, etc.) and type-level functions
/// (Seq, Map, union, all) extracted from the prelude's `--- stage: type` sections.
/// Used by the annotation resolver for evaluating bracket annotations.
///
/// The environment is cached in thread-local storage to avoid re-parsing and
/// re-evaluating the prelude on every type-checking run.
///
/// Returns None if:
/// - We are currently building the type-stage env (recursion guard)
/// - Type-stage env creation fails (graceful degradation)
pub fn build_type_stage_env() -> Option<Arc<RwLock<crate::value::Environment>>> {
    // Check recursion guard first (before cache check, to avoid borrow conflicts)
    let is_building = BUILDING_TYPE_STAGE_ENV.with(|flag| *flag.borrow());
    if is_building {
        // We're already building the type-stage env (recursive call from create_type_stage_env)
        return None;
    }

    TYPE_STAGE_ENV_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(ref env) = *cache {
            // Cache hit: return a clone of the cached environment
            return Some(Arc::clone(env));
        }

        // Set recursion guard
        BUILDING_TYPE_STAGE_ENV.with(|flag| *flag.borrow_mut() = true);

        // Cache miss: build the type-stage environment from scratch
        let result = match crate::builtins::create_type_stage_env() {
            Ok(env) => {
                *cache = Some(Arc::clone(&env));
                Some(env)
            }
            Err(_) => {
                // If type-stage env creation fails, return None (graceful degradation)
                None
            }
        };

        // Clear recursion guard
        BUILDING_TYPE_STAGE_ENV.with(|flag| *flag.borrow_mut() = false);

        result
    })
}

/// Replace all TypeVar occurrences in a type with Unknown.
///
/// TypeVars extracted from the prelude's type_map are stale: they were created
/// in the prelude's InferState and have no meaning in user code's InferState.
/// Leaving them in the prelude env causes CALL-POLY at user call sites, where
/// the first argument binds the TypeVar and subsequent arguments are checked via
/// subsumption against the first argument's type — producing false type errors.
///
/// Replacing stale TypeVars with Unknown restores the pre-sprint gradual behavior:
/// any argument type is acceptable (Unknown ~ T for all T).
fn erase_type_vars(ty: &crate::types::Type) -> crate::types::Type {
    use crate::types::{Row, Type};
    match ty {
        Type::TypeVar(_, _) => Type::Unknown,
        Type::Function {
            params,
            ret,
            variadic,
        } => Type::Function {
            params: params
                .iter()
                .map(|(name, t)| (name.clone(), erase_type_vars(t)))
                .collect(),
            ret: Box::new(erase_type_vars(ret)),
            variadic: *variadic,
        },
        Type::Record(row) => Type::Record(Row {
            fields: row
                .fields
                .iter()
                .map(|(k, t)| (k.clone(), erase_type_vars(t)))
                .collect(),
        }),
        Type::Seq(elem) => Type::Seq(Box::new(erase_type_vars(elem))),
        Type::Map(k, v) => Type::Map(Box::new(erase_type_vars(k)), Box::new(erase_type_vars(v))),
        Type::Union(members) => {
            Type::normalize_union(members.iter().map(erase_type_vars).collect())
        }
        Type::Intersection(members) => {
            // Preserve the intersection structure but erase TypeVars inside
            let erased: Vec<Type> = members.iter().map(erase_type_vars).collect();
            if erased.len() == 1 {
                erased.into_iter().next().unwrap()
            } else {
                Type::Intersection(erased)
            }
        }
        Type::Negation(inner) => Type::Negation(Box::new(erase_type_vars(inner))),
        Type::App(f, a) => Type::App(Box::new(erase_type_vars(f)), Box::new(erase_type_vars(a))),
        // Concrete types: return unchanged
        other => other.clone(),
    }
}

/// Extract top-level binding names and their types from a SurfaceProgram's type map as a Vec.
///
/// Returns a vector of (name, type) pairs. Used by `resolve_includes` to track
/// which bindings each include contributed.
fn extract_bindings_from_program_as_vec(
    program: &SurfaceProgram,
    type_map: &TypeMap,
) -> Vec<(String, Type)> {
    let mut bindings = Vec::new();
    for doc in &program.documents {
        for node in doc.node.expressions() {
            extract_bindings_from_node_to_vec(node, type_map, &mut bindings);
        }
    }
    bindings
}

/// Recursively extract bindings from a surface node into a Vec.
fn extract_bindings_from_node_to_vec(
    node: &Arc<SurfaceNode>,
    type_map: &TypeMap,
    bindings: &mut Vec<(String, Type)>,
) {
    match &node.expr {
        SurfaceExpression::Dict(entries) => {
            // Extract all top-level bindings from this dict
            for entry in entries {
                // Only process entries with explicit string keys (VarRef or Annotated)
                if let Some(ref key_node) = entry.node.key {
                    let name = match &key_node.expr {
                        SurfaceExpression::Str(n) => Some(n.clone()),
                        SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                        SurfaceExpression::Annotated { name, .. } => Some(name.clone()),
                        _ => None,
                    };
                    if let Some(name) = name {
                        let value_span = entry.node.value.span;
                        let key = (value_span.start.offset, value_span.end.offset);
                        if let Some(ty) = type_map.get(&key) {
                            let sanitized = erase_type_vars(ty);
                            bindings.push((name, sanitized));
                        }
                    }
                }
            }
        }
        // Sequential expressions: process ALL expressions in order.
        SurfaceExpression::Sequential(nodes) => {
            for child in nodes {
                extract_bindings_from_node_to_vec(child, type_map, bindings);
            }
        }
        _ => {
            // Other expression types don't introduce bindings at the top level
        }
    }
}

/// Collect statically-known include paths from a SurfaceProgram.
///
/// Walks the AST looking for `[include ...]` patterns and extracts
/// the string literal paths. Returns a list of `(span, cap_name, path)` tuples
/// where `cap_name` is `Some("%libdir")` or `Some("%cwd")` for cap-qualified includes,
/// or `None` for deprecated bare includes.
///
/// Skips dynamic includes (computed paths).
pub fn collect_include_paths(program: &SurfaceProgram) -> Vec<(Span, Option<String>, String)> {
    let mut paths = Vec::new();
    for doc in &program.documents {
        for node in doc.node.expressions() {
            collect_include_paths_from_node(node, &mut paths);
        }
    }
    paths
}

/// Recursively collect include paths from a surface node tree.
fn collect_include_paths_from_node(
    node: &Arc<SurfaceNode>,
    paths: &mut Vec<(Span, Option<String>, String)>,
) {
    match &node.expr {
        SurfaceExpression::Call {
            func,
            args,
            named_args: _,
            implied: _,
        } => {
            // Check if this is a call to `include`
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if name == "include" {
                    // Handle 2-arg cap-qualified form: [include %cap "path"]
                    if args.len() == 2 {
                        if let SurfaceExpression::VarRef { name: cap_name, .. } = &args[0].expr {
                            if let SurfaceExpression::Str(path) = &args[1].expr {
                                paths.push((args[1].span, Some(cap_name.clone()), path.clone()));
                            }
                        }
                    }
                    // Deprecated 1-arg form: [include "path"]
                    // Skip this form — cap-qualified is now required
                }
            }
            // Recurse into function and arguments
            collect_include_paths_from_node(func, paths);
            for arg in args {
                collect_include_paths_from_node(arg, paths);
            }
        }
        SurfaceExpression::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    collect_include_paths_from_node(key, paths);
                }
                collect_include_paths_from_node(&entry.node.value, paths);
            }
        }
        SurfaceExpression::Fn { body, .. } => {
            collect_include_paths_from_node(body, paths);
        }
        SurfaceExpression::TypeAssert { expr: inner, .. } => {
            collect_include_paths_from_node(inner, paths);
        }
        SurfaceExpression::Pipe { lhs, rhs } => {
            collect_include_paths_from_node(lhs, paths);
            collect_include_paths_from_node(rhs, paths);
        }
        SurfaceExpression::Sequential(nodes) => {
            for child in nodes {
                collect_include_paths_from_node(child, paths);
            }
        }
        SurfaceExpression::DotAccess { expr: target, .. } => {
            collect_include_paths_from_node(target, paths);
        }
        SurfaceExpression::Quote(inner)
        | SurfaceExpression::Unquote(inner)
        | SurfaceExpression::UnquoteSplice(inner) => {
            collect_include_paths_from_node(inner, paths);
        }
        SurfaceExpression::Match { scrutinee, arms } => {
            collect_include_paths_from_node(scrutinee, paths);
            for arm in arms {
                if let Some(ref guard) = arm.guard {
                    collect_include_paths_from_node(guard, paths);
                }
                collect_include_paths_from_node(&arm.body, paths);
            }
        }
        SurfaceExpression::PatternDecl { bindings } => {
            for binding in bindings {
                collect_include_paths_from_node(binding, paths);
            }
        }
        SurfaceExpression::LetDecl { bindings } => {
            for binding in bindings {
                collect_include_paths_from_node(binding, paths);
            }
        }
        SurfaceExpression::CaseArm { pattern, body } => {
            collect_include_paths_from_node(pattern, paths);
            collect_include_paths_from_node(body, paths);
        }
        SurfaceExpression::TypeApp { func, arg } => {
            collect_include_paths_from_node(func, paths);
            collect_include_paths_from_node(arg, paths);
        }
        // Literals and other leaf nodes: no recursive traversal needed
        SurfaceExpression::Int(_)
        | SurfaceExpression::Float(_)
        | SurfaceExpression::Bool(_)
        | SurfaceExpression::Str(_)
        | SurfaceExpression::VarRef { .. }
        | SurfaceExpression::Placeholder
        | SurfaceExpression::Decl(_) // type-level declaration, no include paths inside
        | SurfaceExpression::Rest(_)
        | SurfaceExpression::Annotated { .. }
        | SurfaceExpression::Error(_) => {}
    }
}

/// Resolve include paths and build a TypeEnv with all included bindings.
///
/// For each path in `include_paths`:
/// 1. Resolve relative to the appropriate base directory (determined by cap variable)
/// 2. Skip if already visited (cycle detection)
/// 3. Read the file and parse it
/// 4. Type-check it with the accumulated environment
/// 5. Extract bindings and extend the environment
///
/// Cap-qualified includes:
/// - `%libdir` → resolve relative to `libdir` parameter
/// - `%cwd` → resolve relative to `base_dir` parameter
/// - Other caps → skip silently (e.g., `%custom_cap` is not supported)
///
/// Returns a tuple of:
/// - The accumulated `TypeEnv` with all included bindings
/// - A mapping from each include call's `Span` to the bindings it contributed
///
/// Returns `base_env` unchanged on any IO or parse failure (best-effort approach).
/// Depth is capped at `MAX_INCLUDE_DEPTH` to prevent runaway recursion.
// AMBIENT-OK: type-checker include resolution fallback — reads libdir files without cap; type-only, no runtime I/O
#[allow(clippy::disallowed_methods)]
fn resolve_includes(
    include_paths: &[(Span, Option<String>, String)],
    base_dir: Option<&Path>,
    libdir: Option<&Path>,
    base_env: Rc<TypeEnv>,
    visited: &mut HashSet<String>,
    depth: usize,
    cap_dir: &cap_std::fs::Dir,
) -> (Rc<TypeEnv>, IncludeBindings) {
    if depth >= MAX_INCLUDE_DEPTH {
        // Depth limit reached: return base_env unchanged with empty binding map
        return (base_env, HashMap::new());
    }

    let mut env = base_env;
    let mut include_bindings: HashMap<Span, Vec<(String, Type)>> = HashMap::new();

    for (span, cap_name, path) in include_paths {
        // Determine the base directory based on the cap variable
        let resolve_base = match cap_name.as_deref() {
            Some("%libdir") => libdir,
            Some("%cwd") => base_dir,
            Some(_other) => {
                // Unknown cap variable — skip this include silently
                continue;
            }
            None => {
                // Bare include without cap qualifier — deprecated, skip
                continue;
            }
        };

        // Resolve the path relative to the determined base
        let full_path = if let Some(base) = resolve_base {
            base.join(path)
        } else {
            // No base directory available for this cap — skip
            continue;
        };

        // Normalize the path for cycle detection
        let normalized = match full_path.canonicalize() {
            Ok(p) => p,
            Err(_) => continue, // Skip unresolvable paths
        };

        // SECURITY: Verify resolved path is beneath the declared base directory.
        // Prevents path traversal via `../../` sequences in include arguments.
        // The runtime $include uses cap-std RESOLVE_BENEATH; we replicate that
        // protection here for the LSP type-env path.
        if let Some(base) = resolve_base {
            if let Ok(canonical_base) = base.canonicalize() {
                if !normalized.starts_with(&canonical_base) {
                    continue; // Path escapes base — reject silently
                }
            }
        }

        let path_key = normalized.to_string_lossy().to_string();

        // Check for cycles
        if visited.contains(&path_key) {
            continue;
        }
        visited.insert(path_key);

        // Enforce the same 10 MB limit as the runtime $include.
        // Use cap_std for %cwd paths (RESOLVE_BENEATH semantics); fall back to std::fs for %libdir paths.
        let use_cap = cap_name.as_deref() == Some("%cwd");
        let file_len = if use_cap {
            // Derive relative path by stripping the canonical base prefix.
            let canonical_base = base_dir.and_then(|b| b.canonicalize().ok());
            let relative = if let Some(ref base) = canonical_base {
                normalized.strip_prefix(base).unwrap_or(&normalized)
            } else {
                &normalized
            };
            match cap_dir.metadata(relative) {
                Ok(m) => m.len(),
                Err(_) => continue,
            }
        } else {
            match std::fs::metadata(&normalized) {
                Ok(m) => m.len(),
                Err(_) => continue,
            }
        };
        if file_len > crate::builtins::MAX_FILE_SIZE {
            continue;
        }

        // Read the file — use cap_std RESOLVE_BENEATH for %cwd paths.
        let content = if use_cap {
            let canonical_base = base_dir.and_then(|b| b.canonicalize().ok());
            let relative = if let Some(ref base) = canonical_base {
                normalized.strip_prefix(base).unwrap_or(&normalized)
            } else {
                &normalized
            };
            match cap_dir.open(relative) {
                Ok(mut f) => {
                    use std::io::Read;
                    let mut buf = String::new();
                    match f.read_to_string(&mut buf) {
                        Ok(_) => buf,
                        Err(_) => continue,
                    }
                }
                Err(_) => continue,
            }
        } else {
            match std::fs::read_to_string(&normalized) {
                Ok(c) => c,
                Err(_) => continue, // Skip unreadable files
            }
        };

        // Parse the file
        let parsed = match parser::parse(&content) {
            Ok(p) => p,
            Err(_) => continue, // Skip unparseable files
        };

        // Run macro expansion (tolerate errors).
        // Use the provided cap_std Dir for expansion.
        let expand_dir = cap_dir;
        // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> resolve.
        // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
        // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
        let mut program = parsed.program;
        if crate::async_rt::block_on_anywhere(expand::expand_surface_program(
            &mut program,
            true,
            expand_dir,
        ))
        .is_err()
        {
            continue;
        }
        // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
        desugar::desugar_surface_program(&mut program);
        // Variable resolution pass (Phase 1 of arena allocation strategy).
        let _resolution_table = resolve::resolve_surface_program(&program);

        // Type-check with the appropriate environment.
        // For %libdir files (stdlib modules), use a builtins env so that raw builtin
        // names (e.g. `url`, `http-request`) are visible — stdlib files use builtins
        // directly and must not be checked against the restricted user env. This
        // mirrors how `build_prelude_env_inner()` type-checks prelude.llt.
        // For %cwd files (user includes), use the accumulated user env as normal.
        let typecheck_env = if cap_name.as_deref() == Some("%libdir") {
            let mut benv = TypeEnv::with_builtins();
            benv.insert("%cwd".to_string(), crate::types::Type::DirCap);
            benv.insert("%libdir".to_string(), crate::types::Type::DirCap);
            benv.inject_builtin_aliases();
            Rc::new(benv)
        } else {
            Rc::clone(&env)
        };
        let in_prelude_load = cap_name.as_deref() == Some("%libdir");
        let (_type_errors, type_map, _doc_map, _scheme_map, _diagnostics, _state, _final_env, _ann) =
            typecheck_surface_program_with_env(&program, typecheck_env, false, in_prelude_load);

        // Extract bindings from this program and track them
        let mut new_env = TypeEnv::with_parent(&env);
        let bindings = extract_bindings_from_program_as_vec(&program, &type_map);
        for (name, ty) in &bindings {
            new_env.insert(name.clone(), ty.clone());
        }
        env = Rc::new(new_env);

        // Store the bindings for this include call's span
        include_bindings.insert(*span, bindings);

        // Recursively resolve includes from this program.
        // Open the nested file's parent directory via cap_dir to enforce RESOLVE_BENEATH.
        let nested_includes = collect_include_paths(&program);
        let parent_dir = normalized.parent();

        // Derive relative path from cap_dir to the parent directory
        let canonical_base = base_dir.and_then(|b| b.canonicalize().ok());
        let relative_parent = if let Some(parent) = parent_dir {
            if let Some(ref base) = canonical_base {
                parent.strip_prefix(base).unwrap_or(parent)
            } else {
                parent
            }
        } else {
            std::path::Path::new(".")
        };

        // Open nested dir through cap_dir (narrow from existing cap)
        let nested_cap_dir = match cap_dir.open_dir(relative_parent) {
            Ok(d) => d,
            Err(_) => continue, // Skip if parent dir can't be opened
        };

        let (nested_env, nested_bindings) = resolve_includes(
            &nested_includes,
            parent_dir,
            libdir,
            env,
            visited,
            depth + 1,
            &nested_cap_dir,
        );
        env = nested_env;

        // Merge nested include bindings into our map
        include_bindings.extend(nested_bindings);
    }

    (env, include_bindings)
}

/// Post-pass: populate `type_map` with `Type::Record` types for include call expressions.
///
/// After `typecheck_file_with_types_and_env` runs, call this function to inject inferred
/// types for `[include %cap "path"]` expressions into the TypeMap. The type checker does not
/// visit include calls specially — it sees them as opaque function calls returning `Unknown`.
/// This post-pass replaces those `Unknown` entries with precise `Record` types derived from
/// the bindings each include contributed.
///
/// # Algorithm
///
/// Walks the Surface AST to capture call-site spans. For each `[include %cap "path"]` call:
///
/// 1. Looks up `args[1].span` (the path string's span) in `include_bindings`.
/// 2. If found, constructs `Type::Record(Row { fields })` from the contributed bindings.
/// 3. Inserts the Record type at the call expression's span `(start_offset, end_offset)`
///    in `type_map`.
///
/// This enables `[io: [include %libdir "io.llt"]]` to give `io` a precise Record type
/// with the exact fields exported by `io.llt`.
///
/// # Span key: path argument, not call expression
///
/// `resolve_includes` stores binding maps keyed by `args[1].span` (the path string literal).
/// This post-pass re-walks the Surface AST to recover the call expression's own span,
/// using `args[1].span` as the lookup key.
pub fn apply_include_type_post_pass(
    program: &SurfaceProgram,
    include_bindings: &HashMap<Span, Vec<(String, Type)>>,
    type_map: &mut TypeMap,
) {
    for doc in &program.documents {
        for node in doc.node.expressions() {
            apply_include_type_to_node(node, include_bindings, type_map);
        }
    }
}

/// Recursively walk surface nodes and inject Record types for include calls.
fn apply_include_type_to_node(
    node: &Arc<SurfaceNode>,
    include_bindings: &HashMap<Span, Vec<(String, Type)>>,
    type_map: &mut TypeMap,
) {
    match &node.expr {
        SurfaceExpression::Call {
            func,
            args,
            named_args: _,
            implied: _,
        } => {
            // Check for a cap-qualified include call: [include %cap "path"]
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if name == "include" && args.len() == 2 {
                    if let SurfaceExpression::VarRef { .. } = &args[0].expr {
                        if let SurfaceExpression::Str(_) = &args[1].expr {
                            // args[1].span is the lookup key used by resolve_includes
                            let path_span = args[1].span;
                            if let Some(bindings) = include_bindings.get(&path_span) {
                                // Build a closed Record type from the contributed bindings
                                let fields: HashMap<String, Type> = bindings
                                    .iter()
                                    .map(|(name, ty)| (name.clone(), ty.clone()))
                                    .collect();
                                let record_ty = Type::Record(Row { fields });
                                // Store at the call expression's span
                                let key = (node.span.start.offset, node.span.end.offset);
                                type_map.insert(key, record_ty);
                            }
                        }
                    }
                }
            }
            // Recurse into function and arguments (handles nested includes)
            apply_include_type_to_node(func, include_bindings, type_map);
            for arg in args {
                apply_include_type_to_node(arg, include_bindings, type_map);
            }
        }
        SurfaceExpression::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    apply_include_type_to_node(key, include_bindings, type_map);
                }
                apply_include_type_to_node(&entry.node.value, include_bindings, type_map);
            }
        }
        SurfaceExpression::Fn { body, .. } => {
            apply_include_type_to_node(body, include_bindings, type_map);
        }
        SurfaceExpression::TypeAssert { expr: inner, .. } => {
            apply_include_type_to_node(inner, include_bindings, type_map);
        }
        SurfaceExpression::Pipe { lhs, rhs } => {
            apply_include_type_to_node(lhs, include_bindings, type_map);
            apply_include_type_to_node(rhs, include_bindings, type_map);
        }
        SurfaceExpression::Sequential(nodes) => {
            for child in nodes {
                apply_include_type_to_node(child, include_bindings, type_map);
            }
        }
        SurfaceExpression::DotAccess { expr: target, .. } => {
            apply_include_type_to_node(target, include_bindings, type_map);
        }
        SurfaceExpression::Quote(inner)
        | SurfaceExpression::Unquote(inner)
        | SurfaceExpression::UnquoteSplice(inner) => {
            apply_include_type_to_node(inner, include_bindings, type_map);
        }
        SurfaceExpression::Match { scrutinee, arms } => {
            apply_include_type_to_node(scrutinee, include_bindings, type_map);
            for arm in arms {
                if let Some(ref guard) = arm.guard {
                    apply_include_type_to_node(guard, include_bindings, type_map);
                }
                apply_include_type_to_node(&arm.body, include_bindings, type_map);
            }
        }
        SurfaceExpression::PatternDecl { bindings } => {
            for binding in bindings {
                apply_include_type_to_node(binding, include_bindings, type_map);
            }
        }
        SurfaceExpression::LetDecl { bindings } => {
            for binding in bindings {
                apply_include_type_to_node(binding, include_bindings, type_map);
            }
        }
        SurfaceExpression::CaseArm { pattern, body } => {
            apply_include_type_to_node(pattern, include_bindings, type_map);
            apply_include_type_to_node(body, include_bindings, type_map);
        }
        SurfaceExpression::TypeApp { func, arg } => {
            apply_include_type_to_node(func, include_bindings, type_map);
            apply_include_type_to_node(arg, include_bindings, type_map);
        }
        // Leaf nodes: no recursive traversal needed
        SurfaceExpression::Int(_)
        | SurfaceExpression::Float(_)
        | SurfaceExpression::Bool(_)
        | SurfaceExpression::Str(_)
        | SurfaceExpression::VarRef { .. }
        | SurfaceExpression::Placeholder
        | SurfaceExpression::Decl(_) // type-level declaration, no include paths inside
        | SurfaceExpression::Rest(_)
        | SurfaceExpression::Annotated { .. }
        | SurfaceExpression::Error(_) => {}
    }
}

/// Build a type environment seeded with prelude types and optionally with
/// types from statically-resolvable includes.
///
/// This is the main entry point for the LSP and other tools that need a
/// fully-populated type environment for a given program.
///
/// 1. Start with the prelude environment from `build_prelude_env()`
/// 2. Seed the environment with always-available cap variables (`%cwd`, `%libdir`, `%stdin`)
/// 3. If `base_dir` is provided, collect include paths from `program` and
///    resolve them recursively, extending the environment with each included
///    file's top-level bindings.
///
/// Returns a tuple of:
/// - The accumulated `TypeEnv` with all bindings
/// - A mapping from each include call's `Span` to the bindings it contributed
///
/// Best-effort: IO failures, parse errors, and type errors are silently ignored.
// AMBIENT-OK: type-checker API entry point — opens CWD once; propagated to all nested resolution
#[allow(clippy::disallowed_methods)]
pub fn build_type_env(
    program: &SurfaceProgram,
    base_dir: Option<&Path>,
) -> (Rc<TypeEnv>, IncludeBindings) {
    // Open "." as the base cap dir for type-checking includes
    let cwd_cap = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
        .expect("build_type_env: failed to open CWD as cap dir");
    build_type_env_with_cap(program, base_dir, &cwd_cap)
}

/// Like `build_type_env`, but also accepts a `cap_std::fs::Dir` for `%cwd` I/O.
///
/// When `base_cap_dir` is `Some`, file reads for `%cwd`-qualified includes go through
/// the cap-std Dir (RESOLVE_BENEATH semantics) instead of plain `std::fs` calls.
/// This provides kernel-level path confinement rather than software-only path checks.
pub fn build_type_env_with_cap(
    program: &SurfaceProgram,
    base_dir: Option<&Path>,
    cap_dir: &cap_std::fs::Dir,
) -> (Rc<TypeEnv>, IncludeBindings) {
    let prelude_env = build_prelude_env();

    // Seed with always-available cap types
    let mut env = TypeEnv::with_parent(&prelude_env);
    env.insert("%cwd".to_string(), crate::types::Type::DirCap);
    env.insert("%libdir".to_string(), crate::types::Type::DirCap);
    // %stdin is Handle[Readable Text]
    {
        let mut caps = HashMap::new();
        caps.insert("Readable".to_string(), Type::Bool);
        caps.insert("Text".to_string(), Type::Bool);
        env.insert(
            "%stdin".to_string(),
            Type::Handle(Box::new(Type::Record(Row { fields: caps }))),
        );
    }
    // %rust is a legacy virtual module cap — stdlib files (net.llt, io.llt) may reference it.
    // The runtime module system was deleted, but the type checker must still accept the identifier
    // without raising "undefined variable: %rust" errors during stdlib include type-checking.
    env.insert("%rust".to_string(), crate::types::Type::Unknown);
    let mut env = Rc::new(env);

    let mut include_bindings = HashMap::new();

    if let Some(dir) = base_dir {
        let include_paths = collect_include_paths(program);
        let mut visited = HashSet::new();
        let libdir = crate::find_libdir_path();
        let (new_env, bindings) = resolve_includes(
            &include_paths,
            Some(dir),
            libdir.as_deref(),
            env,
            &mut visited,
            0,
            cap_dir,
        );
        env = new_env;
        include_bindings = bindings;
    }

    (env, include_bindings)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test helpers use ambient open_ambient_dir; test-only
mod tests {
    use super::*;

    #[test]
    fn test_build_prelude_env_caches() {
        let env1 = build_prelude_env();
        let env2 = build_prelude_env();
        // Should return the same Rc (pointer equality)
        assert!(Rc::ptr_eq(&env1, &env2));
    }

    #[test]
    fn test_build_prelude_env_has_prelude_exports() {
        let env = build_prelude_env();
        // After builtin-privacy Phase 2: user-facing env contains ONLY prelude exports,
        // not the raw builtin registry. Check a representative sample of prelude-exported names.
        //
        // Prelude re-exports: keys, map, filter, split, str, append, range, error, ...
        assert!(
            env.get("keys").is_some(),
            "keys should be in prelude exports"
        );
        assert!(env.get("map").is_some(), "map should be in prelude exports");
        assert!(
            env.get("filter").is_some(),
            "filter should be in prelude exports"
        );
        //
        // Raw network I/O builtins that are NOT exported by prelude should be absent.
        // These test the boundary: user code should not see raw TCP/QUIC primitives.
        // http2-session and connect are registered in standard_builtins() but not re-exported
        // by prelude, so they should be absent in the user-facing TypeEnv.
        assert!(
            env.get("http2-session").is_none(),
            "http2-session should NOT be in prelude env — raw I/O builtin not exported by prelude"
        );
        assert!(
            env.get("connect").is_none(),
            "connect should NOT be in prelude env — raw I/O builtin not exported by prelude"
        );
    }

    #[test]
    fn test_build_prelude_env_has_prelude_functions() {
        let env = build_prelude_env();
        // Check that prelude functions are present (examples from stdlib/prelude.llt)
        // These are LLT-implemented functions, not Rust builtins
        assert!(env.get("any?").is_some());
        assert!(env.get("all?").is_some());
        assert!(env.get("cond").is_some());
    }

    #[test]
    fn test_collect_include_paths_empty() {
        let program = SurfaceProgram { documents: vec![] };
        let paths = collect_include_paths(&program);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_collect_include_paths_finds_cap_qualified_includes() {
        let source = r#"
            [include %cwd "foo.llt"]
            [include %libdir "bar.llt"]
        "#;
        let program = parser::parse(source).unwrap().program;
        let paths = collect_include_paths(&program);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].1, Some("%cwd".to_string()));
        assert_eq!(paths[0].2, "foo.llt");
        assert_eq!(paths[1].1, Some("%libdir".to_string()));
        assert_eq!(paths[1].2, "bar.llt");
    }

    #[test]
    fn test_collect_include_paths_skips_dynamic_includes() {
        let source = r#"
            [include [str "foo" ".llt"]]
        "#;
        let program = parser::parse(source).unwrap().program;
        let paths = collect_include_paths(&program);
        // Dynamic include should be skipped
        assert_eq!(paths.len(), 0);
    }

    /// Verify that the prelude environment contains type bindings for the six
    /// core prelude functions that downstream code commonly references.
    #[test]
    fn build_prelude_env_resolves_prelude_functions() {
        let env = build_prelude_env();
        // These are all LLT-implemented functions defined in stdlib/prelude.llt,
        // not Rust builtins — their presence proves the full prelude pipeline ran.
        assert!(env.get("map").is_some(), "expected 'map' in prelude env");
        assert!(
            env.get("filter").is_some(),
            "expected 'filter' in prelude env"
        );
        assert!(env.get("and").is_some(), "expected 'and' in prelude env");
        assert!(env.get("or").is_some(), "expected 'or' in prelude env");
        assert!(
            env.get("flatten").is_some(),
            "expected 'flatten' in prelude env"
        );
        assert!(env.get("zip").is_some(), "expected 'zip' in prelude env");

        // strings/math/encoding are NOT loaded at startup — require explicit [include libdir ...]
        assert!(
            env.get("pad-left").is_none(),
            "pad-left should not be in prelude env (requires explicit include)"
        );
        assert!(
            env.get("pi").is_none(),
            "pi should not be in prelude env (requires explicit include)"
        );
        assert!(
            env.get("hex-encode").is_none(),
            "hex-encode should not be in prelude env (requires explicit include)"
        );
    }

    /// Verify that `collect_include_paths` finds `[call $include %cap "path"]` forms.
    ///
    /// This exercises the explicit `call` form (`[call $include ...]`) as distinct
    /// from the implied-call form (`[include ...]`) tested in the existing test.
    /// Both parse to the same `SurfaceExpression::Call` node with `func = VarRef { name: "include" }`.
    #[test]
    fn collect_include_paths_finds_explicit_call_form() {
        let source = r#"[call $include %cwd "foo.llt"]"#;
        let program = parser::parse(source).unwrap().program;
        let paths = collect_include_paths(&program);
        assert_eq!(paths.len(), 1, "expected exactly one include path");
        assert_eq!(paths[0].1, Some("%cwd".to_string()));
        assert_eq!(paths[0].2, "foo.llt");
    }

    /// Verify that `collect_include_paths` skips deprecated 1-arg includes.
    #[test]
    fn collect_include_paths_skips_bare_includes() {
        let source = r#"[include "foo.llt"]"#;
        let program = parser::parse(source).unwrap().program;
        let paths = collect_include_paths(&program);
        assert_eq!(paths.len(), 0, "bare includes should be skipped");
    }

    /// Verify that `resolve_includes` returns `base_env` unchanged when the
    /// include path does not exist on disk.
    ///
    /// The implementation skips paths whose `canonicalize()` fails, so a missing
    /// file is silently ignored and the original environment is returned unmodified.
    #[test]
    fn resolve_includes_missing_file_returns_base() {
        let base_env = Rc::new(TypeEnv::with_builtins());
        let include_paths = vec![(
            Span::origin(),
            Some("%cwd".to_string()),
            "nonexistent_file_xyz.llt".to_string(),
        )];
        let tmp = std::env::temp_dir();
        let mut visited = HashSet::new();

        // Open a test cap dir (required by resolve_includes signature)
        let test_cap_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test cap dir");

        let (result_env, result_bindings) = resolve_includes(
            &include_paths,
            Some(tmp.as_path()),
            None, // no libdir
            Rc::clone(&base_env),
            &mut visited,
            0,
            &test_cap_dir,
        );

        // Missing file: canonicalize fails → skipped → base_env returned as-is.
        assert!(
            Rc::ptr_eq(&result_env, &base_env),
            "expected resolve_includes to return the original base_env when file is missing"
        );
        assert!(
            result_bindings.is_empty(),
            "expected no bindings when file is missing"
        );
    }

    /// Verify that `build_type_env` seeds the environment with cap types.
    #[test]
    fn test_build_type_env_has_cap_types() {
        let program = SurfaceProgram { documents: vec![] };
        let (env, _bindings) = build_type_env(&program, None);

        // Check that cap variables are present with correct types
        assert!(env.get("%cwd").is_some(), "expected %cwd in type env");
        assert!(env.get("%libdir").is_some(), "expected %libdir in type env");
        assert!(env.get("%stdin").is_some(), "expected %stdin in type env");

        // Verify types
        use crate::types::Type;
        assert_eq!(env.get("%cwd").unwrap().body, Type::DirCap);
        assert_eq!(env.get("%libdir").unwrap().body, Type::DirCap);
        // %stdin is Handle[Readable Text] (updated to use concrete capability row)
        if let Type::Handle(inner) = &env.get("%stdin").unwrap().body {
            assert!(
                !matches!(inner.as_ref(), Type::Unknown),
                "expected Handle with concrete capability row, got Handle(Unknown)"
            );
        } else {
            panic!("expected Handle type for %stdin");
        }
    }

    /// Verify that `build_type_env` returns binding maps for includes.
    ///
    /// This test verifies Task 1 of the runtime-reflection-include sprint:
    /// resolve_includes returns a mapping from each include call's span to
    /// the bindings it contributed.
    #[test]
    fn test_build_type_env_returns_include_bindings() {
        // Parse a simple LLT file that includes another file
        let source = r#"
            [x: 42]
        "#;
        let program = parser::parse(source).unwrap().program;

        // Without any includes, the binding map should be empty
        let (_env, bindings) = build_type_env(&program, None);
        assert!(
            bindings.is_empty(),
            "expected empty bindings when there are no includes"
        );

        // Future work: test with actual include statements once we have test fixtures
    }

    /// Verify that `apply_include_type_post_pass` injects a Record type for an include call.
    ///
    /// This test constructs an artificial scenario: a program with an include call, and a
    /// manually-built `include_bindings` map that maps the path-argument span to a set of
    /// bindings. The post-pass should inject a `Type::Record` with those bindings at the
    /// call expression's span in the TypeMap.
    #[test]
    fn apply_include_type_post_pass_injects_record_type() {
        use crate::ast::Span;
        use crate::typecheck::TypeMap;
        use crate::types::{Row, Type};
        use std::collections::HashMap;

        // Parse a source with a cap-qualified include call.
        // We use %cwd (cap var) with a path literal "foo.llt".
        let source = r#"[include %cwd "foo.llt"]"#;
        let program = parser::parse(source).unwrap().program;

        // Find the span of the path string argument ("foo.llt") by walking the AST.
        // We need this span as the key into include_bindings.
        let include_paths = collect_include_paths(&program);
        assert_eq!(include_paths.len(), 1, "expected one include path");
        let path_span = include_paths[0].0;

        // Build a fake binding map: path_span → [(name, type)]
        let bindings = vec![
            ("foo".to_string(), Type::Int),
            ("bar".to_string(), Type::Str),
        ];
        let mut include_bindings: HashMap<Span, Vec<(String, Type)>> = HashMap::new();
        include_bindings.insert(path_span, bindings);

        // Run the post-pass on an empty TypeMap.
        let mut type_map = TypeMap::new();
        apply_include_type_post_pass(&program, &include_bindings, &mut type_map);

        // The post-pass should have injected a Record type at the call expression's span.
        // The call expression span is the full "[include %cwd "foo.llt"]" span.
        // Since the source starts at offset 0, the call's span is (0, source.len()).
        let call_key = (0usize, source.len());
        let injected = type_map.get(&call_key);
        assert!(
            injected.is_some(),
            "expected Record type injected at call expression span; type_map keys: {:?}",
            type_map.keys().collect::<Vec<_>>()
        );

        // Check the injected type is a Record with the expected fields.
        match injected.unwrap() {
            Type::Record(Row { fields }) => {
                assert_eq!(
                    fields.get("foo"),
                    Some(&Type::Int),
                    "expected foo: Int in Record"
                );
                assert_eq!(
                    fields.get("bar"),
                    Some(&Type::Str),
                    "expected bar: Str in Record"
                );
                assert_eq!(fields.len(), 2, "expected exactly 2 fields in Record");
            }
            other => panic!("expected Type::Record, got {:?}", other),
        }
    }

    /// Verify that `apply_include_type_post_pass` is a no-op when include_bindings is empty.
    #[test]
    fn apply_include_type_post_pass_empty_bindings_no_change() {
        use crate::ast::Span;
        use crate::typecheck::TypeMap;
        use crate::types::Type;
        use std::collections::HashMap;

        let source = r#"[include %cwd "foo.llt"]"#;
        let program = parser::parse(source).unwrap().program;

        let include_bindings: HashMap<Span, Vec<(String, Type)>> = HashMap::new();
        let mut type_map = TypeMap::new();
        apply_include_type_post_pass(&program, &include_bindings, &mut type_map);

        // No bindings → TypeMap should remain empty.
        assert!(
            type_map.is_empty(),
            "expected TypeMap to remain empty when include_bindings is empty"
        );
    }

    /// Verify that `apply_include_type_post_pass` handles include calls nested in a dict.
    #[test]
    fn apply_include_type_post_pass_nested_in_dict() {
        use crate::ast::Span;
        use crate::typecheck::TypeMap;
        use crate::types::{Row, Type};
        use std::collections::HashMap;

        // The include call is nested inside a dict entry.
        let source = r#"[io: [include %cwd "io.llt"]]"#;
        let program = parser::parse(source).unwrap().program;

        // Find the path-argument span.
        let include_paths = collect_include_paths(&program);
        assert_eq!(include_paths.len(), 1, "expected one include path");
        let path_span = include_paths[0].0;

        let bindings = vec![("read".to_string(), Type::Unknown)];
        let mut include_bindings: HashMap<Span, Vec<(String, Type)>> = HashMap::new();
        include_bindings.insert(path_span, bindings);

        let mut type_map = TypeMap::new();
        apply_include_type_post_pass(&program, &include_bindings, &mut type_map);

        // At least one entry should be in type_map (the include call's Record type).
        assert!(
            !type_map.is_empty(),
            "expected type_map to have the injected Record type for the nested include call"
        );

        // The injected value should be a Record with a "read" field.
        let record_found = type_map
            .values()
            .any(|ty| matches!(ty, Type::Record(Row { fields }) if fields.contains_key("read")));
        assert!(
            record_found,
            "expected a Record with 'read' field in type_map; got: {:?}",
            type_map
        );
    }

    /// Verify that the prelude document type-checks successfully after the `%rust` fix.
    ///
    /// With `%rust` seeded into the builtins env, the prelude document no longer fails
    /// with "undefined variable: %rust". The `merge_env_bindings_into` env-based path
    /// now runs for the prelude, giving prelude functions their inferred types from the
    /// final TypeEnv rather than the TypeMap fallback.
    ///
    /// `identity` is defined as `[fn@[return: a] [let x] x]`. Since its parameter `x`
    /// is unannotated, it gets `Type::Unknown` (not a fresh TypeVar). This makes identity
    /// appear as `Fn@Unknown [Unknown]` — monomorphic with Unknown types. This is the
    /// correct behavior under the current unannotated-params-get-Unknown policy.
    #[test]
    fn build_prelude_env_identity_via_env_path() {
        use crate::types::Type;

        let env = build_prelude_env();
        let scheme = env
            .get("identity")
            .expect("expected 'identity' in prelude env");

        // With %rust seeded, the prelude document type-checks successfully and identity
        // comes from the env-based path (merge_env_bindings_into). The env path gives
        // identity as Fn@Unknown [Unknown] because its param is unannotated → Unknown.
        // This is a Function body (not the fallback Unknown body), confirming env path.
        assert!(
            matches!(scheme.body, Type::Function { .. }),
            "expected 'identity' body to be Function (env path active after %rust fix), \
             got: {:?}",
            scheme.body
        );
    }

    /// Verify that `=` and `<` have their constraint-annotated builtin schemes in the
    /// prelude env, not degraded monomorphic schemes with Unknown params.
    ///
    /// This exercises the builtin-privacy-constraint-hover fix: if the prelude's
    /// type-checking produces a monomorphic (no type_vars) scheme for these operators,
    /// the fallback to the authoritative builtin scheme restores the Equatable/Comparable
    /// constraint for LSP hover display.
    #[test]
    fn build_prelude_env_eq_lt_have_constrained_schemes() {
        let env = build_prelude_env();

        // `=` must be polymorphic (has type_vars) — degraded schemes are monomorphic.
        let eq_scheme = env.get("=").expect("expected '=' in prelude env");
        assert!(
            !eq_scheme.type_vars.is_empty(),
            "expected '=' to have type_vars (Equatable constraint), got monomorphic scheme: {:?}",
            eq_scheme
        );
        // `=` must have an Equatable constraint.
        assert!(
            eq_scheme
                .constraints
                .iter()
                .any(|c| matches!(c, crate::types::Constraint::Class { class, .. } if class.name == "Equatable")),
            "expected '=' to have Equatable constraint, got: {:?}",
            eq_scheme.constraints
        );

        // `<` must be polymorphic (has type_vars).
        let lt_scheme = env.get("<").expect("expected '<' in prelude env");
        assert!(
            !lt_scheme.type_vars.is_empty(),
            "expected '<' to have type_vars (Comparable constraint), got monomorphic scheme: {:?}",
            lt_scheme
        );
        // `<` must have a Comparable constraint.
        assert!(
            lt_scheme
                .constraints
                .iter()
                .any(|c| matches!(c, crate::types::Constraint::Class { class, .. } if class.name == "Comparable")),
            "expected '<' to have Comparable constraint, got: {:?}",
            lt_scheme.constraints
        );
    }
}
