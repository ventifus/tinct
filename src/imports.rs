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
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;

use crate::ast::{Expr, File, Span};
use crate::desugar;
use crate::expand;
use crate::parser;
use crate::resolve;
use crate::typecheck::{typecheck_file_with_types_and_env, TypeMap};
use crate::types::TypeEnv;

/// Depth limit for recursive include resolution (prevents infinite include cycles).
const MAX_INCLUDE_DEPTH: usize = 16;

thread_local! {
    /// Thread-local cache of the prelude type environment.
    /// Built once per thread on first access, then reused for all subsequent calls.
    static PRELUDE_CACHE: RefCell<Option<Rc<TypeEnv>>> = const { RefCell::new(None) };
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

/// Inner implementation of `build_prelude_env()`.
///
/// Parses the embedded prelude source, runs the full type-checking pipeline
/// (expand → desugar → resolve → typecheck), and extracts all top-level binding
/// types into a new `TypeEnv`. Returns the environment even if type errors occur
/// (best-effort approach).
/// Helper function to type-check a stdlib module and extract its bindings into the given env
fn typecheck_and_merge_stdlib_module(
    source: &str,
    parent_env: &Rc<TypeEnv>,
    env: &mut TypeEnv,
) -> Result<(), ()> {
    // Parse the module source
    let file = parser::parse(source).map_err(|_| ())?;

    // Run macro expansion
    let expand_result = expand::expand_macros(file, true).map_err(|_| ())?;
    let mut file = expand_result.file;

    // Desugar and resolve
    desugar::desugar_file(&mut file.node);
    resolve::resolve_file(&file.node);

    // Type-check with the parent environment (builtins + prelude)
    let (type_errors, type_map, _doc_map, _scheme_map) =
        typecheck_file_with_types_and_env(&file.node, Rc::clone(parent_env));

    // Silently ignore type errors
    let _ = type_errors;

    // Extract bindings directly into the provided env
    extract_bindings_from_file(&file.node, &type_map, env);

    Ok(())
}

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
    let builtins_env = Rc::new(builtins_env);

    if typecheck_and_merge_stdlib_module(prelude_source, &builtins_env, &mut env).is_err() {
        // Parse/expand error: return builtins-only environment (with capability bindings)
        return builtins_env;
    }

    Rc::new(env)
}

/// Extract top-level binding names and their types from a File's type map.
///
/// Walks the File's documents and expressions, looking for dict entries that
/// represent top-level bindings. For each binding, extracts its inferred type
/// from the type_map and inserts it into the provided TypeEnv.
///
/// This mirrors the evaluator's `eval_document` behavior: ALL expressions in a
/// document are processed in order, and each intermediate dict/record extends
/// the environment for subsequent expressions. The last expression's bindings
/// are also extracted (if it's a dict).
fn extract_bindings_from_file(file: &File, type_map: &TypeMap, env: &mut TypeEnv) {
    for doc in &file.documents {
        // Process ALL expressions in the document (not just the last one).
        // This matches what the evaluator does in eval_document: each intermediate
        // expression that produces a dict extends the scope for later expressions.
        for expr in &doc.node.expressions {
            extract_bindings_from_expr(&expr.node, type_map, env);
        }
    }
}

