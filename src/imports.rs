//! Import resolution for the type checker.
//!
//! Provides `get_builtin_core_type_env()` which parses and type-checks
//! `stdlib/builtin_core.llt` to build the initial type environment.
//! Also provides LSP-level include resolution (`collect_include_paths`, `build_type_env`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::ast::{Span, SurfaceExpression, SurfaceNode, SurfaceProgram};
use crate::desugar;
use crate::env::Env;
use crate::parser;
use crate::typecheck::{typecheck_surface_program_with_env, TypeMap};
use crate::types::{Row, Type};

/// Type alias for include bindings map: span → list of (name, type) pairs
type IncludeBindings = HashMap<Span, Vec<(String, Type)>>;

/// Depth limit for recursive include resolution (prevents infinite include cycles).
const MAX_INCLUDE_DEPTH: usize = 16;

// Thread-local cache for the builtin_core.llt type environment (T-1366 Rust step 2 bootstrap).
// Populated on first call to `get_builtin_core_type_env()`. Once built, all subsequent
// calls on the same thread return an `Arc::clone` without re-parsing or re-typechecking.
thread_local! {
    static BUILTIN_CORE_TYPE_ENV: RefCell<Option<Arc<RwLock<Env>>>> = const { RefCell::new(None) };
    /// Recursion guard: prevents re-entrant calls from within the typecheck of builtin_core.llt.
    static BUILDING_BUILTIN_CORE_ENV: RefCell<bool> = const { RefCell::new(false) };
}

/// T-1366 Rust step 2 bootstrap: type-check `stdlib/builtin_core.llt` and return the
/// resulting `TypeEnv` so that `Boolean`, `Handle`, `builtin-raise`, etc.
/// are visible to the prelude type-checker by their bare names.
///
/// Uses `include_str!` so the file is embedded at compile time — no runtime libdir access
/// needed. The result is cached thread-locally; subsequent calls return `Arc::clone` in O(1).
///
/// Returns `None` if:
/// - A re-entrant call is detected (recursion guard).
/// - Parsing or resolution fails (rare; the file is compiled-in and known-good).
pub async fn get_builtin_core_type_env() -> Option<Arc<RwLock<Env>>> {
    // Fast path: return cached result.
    let cached = BUILTIN_CORE_TYPE_ENV.with(|c| c.borrow().clone());
    if let Some(env) = cached {
        return Some(env);
    }

    // Recursion guard: prevent re-entrant calls (e.g. from within builtin_core.llt typecheck).
    let already_building = BUILDING_BUILTIN_CORE_ENV.with(|f| *f.borrow());
    if already_building {
        return None;
    }
    BUILDING_BUILTIN_CORE_ENV.with(|f| *f.borrow_mut() = true);

    let result = build_builtin_core_type_env_inner().await;

    BUILDING_BUILTIN_CORE_ENV.with(|f| *f.borrow_mut() = false);

    if let Some(ref env) = result {
        BUILTIN_CORE_TYPE_ENV.with(|c| *c.borrow_mut() = Some(Arc::clone(env)));
    }

    result
}

