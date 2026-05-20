//! Import resolution for the type checker.
//!
//! This module provides shared import resolution logic that seeds the type checker
//! with prelude function type signatures. It ensures that `typecheck_source` and
//! `typecheck_file` know about stdlib prelude functions, suppressing false
//! "undefined variable" errors.
//!
//! The prelude environment is built once per thread and cached using thread-local
//! storage. Subsequent calls to `build_prelude_env()` return a cheap `Rc::clone`
//! of the cached environment.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;

use crate::ast::{Expr, File, Span, Spanned};
use crate::desugar;
use crate::expand;
use crate::parser;
use crate::resolve;
use crate::typecheck::{
    typecheck_file_with_types_and_env,
    typecheck_file_with_types_and_env_and_source_returning_state, TypeMap,
};
use crate::types::{ClassEnv, InferState, InstanceEnv, Row, Type, TypeEnv};

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
    static TYPE_STAGE_ENV_CACHE: RefCell<Option<Rc<RefCell<crate::value::Environment>>>> = const { RefCell::new(None) };

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
    let mut file = parser::parse(source).map_err(|_| ())?.file;

    // Skip macro expansion for stdlib modules.
    //
    // Rationale: stdlib modules (prelude.llt, macros.llt) never use [defmacro ...],
    // so expand_macros is a no-op for them — but at depth 0 it triggers a full
    // create_stdlib_env() bootstrap (~20s in debug builds). Since build_prelude_env
    // is called once per test thread, this turns parallel test runs into a hang
    // when each of N threads pays the 20s bootstrap cost simultaneously under
    // memory pressure.
    //
    // The previous code called expand::expand_macros(file, true) here, which
    // recursively built the stdlib just to check for macros that don't exist.

    // Desugar and resolve
    desugar::desugar_file(&mut file.node);
    resolve::resolve_file(&file.node);

    // Type-check with the parent environment (builtins + prelude), capturing InferState
    // and the final TypeEnv (which holds properly generalized TypeSchemes for all prelude
    // bindings — no TypeVar erasure needed).
    //
    // `enable_scheme_map: false` — no LSP hover needed for stdlib modules.
    // `in_prelude_load: true` — skip instance method body inference (optimization).
    let (_type_errors, _type_map, _doc_map, _scheme_map, _diagnostics, state, final_env) =
        typecheck_file_with_types_and_env_and_source_returning_state(
            &file.node,
            Rc::clone(parent_env),
            false,
            true,
        );

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
    extract_bindings_from_file_with_fallback(&file.node, &_type_map, env);

    Ok(state)
}