/// Recursively extract bindings from an expression tree.
///
/// Focuses on Dict expressions, which represent letrec scopes. For each dict
/// entry with a string key, look up the entry's inferred type in the type_map
/// and insert it into the TypeEnv.
fn extract_bindings_from_expr(expr: &Expr, type_map: &TypeMap, env: &mut TypeEnv) {
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
                        // Look up the value's inferred type in the type_map
                        let value_span = entry.node.value.span;
                        let key = (value_span.start.offset, value_span.end.offset);
                        if let Some(ty) = type_map.get(&key) {
                            env.insert(name, ty.clone());
                        }
                    }
                }
            }
        }
        // Sequential expressions: process ALL expressions in order.
        // Each intermediate dict extends the environment, just like in eval_document.
        Expr::Sequential(exprs) => {
            for expr in exprs {
                extract_bindings_from_expr(&expr.node, type_map, env);
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
        Expr::DefMacro { transformer, .. } => {
            collect_include_paths_from_expr(&transformer.node, paths);
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
        Expr::ClassDecl { methods, .. } | Expr::InstanceDecl { methods, .. } => {
            for method in methods {
                if let Some(ref key) = method.node.key {
                    collect_include_paths_from_expr(&key.node, paths);
                }
                collect_include_paths_from_expr(&method.node.value.node, paths);
            }
        }
        // Literals and other leaf nodes: no recursive traversal needed
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::VarRef { .. }
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
/// Returns `base_env` unchanged on any IO or parse failure (best-effort approach).
/// Depth is capped at `MAX_INCLUDE_DEPTH` to prevent runaway recursion.
fn resolve_includes(
    include_paths: &[(Span, Option<String>, String)],
    base_dir: Option<&Path>,
    libdir: Option<&Path>,
    base_env: Rc<TypeEnv>,
    visited: &mut HashSet<String>,
    depth: usize,
) -> Rc<TypeEnv> {
    if depth >= MAX_INCLUDE_DEPTH {
        // Depth limit reached: return base_env unchanged
        return base_env;
    }

    let mut env = base_env;

    for (_span, cap_name, path) in include_paths {
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
        let path_key = normalized.to_string_lossy().to_string();

        // Check for cycles
        if visited.contains(&path_key) {
            continue;
        }
        visited.insert(path_key);

        // Enforce the same 10 MB limit as the runtime $include.
        let metadata = match std::fs::metadata(&normalized) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.len() > crate::builtins::MAX_FILE_SIZE {
            continue;
        }

        // Read the file
        let content = match std::fs::read_to_string(&normalized) {
            Ok(c) => c,
            Err(_) => continue, // Skip unreadable files
        };

        // Parse the file
        let file = match parser::parse(&content) {
            Ok(f) => f,
            Err(_) => continue, // Skip unparseable files
        };

        // Run macro expansion (tolerate errors)
        let expand_result = match expand::expand_macros(file, true) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut file = expand_result.file;

        // Desugar and resolve
        desugar::desugar_file(&mut file.node);
        resolve::resolve_file(&file.node);

        // Type-check with the current accumulated environment
        let (_type_errors, type_map, _doc_map, _scheme_map) =
            typecheck_file_with_types_and_env(&file.node, Rc::clone(&env));

        // Extract bindings from this file
        let mut new_env = TypeEnv::with_parent(&env);
        extract_bindings_from_file(&file.node, &type_map, &mut new_env);
        env = Rc::new(new_env);

        // Recursively resolve includes from this file
        let nested_includes = collect_include_paths(&file.node);
        let parent_dir = normalized.parent();
        env = resolve_includes(
            &nested_includes,
            parent_dir,
            libdir,
            env,
            visited,
            depth + 1,
        );
    }

    env
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
/// Returns the accumulated environment. Best-effort: IO failures, parse errors,
/// and type errors are silently ignored.
pub fn build_type_env(file: &File, base_dir: Option<&Path>) -> Rc<TypeEnv> {
    let prelude_env = build_prelude_env();

    // Seed with always-available cap types
    let mut env = TypeEnv::with_parent(&prelude_env);
    env.insert("%pwd".to_string(), crate::types::Type::DirCap);
    env.insert("%libdir".to_string(), crate::types::Type::DirCap);
    env.insert("%stdin".to_string(), crate::types::Type::Handle);
    let mut env = Rc::new(env);

    if let Some(dir) = base_dir {
        let include_paths = collect_include_paths(file);
        let mut visited = HashSet::new();
        let libdir = crate::find_libdir_path();
        env = resolve_includes(
            &include_paths,
            Some(dir),
            libdir.as_deref(),
            env,
            &mut visited,
            0,
        );
    }

    env
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
        let paths = collect_include_paths(&file.node);
        assert_eq!(paths.len(), 1, "expected exactly one include path");
        assert_eq!(paths[0].1, Some("%pwd".to_string()));
        assert_eq!(paths[0].2, "foo.llt");
    }

    /// Verify that `collect_include_paths` skips deprecated 1-arg includes.
    #[test]
    fn collect_include_paths_skips_bare_includes() {
        let source = r#"[include "foo.llt"]"#;
        let file = parser::parse(source).unwrap();
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

        let result = resolve_includes(
            &include_paths,
            Some(tmp.as_path()),
            None, // no libdir
            Rc::clone(&base_env),
            &mut visited,
            0,
        );

        // Missing file: canonicalize fails → skipped → base_env returned as-is.
        assert!(
            Rc::ptr_eq(&result, &base_env),
            "expected resolve_includes to return the original base_env when file is missing"
        );
    }

    /// Verify that `build_type_env` seeds the environment with cap types.
    #[test]
    fn test_build_type_env_has_cap_types() {
        let file = File { documents: vec![] };
        let env = build_type_env(&file, None);

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
}