/// Inner implementation of `get_builtin_core_type_env`.
///
/// Parses `stdlib/builtin_core.llt` (embedded at compile time via `include_str!`),
/// runs the full pipeline (desugar → resolve → typecheck), and returns the
/// resulting `Arc<RwLock<Env>>` with the new type declarations merged on top of
/// `build_builtins_type_env_arc()` as the parent.
async fn build_builtin_core_type_env_inner() -> Option<Arc<RwLock<Env>>> {
    // Embedded source — no libdir access needed at runtime.
    let source = include_str!("../stdlib/builtin_core.llt");
    let sf = Arc::new(crate::ast::SourceFile {
        path: Arc::from("stdlib/builtin_core.llt"),
        content: Arc::from(source),
    });

    // Parse — extract .program from ParseOutput
    let mut program = crate::parser::parse_with_file(source, sf).ok()?.program;

    // Desugar
    crate::desugar::desugar_surface_program(&mut program);

    // Resolve (writes inline to AST nodes).
    // No runtime env at this type-checker bootstrap path; pass None.
    let _resolve_errors = crate::resolve::resolve_surface_program(&program, None);

    // Empty parent — builtin_core.llt is the source of truth. Primitives are hardcoded
    // in resolve_type_name; types declared within the file resolve via state.tycon_env.
    let parent_env = Arc::new(RwLock::new(crate::env::Env::new()));

    // Typecheck with builtins env as parent.
    // enable_scheme_map=false (no LSP hover needed for bootstrap).
    let (_errors, _type_map, _doc_map, _scheme_map, _diagnostics, _state, final_env, _annot) =
        typecheck_surface_program_with_env(
            &program, parent_env, false, // enable_scheme_map
            None,  // resolver_seed_env: no runtime env available at bootstrap
            None,  // type_stage_env: not available at bootstrap
            std::collections::HashMap::new(), // seed_tycon_env: empty at bootstrap
        )
        .await;

    // `final_env` is the child Env containing parent bindings plus new type declarations.
    Some(final_env)
}