/// Copy all bindings from `source_env` that are not in `baseline_env` into `target`.
///
/// This is used after type-checking a stdlib module to extract the newly-added bindings
/// (the ones the module introduced) without including the builtins already in baseline_env.
/// The bindings are inserted as TypeSchemes, preserving let-generalization.
fn merge_env_bindings_into(source_env: &TypeEnv, baseline_env: &TypeEnv, target: &mut TypeEnv) {
    // Collect all names visible in source_env
    let mut all_names = std::collections::HashSet::new();
    source_env.collect_all_names(&mut all_names);

    for name in all_names {
        // Skip bindings already present in the baseline (builtins, cap vars)
        if baseline_env.get(&name).is_some() {
            continue;
        }
        // Insert the scheme from source_env into target
        if let Some(scheme) = source_env.get(&name) {
            target.insert_scheme(name, scheme.clone());
        }
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
fn extract_bindings_from_file_with_fallback(file: &File, type_map: &TypeMap, target: &mut TypeEnv) {
    for doc in &file.documents {
        for expr in &doc.node.expressions {
            extract_bindings_fallback_from_expr(&expr.node, type_map, target);
        }
    }
}

/// Recursively extract bindings from an expression tree into `target`, skipping
/// names already present in `target`.
fn extract_bindings_fallback_from_expr(expr: &Expr, type_map: &TypeMap, target: &mut TypeEnv) {
    match expr {
        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(ref key_expr) = entry.node.key {
                    let name = match &key_expr.node {
                        Expr::Str(n) => Some(n.clone()),
                        Expr::VarRef { name, .. } => Some(name.clone()),
                        Expr::Annotated { name, .. } => Some(name.clone()),
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
        Expr::Sequential(exprs) => {
            for expr in exprs {
                extract_bindings_fallback_from_expr(&expr.node, type_map, target);
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
    // Start with builtins
    let mut env = TypeEnv::with_builtins();

    // Inject capability variable types that the CLI always provides.
    // These are runtime-injected by the CLI (see main.rs:905, 955, 934),
    // so the type checker needs to know about them to avoid false "undefined variable" errors.
    env.insert("%pwd".to_string(), crate::types::Type::DirCap);
    env.insert("%libdir".to_string(), crate::types::Type::DirCap);
    env.insert("%stdin".to_string(), crate::types::Type::Handle);

    // Only prelude.llt is loaded at startup.
    // strings.llt, math.llt, and encoding.llt require explicit [include libdir "module.llt"].
    let prelude_source = include_str!("../stdlib/prelude.llt");

    // Type-check prelude
    let mut builtins_env = TypeEnv::with_builtins();
    // Inject capability types into builtins_env for prelude type-checking
    builtins_env.insert("%pwd".to_string(), crate::types::Type::DirCap);
    builtins_env.insert("%libdir".to_string(), crate::types::Type::DirCap);
    builtins_env.insert("%stdin".to_string(), crate::types::Type::Handle);
    // Inject %rust so that [include %rust "group"] calls in prelude.llt don't
    // produce "undefined variable: %rust" errors during type-checking.
    //
    // %rust is a runtime-only pseudo-module (a Value::RustRegistry) with no
    // static type representation — Type::Unknown is genuinely correct here
    // (it's passed to `include` which already accepts and returns Unknown).
    // Without this, the eight [include %rust "..."] sequential expressions at
    // the top of the prelude document each cause a type error, causing
    // typecheck_document to return Err and discard the properly-generalized
    // prelude bindings from the final TypeEnv.
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

    Rc::new(env)
}

/// Seed a fresh [`InferState`] with the prelude's class and instance environments.
///
/// Called at the start of every user-code type-checking session (in `typecheck_file`
/// and `typecheck_file_with_types_and_env_and_source_returning_state`). This ensures
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
pub fn build_type_stage_env() -> Option<Rc<RefCell<crate::value::Environment>>> {
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
            return Some(Rc::clone(env));
        }

        // Set recursion guard
        BUILDING_TYPE_STAGE_ENV.with(|flag| *flag.borrow_mut() = true);

        // Cache miss: build the type-stage environment from scratch
        let result = match crate::builtins::create_type_stage_env() {
            Ok(env) => {
                *cache = Some(Rc::clone(&env));
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

/// Extract top-level binding names and their types from a File's type map as a Vec.
///
/// Like `extract_bindings_from_file`, but returns a vector of (name, type) pairs
/// instead of mutating a TypeEnv. This is used by `resolve_includes` to track
/// which bindings each include contributed.
fn extract_bindings_from_file_as_vec(file: &File, type_map: &TypeMap) -> Vec<(String, Type)> {
    let mut bindings = Vec::new();
    for doc in &file.documents {
        for expr in &doc.node.expressions {
            extract_bindings_from_expr_to_vec(&expr.node, type_map, &mut bindings);
        }
    }
    bindings
}

/// Recursively extract bindings from an expression tree into a Vec.
///
/// Like `extract_bindings_from_expr`, but appends to a vector instead of mutating a TypeEnv.
fn extract_bindings_from_expr_to_vec(
    expr: &Expr,
    type_map: &TypeMap,
    bindings: &mut Vec<(String, Type)>,
) {
    match expr {
        Expr::Dict(entries) => {
            // Extract all top-level bindings from this dict
            for entry in entries {
                // Only process entries with explicit string keys (VarRef or Annotated)
                if let Some(ref key_expr) = entry.node.key {
                    let name = match &key_expr.node {
                        Expr::Str(n) => Some(n.clone()),
                        Expr::VarRef { name, .. } => Some(name.clone()),
                        Expr::Annotated { name, .. } => Some(name.clone()),
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
        Expr::Sequential(exprs) => {
            for expr in exprs {
                extract_bindings_from_expr_to_vec(&expr.node, type_map, bindings);
            }
        }
        _ => {
            // Other expression types don't introduce bindings at the top level
        }
    }
}

/// Collect statically-known include paths from a File.
///
/// Walks the AST looking for `[include ...]` patterns and extracts
/// the string literal paths. Returns a list of `(span, cap_name, path)` tuples
/// where `cap_name` is `Some("%libdir")` or `Some("%pwd")` for cap-qualified includes,
/// or `None` for deprecated bare includes.
///
/// Skips dynamic includes (computed paths).
pub fn collect_include_paths(file: &File) -> Vec<(Span, Option<String>, String)> {
    let mut paths = Vec::new();
    for doc in &file.documents {
        for expr in &doc.node.expressions {
            collect_include_paths_from_expr(&expr.node, &mut paths);
        }
    }
    paths
}

/// Recursively collect include paths from an expression tree.
fn collect_include_paths_from_expr(expr: &Expr, paths: &mut Vec<(Span, Option<String>, String)>) {
    match expr {
        Expr::Call {
            func,
            args,
            named_args: _,
            implied: _,
        } => {
            // Check if this is a call to `include`
            if let Expr::VarRef { name, .. } = &func.node {
                if name == "include" {
                    // Handle 2-arg cap-qualified form: [include %cap "path"]
                    if args.len() == 2 {
                        if let Expr::VarRef { name: cap_name, .. } = &args[0].node {
                            if let Expr::Str(path) = &args[1].node {
                                paths.push((args[1].span, Some(cap_name.clone()), path.clone()));
                            }
                        }
                    }
                    // Deprecated 1-arg form: [include "path"]
                    // Skip this form — cap-qualified is now required
                }
            }
            // Recurse into function and arguments
            collect_include_paths_from_expr(&func.node, paths);
            for arg in args {
                collect_include_paths_from_expr(&arg.node, paths);
            }
        }
        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    collect_include_paths_from_expr(&key.node, paths);
                }
                collect_include_paths_from_expr(&entry.node.value.node, paths);
            }
        }
        Expr::Fn { body, .. } => {
            collect_include_paths_from_expr(&body.node, paths);
        }
        Expr::TypeAssert { expr: inner, .. } => {
            collect_include_paths_from_expr(&inner.node, paths);
        }
        Expr::Pipe { lhs, rhs } => {
            collect_include_paths_from_expr(&lhs.node, paths);
            collect_include_paths_from_expr(&rhs.node, paths);
        }
        Expr::Sequential(exprs) => {
            for e in exprs {
                collect_include_paths_from_expr(&e.node, paths);
            }
        }
        Expr::DotAccess { expr: target, .. } => {
            collect_include_paths_from_expr(&target.node, paths);
        }
        Expr::Quote(inner) | Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
            collect_include_paths_from_expr(&inner.node, paths);
        }
        Expr::TypeAlias { body, .. } => {
            collect_include_paths_from_expr(&body.node, paths);
        }
        Expr::DefMacro { body, .. } => {
            collect_include_paths_from_expr(&body.node, paths);
        }
        Expr::Match { scrutinee, arms } => {
            collect_include_paths_from_expr(&scrutinee.node, paths);
            for arm in arms {
                if let Some(ref guard) = arm.guard {
                    collect_include_paths_from_expr(&guard.node, paths);
                }
                collect_include_paths_from_expr(&arm.body.node, paths);
            }
        }
        Expr::ClassDecl { methods, .. } => {
            for method in methods {
                if let Some(ref key) = method.node.key {
                    collect_include_paths_from_expr(&key.node, paths);
                }
                collect_include_paths_from_expr(&method.node.value.node, paths);
            }
        }
        Expr::InstanceDecl { arms, .. } => {
            for (pattern_expr, methods) in arms {
                collect_include_paths_from_expr(&pattern_expr.node, paths);
                for method in methods {
                    if let Some(ref key) = method.node.key {
                        collect_include_paths_from_expr(&key.node, paths);
                    }
                    collect_include_paths_from_expr(&method.node.value.node, paths);
                }
            }
        }
        Expr::PatternDecl { bindings } => {
            for binding in bindings {
                collect_include_paths_from_expr(&binding.node, paths);
            }
        }
        Expr::LetDecl { bindings } => {
            for binding in bindings {
                collect_include_paths_from_expr(&binding.node, paths);
            }
        }
        Expr::CaseArm { pattern, body } => {
            collect_include_paths_from_expr(&pattern.node, paths);
            collect_include_paths_from_expr(&body.node, paths);
        }
        Expr::MacroDecl { .. } | Expr::Splice(_) | Expr::SyntaxClass { .. } => {
            // Macro declarations are removed by expansion before include resolution
        }
        // Literals and other leaf nodes: no recursive traversal needed
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::VarRef { .. }
        | Expr::Placeholder
        | Expr::Rest(_)
        | Expr::Annotated { .. }
        | Expr::TypeApp { .. }
        | Expr::Error(_) => {}
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
/// - `%pwd` → resolve relative to `base_dir` parameter
/// - Other caps → skip silently (e.g., `%custom_cap` is not supported)
///
/// Returns a tuple of:
/// - The accumulated `TypeEnv` with all included bindings
/// - A mapping from each include call's `Span` to the bindings it contributed
///
/// Returns `base_env` unchanged on any IO or parse failure (best-effort approach).
/// Depth is capped at `MAX_INCLUDE_DEPTH` to prevent runaway recursion.
fn resolve_includes(
    include_paths: &[(Span, Option<String>, String)],
    base_dir: Option<&Path>,
    libdir: Option<&Path>,
    base_env: Rc<TypeEnv>,
    visited: &mut HashSet<String>,
    depth: usize,
    base_cap_dir: Option<&cap_std::fs::Dir>,
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
            Some("%pwd") => base_dir,
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
        // When a cap_std Dir is available for the %pwd base, use it (RESOLVE_BENEATH semantics);
        // fall back to std::fs for %libdir paths or when no cap dir was provided.
        let use_cap = cap_name.as_deref() == Some("%pwd");
        let file_len = if use_cap {
            if let Some(cap_dir) = base_cap_dir {
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

        // Read the file — use cap_std RESOLVE_BENEATH when available for %pwd paths.
        let content = if use_cap {
            if let Some(cap_dir) = base_cap_dir {
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
                    Err(_) => continue,
                }
            }
        } else {
            match std::fs::read_to_string(&normalized) {
                Ok(c) => c,
                Err(_) => continue, // Skip unreadable files
            }
        };

        // Parse the file
        let file = match parser::parse(&content).map(|o| o.file) {
            Ok(f) => f,
            Err(_) => continue, // Skip unparseable files
        };

        // Run macro expansion (tolerate errors).
        // Use the cap_std Dir for expansion when available (avoids re-acquiring ambient authority).
        // AMBIENT-OK: falls back to CWD open only when no cap Dir is provided (lib API boundary).
        let fallback_dir;
        let expand_dir: &cap_std::fs::Dir = match base_cap_dir {
            Some(dir) => dir,
            None => {
                fallback_dir =
                    match cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()) {
                        Ok(d) => d,
                        Err(_) => continue,
                    };
                &fallback_dir
            }
        };
        let expand_result = match expand::expand_macros(file, true, expand_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut file = expand_result.file;

        // Desugar and resolve
        desugar::desugar_file(&mut file.node);
        resolve::resolve_file(&file.node);

        // Type-check with the current accumulated environment
        let (_type_errors, type_map, _doc_map, _scheme_map, _diagnostics) =
            typecheck_file_with_types_and_env(&file.node, Rc::clone(&env));

        // Extract bindings from this file and track them
        let mut new_env = TypeEnv::with_parent(&env);
        let bindings = extract_bindings_from_file_as_vec(&file.node, &type_map);
        for (name, ty) in &bindings {
            new_env.insert(name.clone(), ty.clone());
        }
        env = Rc::new(new_env);

        // Store the bindings for this include call's span
        include_bindings.insert(*span, bindings);

        // Recursively resolve includes from this file.
        // For nested files, we don't pass the cap dir — they resolve against their own
        // parent directory (a different base), so RESOLVE_BENEATH would need a new Dir
        // scoped to that parent. Nested includes fall back to std::fs (path-checked above).
        let nested_includes = collect_include_paths(&file.node);
        let parent_dir = normalized.parent();
        let (nested_env, nested_bindings) = resolve_includes(
            &nested_includes,
            parent_dir,
            libdir,
            env,
            visited,
            depth + 1,
            None,
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
/// Walks the AST as `Spanned<Expr>` nodes to capture call-site spans. For each
/// `[include %cap "path"]` call:
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
/// This post-pass re-walks the AST as `Spanned<Expr>` to recover the call expression's own
/// span, using `args[1].span` as the lookup key.
pub fn apply_include_type_post_pass(
    file: &File,
    include_bindings: &HashMap<Span, Vec<(String, Type)>>,
    type_map: &mut TypeMap,
) {
    for doc in &file.documents {
        for expr in &doc.node.expressions {
            apply_include_type_to_spanned(expr.as_ref(), include_bindings, type_map);
        }
    }
}

/// Recursively walk `Spanned<Expr>` nodes and inject Record types for include calls.
fn apply_include_type_to_spanned(
    spanned: &Spanned<Expr>,
    include_bindings: &HashMap<Span, Vec<(String, Type)>>,
    type_map: &mut TypeMap,
) {
    match &spanned.node {
        Expr::Call {
            func,
            args,
            named_args: _,
            implied: _,
        } => {
            // Check for a cap-qualified include call: [include %cap "path"]
            if let Expr::VarRef { name, .. } = &func.node {
                if name == "include" && args.len() == 2 {
                    if let Expr::VarRef { .. } = &args[0].node {
                        if let Expr::Str(_) = &args[1].node {
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
                                let key = (spanned.span.start.offset, spanned.span.end.offset);
                                type_map.insert(key, record_ty);
                            }
                        }
                    }
                }
            }
            // Recurse into function and arguments (handles nested includes)
            apply_include_type_to_spanned(func, include_bindings, type_map);
            for arg in args {
                apply_include_type_to_spanned(arg.as_ref(), include_bindings, type_map);
            }
        }
        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    apply_include_type_to_spanned(key, include_bindings, type_map);
                }
                apply_include_type_to_spanned(
                    entry.node.value.as_ref(),
                    include_bindings,
                    type_map,
                );
            }
        }
        Expr::Fn { body, .. } => {
            apply_include_type_to_spanned(body.as_ref(), include_bindings, type_map);
        }
        Expr::TypeAssert { expr: inner, .. } => {
            apply_include_type_to_spanned(inner, include_bindings, type_map);
        }
        Expr::Pipe { lhs, rhs } => {
            apply_include_type_to_spanned(lhs, include_bindings, type_map);
            apply_include_type_to_spanned(rhs, include_bindings, type_map);
        }
        Expr::Sequential(exprs) => {
            for e in exprs {
                apply_include_type_to_spanned(e.as_ref(), include_bindings, type_map);
            }
        }
        Expr::DotAccess { expr: target, .. } => {
            apply_include_type_to_spanned(target, include_bindings, type_map);
        }
        Expr::Quote(inner) | Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
            apply_include_type_to_spanned(inner, include_bindings, type_map);
        }
        Expr::TypeAlias { body, .. } => {
            apply_include_type_to_spanned(body, include_bindings, type_map);
        }
        Expr::DefMacro { body, .. } => {
            apply_include_type_to_spanned(body.as_ref(), include_bindings, type_map);
        }
        Expr::Match { scrutinee, arms } => {
            apply_include_type_to_spanned(scrutinee, include_bindings, type_map);
            for arm in arms {
                if let Some(ref guard) = arm.guard {
                    apply_include_type_to_spanned(guard, include_bindings, type_map);
                }
                apply_include_type_to_spanned(&arm.body, include_bindings, type_map);
            }
        }
        Expr::ClassDecl { methods, .. } => {
            for method in methods {
                if let Some(ref key) = method.node.key {
                    apply_include_type_to_spanned(key, include_bindings, type_map);
                }
                apply_include_type_to_spanned(
                    method.node.value.as_ref(),
                    include_bindings,
                    type_map,
                );
            }
        }
        Expr::InstanceDecl { arms, .. } => {
            for (pattern_expr, methods) in arms {
                apply_include_type_to_spanned(pattern_expr, include_bindings, type_map);
                for method in methods {
                    if let Some(ref key) = method.node.key {
                        apply_include_type_to_spanned(key, include_bindings, type_map);
                    }
                    apply_include_type_to_spanned(
                        method.node.value.as_ref(),
                        include_bindings,
                        type_map,
                    );
                }
            }
        }
        Expr::PatternDecl { bindings } => {
            for binding in bindings {
                apply_include_type_to_spanned(binding, include_bindings, type_map);
            }
        }
        Expr::LetDecl { bindings } => {
            for binding in bindings {
                apply_include_type_to_spanned(binding, include_bindings, type_map);
            }
        }
        Expr::CaseArm { pattern, body } => {
            apply_include_type_to_spanned(pattern, include_bindings, type_map);
            apply_include_type_to_spanned(body, include_bindings, type_map);
        }
        Expr::MacroDecl { .. } | Expr::Splice(_) | Expr::SyntaxClass { .. } => {
            // Macro declarations are removed by expansion before type resolution
        }
        // Leaf nodes: no recursive traversal needed
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::VarRef { .. }
        | Expr::Placeholder
        | Expr::Rest(_)
        | Expr::Annotated { .. }
        | Expr::TypeApp { .. }
        | Expr::Error(_) => {}
    }
}

/// Build a type environment seeded with prelude types and optionally with
/// types from statically-resolvable includes.
///
/// This is the main entry point for the LSP and other tools that need a
/// fully-populated type environment for a given file.
///
/// 1. Start with the prelude environment from `build_prelude_env()`
/// 2. Seed the environment with always-available cap variables (`%pwd`, `%libdir`, `%stdin`)
/// 3. If `base_dir` is provided, collect include paths from `file` and
///    resolve them recursively, extending the environment with each included
///    file's top-level bindings.
///
/// Returns a tuple of:
/// - The accumulated `TypeEnv` with all bindings
/// - A mapping from each include call's `Span` to the bindings it contributed
///
/// Best-effort: IO failures, parse errors, and type errors are silently ignored.
pub fn build_type_env(file: &File, base_dir: Option<&Path>) -> (Rc<TypeEnv>, IncludeBindings) {
    build_type_env_with_cap(file, base_dir, None)
}

/// Like `build_type_env`, but also accepts a `cap_std::fs::Dir` for `%pwd` I/O.
///
/// When `base_cap_dir` is `Some`, file reads for `%pwd`-qualified includes go through
/// the cap-std Dir (RESOLVE_BENEATH semantics) instead of plain `std::fs` calls.
/// This provides kernel-level path confinement rather than software-only path checks.
pub fn build_type_env_with_cap(
    file: &File,
    base_dir: Option<&Path>,
    base_cap_dir: Option<&cap_std::fs::Dir>,
) -> (Rc<TypeEnv>, IncludeBindings) {
    let prelude_env = build_prelude_env();

    // Seed with always-available cap types
    let mut env = TypeEnv::with_parent(&prelude_env);
    env.insert("%pwd".to_string(), crate::types::Type::DirCap);
    env.insert("%libdir".to_string(), crate::types::Type::DirCap);
    env.insert("%stdin".to_string(), crate::types::Type::Handle);
    let mut env = Rc::new(env);

    let mut include_bindings = HashMap::new();

    if let Some(dir) = base_dir {
        let include_paths = collect_include_paths(file);
        let mut visited = HashSet::new();
        let libdir = crate::find_libdir_path();
        let (new_env, bindings) = resolve_includes(
            &include_paths,
            Some(dir),
            libdir.as_deref(),
            env,
            &mut visited,
            0,
            base_cap_dir,
        );
        env = new_env;
        include_bindings = bindings;
    }

    (env, include_bindings)
}

#[cfg(test)]
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
    fn test_build_prelude_env_has_builtins() {
        let env = build_prelude_env();
        // Check that builtins are present
        assert!(env.get("+").is_some());
        assert!(env.get("if").is_some());
        assert!(env.get("keys").is_some());
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
        let file = File { documents: vec![] };
        let paths = collect_include_paths(&file);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_collect_include_paths_finds_cap_qualified_includes() {
        let source = r#"
            [include %pwd "foo.llt"]
            [include %libdir "bar.llt"]
        "#;
        let file = parser::parse(source).unwrap().file;
        let paths = collect_include_paths(&file.node);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].1, Some("%pwd".to_string()));
        assert_eq!(paths[0].2, "foo.llt");
        assert_eq!(paths[1].1, Some("%libdir".to_string()));
        assert_eq!(paths[1].2, "bar.llt");
    }

    #[test]
    fn test_collect_include_paths_skips_dynamic_includes() {
        let source = r#"
            [include [str "foo" ".llt"]]
        "#;
        let file = parser::parse(source).unwrap().file;
        let paths = collect_include_paths(&file.node);
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
    /// Both parse to the same `Expr::Call` AST node with `func = VarRef { name: "include" }`.
    #[test]
    fn collect_include_paths_finds_explicit_call_form() {
        let source = r#"[call $include %pwd "foo.llt"]"#;
        let file = parser::parse(source).unwrap().file;
        let paths = collect_include_paths(&file.node);
        assert_eq!(paths.len(), 1, "expected exactly one include path");
        assert_eq!(paths[0].1, Some("%pwd".to_string()));
        assert_eq!(paths[0].2, "foo.llt");
    }

    /// Verify that `collect_include_paths` skips deprecated 1-arg includes.
    #[test]
    fn collect_include_paths_skips_bare_includes() {
        let source = r#"[include "foo.llt"]"#;
        let file = parser::parse(source).unwrap().file;
        let paths = collect_include_paths(&file.node);
        assert_eq!(paths.len(), 0, "bare includes should be skipped");
    }

    /// Verify that `resolve_includes` returns `base_env` unchanged when the
    /// include path does not exist on disk.
    ///
    /// The implementation skips paths whose `canonicalize()` fails, so a missing
    /// file is silently ignored and the original environment is returned unmodified.
    #[test]
    fn resolve_includes_missing_file_returns_base() {
        use crate::ast::Span;

        let base_env = Rc::new(TypeEnv::with_builtins());
        let include_paths = vec![(
            Span::origin(),
            Some("%pwd".to_string()),
            "nonexistent_file_xyz.llt".to_string(),
        )];
        let tmp = std::env::temp_dir();
        let mut visited = HashSet::new();

        let (result_env, result_bindings) = resolve_includes(
            &include_paths,
            Some(tmp.as_path()),
            None, // no libdir
            Rc::clone(&base_env),
            &mut visited,
            0,
            None, // no cap dir in this test
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
        let file = File { documents: vec![] };
        let (env, _bindings) = build_type_env(&file, None);

        // Check that cap variables are present with correct types
        assert!(env.get("%pwd").is_some(), "expected %pwd in type env");
        assert!(env.get("%libdir").is_some(), "expected %libdir in type env");
        assert!(env.get("%stdin").is_some(), "expected %stdin in type env");

        // Verify types
        use crate::types::Type;
        assert_eq!(env.get("%pwd").unwrap().body, Type::DirCap);
        assert_eq!(env.get("%libdir").unwrap().body, Type::DirCap);
        assert_eq!(env.get("%stdin").unwrap().body, Type::Handle);
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
        let file = parser::parse(source).unwrap().file;

        // Without any includes, the binding map should be empty
        let (_env, bindings) = build_type_env(&file.node, None);
        assert!(
            bindings.is_empty(),
            "expected empty bindings when there are no includes"
        );

        // Future work: test with actual include statements once we have test fixtures
    }

    /// Verify that `apply_include_type_post_pass` injects a Record type for an include call.
    ///
    /// This test constructs an artificial scenario: a file with an include call, and a
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
        // We use %pwd (cap var) with a path literal "foo.llt".
        let source = r#"[include %pwd "foo.llt"]"#;
        let file = parser::parse(source).unwrap().file;

        // Find the span of the path string argument ("foo.llt") by walking the AST.
        // We need this span as the key into include_bindings.
        let include_paths = collect_include_paths(&file.node);
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
        apply_include_type_post_pass(&file.node, &include_bindings, &mut type_map);

        // The post-pass should have injected a Record type at the call expression's span.
        // The call expression span is the full "[include %pwd "foo.llt"]" span.
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

        let source = r#"[include %pwd "foo.llt"]"#;
        let file = parser::parse(source).unwrap().file;

        let include_bindings: HashMap<Span, Vec<(String, Type)>> = HashMap::new();
        let mut type_map = TypeMap::new();
        apply_include_type_post_pass(&file.node, &include_bindings, &mut type_map);

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
        let source = r#"[io: [include %pwd "io.llt"]]"#;
        let file = parser::parse(source).unwrap().file;

        // Find the path-argument span.
        let include_paths = collect_include_paths(&file.node);
        assert_eq!(include_paths.len(), 1, "expected one include path");
        let path_span = include_paths[0].0;

        let bindings = vec![("read".to_string(), Type::Unknown)];
        let mut include_bindings: HashMap<Span, Vec<(String, Type)>> = HashMap::new();
        include_bindings.insert(path_span, bindings);

        let mut type_map = TypeMap::new();
        apply_include_type_post_pass(&file.node, &include_bindings, &mut type_map);

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

    /// Verify that `identity` is present in the prelude environment as a Function.
    ///
    /// `identity` is `[fn@[return: a] [let x] x]`. With `%rust` seeded into the
    /// type-checking environment, the prelude document returns `Ok` and identity
    /// is extracted via `merge_env_bindings_into` (the env-based path), giving it
    /// a `Function` body — `Fn@Unknown [Unknown]` because its parameter is unannotated.
    #[test]
    fn build_prelude_env_identity_current_behavior() {
        use crate::types::Type;

        let env = build_prelude_env();
        let scheme = env
            .get("identity")
            .expect("expected 'identity' in prelude env");

        // After the %rust fix, the prelude document type-checks successfully.
        // identity comes from the env-based path (merge_env_bindings_into) as a Function.
        // Its param is unannotated → Unknown, so the body is Fn@Unknown [Unknown].
        assert!(
            matches!(scheme.body, Type::Function { .. }),
            "expected 'identity' body to be Function (env path active after %rust fix), \
             got: {:?}",
            scheme.body
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
}