/// Replace all TypeVar occurrences in a type with Top.
///
/// TypeVars extracted from the prelude's type_map are stale: they were created
/// in the prelude's InferState and have no meaning in user code's InferState.
/// Leaving them in the prelude env causes CALL-POLY at user call sites, where
/// the first argument binds the TypeVar and subsequent arguments are checked via
/// subsumption against the first argument's type — producing false type errors.
///
/// Replacing stale TypeVars with Top is correct: Top admits any value via subtyping
/// (τ <: Top for all τ), expressing "we don't know the precise type" without
/// activating gradual consistency checking (which Unknown would do).
fn erase_type_vars(ty: &crate::types::Type) -> crate::types::Type {
    use crate::types::{Row, Type};
    match ty {
        Type::TypeVar(_, _) => Type::Any,
        Type::Function {
            params,
            ret,
            variadic,
            required_count,
        } => Type::Function {
            params: params
                .iter()
                .map(|(name, t)| (name.clone(), erase_type_vars(t)))
                .collect(),
            ret: Box::new(erase_type_vars(ret)),
            variadic: *variadic,
            required_count: *required_count,
        },
        Type::Dict(row) => Type::Dict(Row {
            fields: row
                .fields
                .iter()
                .map(|(k, t)| (k.clone(), erase_type_vars(t)))
                .collect(),
            tail: match &row.tail {
                crate::type_def::RowTail::Empty => crate::type_def::RowTail::Empty,
                crate::type_def::RowTail::Uniform { key, value } => {
                    crate::type_def::RowTail::Uniform {
                        key: key.as_ref().map(|k| Box::new(erase_type_vars(k))),
                        value: Box::new(erase_type_vars(value)),
                    }
                }
            },
        }),
        Type::App(f, a) => Type::App(Box::new(erase_type_vars(f)), Box::new(erase_type_vars(a))),
        Type::TyCon(_) => ty.clone(), // TyCon has no type variables
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
                        // Both plain and annotated VarRef use the name field.
                        SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                        _ => None,
                    };
                    if let Some(name) = name {
                        let value_span = entry.node.value.span.clone();
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
                                paths.push((args[1].span.clone(), Some(cap_name.clone()), path.clone()));
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
        SurfaceExpression::Field { expr: Some(target), .. } => {
            collect_include_paths_from_node(target, paths);
        }
        SurfaceExpression::Field { expr: None, .. } => {}
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
        SurfaceExpression::CaseArm { let_bindings, pattern, body } => {
            collect_include_paths_from_node(let_bindings, paths);
            collect_include_paths_from_node(pattern, paths);
            collect_include_paths_from_node(body, paths);
        }
        // Literals and other leaf nodes: no recursive traversal needed
        SurfaceExpression::Int(_)
        | SurfaceExpression::U64(_)
        | SurfaceExpression::Float(_)
        | SurfaceExpression::Str(_)
        | SurfaceExpression::VarRef { .. }  // includes annotated VarRef
        | SurfaceExpression::Placeholder
        | SurfaceExpression::Decl(_) // type-level declaration, no include paths inside
        | SurfaceExpression::Rest(..)
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
#[allow(clippy::too_many_arguments)]
async fn resolve_includes(
    include_paths: &[(Span, Option<String>, String)],
    base_dir: Option<&Path>,
    libdir: Option<&Path>,
    base_env: Arc<RwLock<Env>>,
    visited: &mut HashSet<String>,
    depth: usize,
    cap_dir: &cap_std::fs::Dir,
    prelude_type_env: Arc<RwLock<Env>>,
) -> (Arc<RwLock<Env>>, IncludeBindings) {
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

        // Build a SourceFile so spans in the parsed AST carry the file name.
        let sf = Arc::new(crate::ast::SourceFile {
            path: Arc::from(normalized.to_string_lossy().as_ref()),
            content: Arc::from(content.as_str()),
        });

        // Parse the file, stamping all spans with the SourceFile.
        let parsed = match parser::parse_with_file(&content, sf) {
            Ok(p) => p,
            Err(_) => continue, // Skip unparseable files
        };

        // PIPELINE INVARIANT: parse -> desugar -> resolve.
        let mut program = parsed.program;
        desugar::desugar_surface_program(&mut program);

        // Type-check with the appropriate environment.
        // For %libdir files (stdlib modules), use the prelude TypeEnv as the baseline.
        // Raw builtin-* names are NOT in the outer scope — stdlib modules access them
        // via `--- uses: ["core"]` headers, which inject module-specific type signatures
        // at document level (see typecheck.rs:408-427). This matches the runtime's
        // builtin_module() injection and ensures T002 warnings fire correctly for raw
        // builtin references in user code that omits the --- uses: header.
        // For %cwd files (user includes), use the accumulated user env as normal.
        let typecheck_env: Arc<RwLock<Env>> = if cap_name.as_deref() == Some("%libdir") {
            // Build a child Env for stdlib includes: parent = prelude env,
            // plus %cwd and %libdir capability bindings.
            let child = Arc::new(RwLock::new(Env::with_parent(Arc::clone(&prelude_type_env))));
            {
                let mut guard = child.write().unwrap();
                guard.insert("%cwd".to_string(), crate::types::Type::DirCap);
                guard.insert("%libdir".to_string(), crate::types::Type::DirCap);
            }
            child
        } else {
            Arc::clone(&env)
        };
        let (
            type_errors,
            type_map,
            _doc_map,
            _scheme_map,
            _diagnostics,
            _state,
            _final_env,
            _annot,
        ) = typecheck_surface_program_with_env(&program, typecheck_env, false, None, None, std::collections::HashMap::new()).await;

        // Stdlib includes are user code — their type errors are surfaced like any other.
        if !type_errors.is_empty() {
            let file_path = normalized.to_string_lossy().into_owned();
            for err in &type_errors {
                eprintln!("{}", crate::format_type_error(err, &content, &file_path));
            }
            continue; // Skip bindings from this file; errors have been reported
        }

        // Extract bindings from this program and track them.
        // Create a child Env that extends the current env with the new bindings.
        let child_env = Arc::new(RwLock::new(Env::with_parent(Arc::clone(&env))));
        let bindings = extract_bindings_from_program_as_vec(&program, &type_map);
        {
            let mut guard = child_env.write().unwrap();
            for (name, ty) in &bindings {
                guard.insert(name.clone(), ty.clone());
            }
        }
        env = child_env;

        // Store the bindings for this include call's span
        include_bindings.insert(span.clone(), bindings);

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
            Err(e) => {
                eprintln!(
                    "type-checker include: cannot open parent dir {} for nested includes: {e}",
                    relative_parent.display()
                );
                continue;
            }
        };

        let (nested_env, nested_bindings) = Box::pin(resolve_includes(
            &nested_includes,
            parent_dir,
            libdir,
            env,
            visited,
            depth + 1,
            &nested_cap_dir,
            Arc::clone(&prelude_type_env),
        ))
        .await;
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
/// 2. If found, constructs `Type::Dict(Row { fields })` from the contributed bindings.
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
                            let path_span = args[1].span.clone();
                            if let Some(bindings) = include_bindings.get(&path_span) {
                                // Build a closed Record type from the contributed bindings
                                let fields: indexmap::IndexMap<String, Type> = bindings
                                    .iter()
                                    .map(|(name, ty)| (name.clone(), ty.clone()))
                                    .collect();
                                let record_ty = Type::Dict(Row { fields, tail: crate::type_def::RowTail::Empty });
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
        SurfaceExpression::Field { expr: Some(target), .. } => {
            apply_include_type_to_node(target, include_bindings, type_map);
        }
        SurfaceExpression::Field { expr: None, .. } => {}
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
        SurfaceExpression::CaseArm { let_bindings, pattern, body } => {
            apply_include_type_to_node(let_bindings, include_bindings, type_map);
            apply_include_type_to_node(pattern, include_bindings, type_map);
            apply_include_type_to_node(body, include_bindings, type_map);
        }
        // Leaf nodes: no recursive traversal needed
        SurfaceExpression::Int(_)
        | SurfaceExpression::U64(_)
        | SurfaceExpression::Float(_)
        | SurfaceExpression::Str(_)
        | SurfaceExpression::VarRef { .. }  // includes annotated VarRef
        | SurfaceExpression::Placeholder
        | SurfaceExpression::Decl(_) // type-level declaration, no include paths inside
        | SurfaceExpression::Rest(..)
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
/// 2. Seed the environment with always-available cap variables (`%cwd`, `%libdir`, `%stdin`, `%stdout`)
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
pub async fn build_type_env(
    program: &SurfaceProgram,
    base_dir: Option<&Path>,
) -> (Arc<RwLock<Env>>, IncludeBindings) {
    // Open "." as the base cap dir for type-checking includes
    let cwd_cap = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
        .expect("build_type_env: failed to open CWD as cap dir");
    build_type_env_with_cap(program, base_dir, &cwd_cap).await
}

/// Like `build_type_env`, but also accepts a `cap_std::fs::Dir` for `%cwd` I/O.
///
/// When `base_cap_dir` is `Some`, file reads for `%cwd`-qualified includes go through
/// the cap-std Dir (RESOLVE_BENEATH semantics) instead of plain `std::fs` calls.
/// This provides kernel-level path confinement rather than software-only path checks.
pub async fn build_type_env_with_cap(
    program: &SurfaceProgram,
    base_dir: Option<&Path>,
    cap_dir: &cap_std::fs::Dir,
) -> (Arc<RwLock<Env>>, IncludeBindings) {
    // Seed the environment with the builtin_core type env as the baseline.
    // Falls back to an empty env if unavailable (e.g., re-entrant bootstrap call).
    let prelude_env = get_builtin_core_type_env()
        .await
        .unwrap_or_else(|| Arc::new(RwLock::new(Env::new())));

    // Build a child Env with always-available cap types.
    let env = Arc::new(RwLock::new(Env::with_parent(Arc::clone(&prelude_env))));
    {
        let mut guard = env.write().unwrap();
        guard.insert("%cwd".to_string(), crate::types::Type::DirCap);
        guard.insert("%libdir".to_string(), crate::types::Type::DirCap);
        // %stdin is Handle[Readable Text]
        {
            let mut caps: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
            caps.insert("Readable".to_string(), Type::Any);
            caps.insert("Text".to_string(), Type::Any);
            guard.insert(
                "%stdin".to_string(),
                Type::handle(Type::Dict(Row {
                    fields: caps,
                    tail: crate::type_def::RowTail::Empty,
                })),
            );
        }
        // %stdout — use __cap_flag_* format consistent with write-handle's type expectation.
        // write-handle expects Handle[[__cap_flag_writable: []]] (from cap_flag("writable") in builtins_core.rs).
        // Under BAS width subtyping, Handle[[__cap_flag_writable: [], __cap_flag_text: []]] satisfies it.
        {
            let mut caps: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
            caps.insert(
                "__cap_flag_writable".to_string(),
                Type::Dict(Row {
                    fields: indexmap::IndexMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }),
            );
            caps.insert(
                "__cap_flag_text".to_string(),
                Type::Dict(Row {
                    fields: indexmap::IndexMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }),
            );
            guard.insert(
                "%stdout".to_string(),
                Type::handle(Type::Dict(Row {
                    fields: caps,
                    tail: crate::type_def::RowTail::Empty,
                })),
            );
        }
    }
    let mut env = env;

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
            Arc::clone(&prelude_env),
        )
        .await;
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

    /// Verify that `build_type_env` seeds the environment with cap types.
    #[tokio::test]
    async fn test_build_type_env_has_cap_types() {
        let program = SurfaceProgram { documents: vec![] };
        let (env, _bindings) = build_type_env(&program, None).await;
        let env_guard = env.read().unwrap();

        // Check that cap variables are present with correct types
        assert!(
            env_guard.get_scheme("%cwd").is_some(),
            "expected %cwd in type env"
        );
        assert!(
            env_guard.get_scheme("%libdir").is_some(),
            "expected %libdir in type env"
        );
        assert!(
            env_guard.get_scheme("%stdin").is_some(),
            "expected %stdin in type env"
        );
        assert!(
            env_guard.get_scheme("%stdout").is_some(),
            "expected %stdout in type env"
        );

        // Verify types
        use crate::types::Type;
        assert_eq!(env_guard.get_scheme("%cwd").unwrap().body, Type::DirCap);
        assert_eq!(env_guard.get_scheme("%libdir").unwrap().body, Type::DirCap);
        // %stdin is Handle[Readable Text] (updated to use concrete capability row)
        let stdin_ty = env_guard.get_scheme("%stdin").unwrap().body;
        if let Some(inner) = stdin_ty.as_handle() {
            assert!(
                !matches!(inner, Type::Unknown),
                "expected Handle with concrete capability row, got Handle(Unknown)"
            );
        } else {
            panic!("expected Handle type for %stdin, got: {}", stdin_ty);
        }
        // %stdout is Handle[Writable Text]
        let stdout_ty = env_guard.get_scheme("%stdout").unwrap().body;
        if let Some(inner) = stdout_ty.as_handle() {
            assert!(
                !matches!(inner, Type::Unknown),
                "expected Handle with concrete capability row, got Handle(Unknown)"
            );
        } else {
            panic!("expected Handle type for %stdout, got: {}", stdout_ty);
        }
    }

    /// Verify that `build_type_env` returns binding maps for includes.
    ///
    /// This test verifies Task 1 of the runtime-reflection-include sprint:
    /// resolve_includes returns a mapping from each include call's span to
    /// the bindings it contributed.
    #[tokio::test]
    async fn test_build_type_env_returns_include_bindings() {
        // Parse a simple LLT file that includes another file
        let source = r#"
            [x: 42]
        "#;
        let program = parser::parse(source).unwrap().program;

        // Without any includes, the binding map should be empty
        let (_env, bindings) = build_type_env(&program, None).await;
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
        let path_span = include_paths[0].0.clone();

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
            Type::Dict(Row { fields, .. }) => {
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
        let path_span = include_paths[0].0.clone();

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
            .any(|ty| matches!(ty, Type::Dict(Row { fields, .. }) if fields.contains_key("read")));
        assert!(
            record_found,
            "expected a Record with 'read' field in type_map; got: {:?}",
            type_map
        );
    }
}
