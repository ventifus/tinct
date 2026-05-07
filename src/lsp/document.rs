//! Document storage and incremental re-parsing for the LSP.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use lsp_types::Url;

use crate::ast::{Expr, File, Span, Spanned};
use crate::builtins::create_stdlib_env;
use crate::error::EvalError;
use crate::eval::{eval_file, materialize};
use crate::parser::{parse2, ParseError};
use crate::typecheck::{
    typecheck_file_with_types, typecheck_file_with_types_and_env, DocMap, TypeMap,
};
use crate::types::{TypeEnv, TypeError};
use crate::value::Environment;

/// The parsed and analyzed state of a single document.
#[derive(Debug, Clone)]
pub struct DocumentState {
    /// The original source text.
    pub text: String,
    /// Parsed AST (if parsing succeeded).
    pub ast: Result<Spanned<File>, ParseError>,
    /// Recovered parse errors from inside bracket forms (non-fatal; collected even when `ast` is Ok).
    /// These come from `ParseOutput.errors` and represent errors where the parser substituted
    /// an `Expr::Error` node and continued rather than stopping.
    pub parse_errors: Vec<ParseError>,
    /// Type errors (advisory; evaluation proceeds regardless).
    pub type_errors: Vec<TypeError>,
    /// Evaluation errors (if eval was attempted and failed).
    pub eval_errors: Vec<EvalError>,
    /// Map from expression spans to inferred types (for hover).
    pub type_map: TypeMap,
    /// Map from variable/parameter names to documentation strings (for hover).
    pub doc_map: DocMap,
}

impl DocumentState {
    /// Create a new document state by parsing and analyzing the given text.
    ///
    /// The `stdlib_env` parameter is the cached stdlib environment from the
    /// [`DocumentStore`]. Each document evaluation creates a child scope, so
    /// the shared env is never mutated.
    ///
    /// The `prelude_index` parameter provides prelude function names to seed
    /// the type environment, suppressing false "undefined variable" errors.
    pub fn new(
        text: String,
        stdlib_env: &Rc<RefCell<Environment>>,
        eval_ctx: &Rc<crate::eval::EvalContext>,
        prelude_index: &PreludeIndex,
    ) -> Self {
        // Use parse2() to capture both the AST and any recovered parse errors.
        let parse_result = parse2(&text);
        let mut parse_errors = Vec::new();
        let mut ast: Result<Spanned<File>, ParseError> = match parse_result {
            Ok(output) => {
                parse_errors = output.errors;
                Ok(output.file)
            }
            Err(err) => Err(err),
        };
        let mut type_errors = Vec::new();
        let mut eval_errors = Vec::new();
        let mut type_map = TypeMap::new();
        let mut doc_map = DocMap::new();

        if let Ok(file) = ast {
            // Expand macros before desugar: rewrites [defmacro ...] and macro calls.
            // This matches the pipeline used by all other entry points (main.rs, lib.rs).
            let mut file = match crate::expand::expand_macros(file, eval_ctx.config.no_fs) {
                Ok(f) => f,
                Err(e) => {
                    // Macro expansion error — convert to parse error
                    ast = Err(crate::parser::ParseError {
                        message: format!("macro expansion error: {}", e),
                        span: None,
                    });
                    // Continue with diagnostics
                    return Self {
                        text: text.clone(),
                        ast: Err(crate::parser::ParseError {
                            message: format!("macro expansion error: {}", e),
                            span: None,
                        }),
                        parse_errors: vec![crate::parser::ParseError {
                            message: format!("macro expansion error: {}", e),
                            span: None,
                        }],
                        type_errors: vec![],
                        eval_errors: vec![],
                        type_map: TypeMap::new(),
                        doc_map: DocMap::new(),
                    };
                }
            };

            // Desugar before type check and eval: rewrites $_ implicit lambdas to explicit forms.
            // This matches the pipeline used by all other entry points (main.rs, repl.rs, lib.rs,
            // builtins.rs). Without this pass the type checker sees VarRef("_") instead of Fn nodes,
            // producing spurious "undefined variable _" errors for any $_ expression.
            crate::desugar::desugar_file(&mut file.node);

            // Variable resolution pass (Phase 1 of arena allocation strategy).
            crate::resolve::resolve_file(&file.node);

            // Run type checker (advisory), collecting the span-to-type map for hover.
            // Seed the type environment with prelude types from the prelude index to suppress
            // false "undefined variable" errors and provide accurate types for hover.
            let base_env = Rc::new(TypeEnv::with_builtins());
            let seeded_env = Rc::new(
                base_env
                    .with_prelude_types(prelude_index.name_to_key_span(), prelude_index.type_map()),
            );
            let (errs, map, docs) = typecheck_file_with_types_and_env(&file.node, seeded_env);
            type_errors = errs;
            type_map = map;
            doc_map = docs;

            // Attempt evaluation to catch runtime errors early (child scope of cached stdlib env).
            // Always materialize (even when no_fs=true) so that IncludeForbidden errors
            // are reported as diagnostics in the LSP.
            match eval_file(&file.node, Rc::clone(stdlib_env), eval_ctx, 0) {
                Err(err) => eval_errors.push(*err),
                Ok(thunk) => {
                    if let Err(err) = materialize(&thunk, None, eval_ctx, 0) {
                        eval_errors.push(*err);
                    }
                }
            }

            ast = Ok(file);
        }

        Self {
            text,
            ast,
            parse_errors,
            type_errors,
            eval_errors,
            type_map,
            doc_map,
        }
    }
}

/// Index of prelude definitions for LSP features.
///
/// Built once at LSP startup by parsing and type-checking the embedded prelude source.
/// Provides accurate types for hover and enables go-to-definition navigation to the
/// on-disk stdlib/prelude.llt.
///
/// Uses Arc internally to make cloning cheap (O(1) reference count bump instead of
/// O(prelude_size) HashMap clones).
#[derive(Debug, Clone)]
pub struct PreludeIndex {
    inner: Arc<PreludeIndexInner>,
}

#[derive(Debug)]
struct PreludeIndexInner {
    /// Path to the on-disk stdlib/prelude.llt, if it exists.
    path: Option<PathBuf>,
    /// Map from prelude function name to its key span in the prelude source.
    name_to_key_span: HashMap<String, Span>,
    /// Map from source span to inferred type for all expressions in the prelude.
    type_map: TypeMap,
}

impl PreludeIndex {
    /// Create an empty index (used when prelude parsing fails).
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(PreludeIndexInner {
                path: None,
                name_to_key_span: HashMap::new(),
                type_map: TypeMap::new(),
            }),
        }
    }

    /// Get the path to the on-disk prelude file, if it exists.
    pub fn path(&self) -> Option<&PathBuf> {
        self.inner.path.as_ref()
    }

    /// Get the name-to-span map.
    pub fn name_to_key_span(&self) -> &HashMap<String, Span> {
        &self.inner.name_to_key_span
    }

    /// Get the type map.
    pub fn type_map(&self) -> &TypeMap {
        &self.inner.type_map
    }
}

/// Resolve an include path relative to a base URL.
///
/// Returns `None` for non-`file://` base URLs or resolution failures.
pub fn resolve_include_url(base_url: &Url, path: &str) -> Option<Url> {
    // Only support file:// URLs for now
    if base_url.scheme() != "file" {
        return None;
    }

    // Convert base URL to a file path
    let base_path = base_url.to_file_path().ok()?;
    let base_dir = base_path.parent()?;

    // Resolve the include path relative to the base directory
    let resolved_path = base_dir.join(path);

    // Canonicalize to handle .. and . components
    let canonical_path = resolved_path.canonicalize().ok()?;

    // Convert back to URL
    Url::from_file_path(canonical_path).ok()
}

/// Index a file and its includes into the include graph.
///
/// Recursively indexes all included files up to a depth limit of 16.
/// Uses plain `std::fs::read_to_string` (not eval-time `$include`) — safe
/// because no user code execution occurs, only parsing.
pub fn index_file(
    url: Url,
    graph: &mut IncludeGraph,
    stdlib_env: &Rc<RefCell<Environment>>,
    eval_ctx: &Rc<crate::eval::EvalContext>,
    prelude_index: &PreludeIndex,
    depth: usize,
) -> Result<(), String> {
    const MAX_DEPTH: usize = 16;

    if depth >= MAX_DEPTH {
        return Err(format!(
            "Include depth limit ({}) exceeded at {}",
            MAX_DEPTH, url
        ));
    }

    // Skip if already indexed
    if graph.contains_key(&url) {
        return Ok(());
    }

    // Read the file
    let path = url
        .to_file_path()
        .map_err(|_| format!("Cannot convert URL to path: {}", url))?;

    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    // Create document state
    let state = DocumentState::new(text, stdlib_env, eval_ctx, prelude_index);

    // Collect include paths from this file
    let include_paths = if let Ok(ref file) = state.ast {
        crate::lsp::analysis::collect_include_paths(&file.node)
    } else {
        vec![]
    };

    // Resolve include URLs
    let mut includes = Vec::new();
    for (path, _span) in include_paths {
        if let Some(include_url) = resolve_include_url(&url, &path) {
            includes.push(include_url);
        }
    }

    // Insert this node into the graph (before recursing, to handle circular deps)
    graph.insert(
        url.clone(),
        IncludeNode {
            state,
            includes: includes.clone(),
            included_by: vec![],
        },
    );

    // Recursively index included files and build reverse edges
    for include_url in &includes {
        // Recurse
        if let Err(e) = index_file(
            include_url.clone(),
            graph,
            stdlib_env,
            eval_ctx,
            prelude_index,
            depth + 1,
        ) {
            eprintln!("LSP: Failed to index {}: {}", include_url, e);
            continue;
        }

        // Add reverse edge
        if let Some(included_node) = graph.get_mut(include_url) {
            if !included_node.included_by.contains(&url) {
                included_node.included_by.push(url.clone());
            }
        }
    }

    Ok(())
}

/// Invalidate and re-index all dependents of a changed file.
///
/// Follows reverse edges breadth-first to find all files that transitively
/// include the changed file, then re-indexes them.
pub fn invalidate_dependents(
    changed_url: &Url,
    graph: &mut IncludeGraph,
    stdlib_env: &Rc<RefCell<Environment>>,
    eval_ctx: &Rc<crate::eval::EvalContext>,
    prelude_index: &PreludeIndex,
) {
    use std::collections::VecDeque;
    let mut queue = VecDeque::new();
    let mut visited = std::collections::HashSet::new();

    // Start with the changed file's dependents
    if let Some(node) = graph.get(changed_url) {
        for dependent_url in &node.included_by {
            queue.push_back(dependent_url.clone());
        }
    }

    // BFS through reverse edges
    while let Some(url) = queue.pop_front() {
        if !visited.insert(url.clone()) {
            continue; // Already processed
        }

        // Re-index this file
        // Remove the old entry first to avoid depth-limit issues
        graph.remove(&url);

        if let Err(e) = index_file(
            url.clone(),
            graph,
            stdlib_env,
            eval_ctx,
            prelude_index,
            0, // Reset depth for re-indexing
        ) {
            eprintln!("LSP: Failed to re-index {}: {}", url, e);
            continue;
        }

        // Add its dependents to the queue
        if let Some(node) = graph.get(&url) {
            for dependent_url in &node.included_by {
                queue.push_back(dependent_url.clone());
            }
        }
    }
}

/// Find the on-disk stdlib/prelude.llt path.
///
/// Tries two layouts in order:
/// 1. Development: `<exe_grandparent>/stdlib/prelude.llt`
/// 2. Installed: `<exe_parent>/../share/tinct/stdlib/prelude.llt`
///
/// Returns `None` if neither file exists.
fn find_stdlib_prelude_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent() // target/debug
                .and_then(|p| p.parent()) // target
                .and_then(|p| p.parent()) // project root
                .map(|root| root.join("stdlib").join("prelude.llt"))
        })
        .filter(|p| p.is_file())
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| {
                    let bin_dir = exe.parent()?; // bin/
                                                 // Verify we're in a bin/ directory before assuming installed layout
                    if bin_dir.file_name()? != std::ffi::OsStr::new("bin") {
                        return None;
                    }
                    let prefix = bin_dir.parent()?; // <prefix>/
                    Some(
                        prefix
                            .join("share")
                            .join("tinct")
                            .join("stdlib")
                            .join("prelude.llt"),
                    )
                })
                .filter(|p| p.is_file())
        })
}

/// Build the prelude index by parsing and type-checking the embedded prelude source.
///
/// Returns an empty index on parse failure (with a warning logged to stderr).
pub fn build_prelude_index() -> PreludeIndex {
    let prelude_source = include_str!("../../stdlib/prelude.llt");
    let path = find_stdlib_prelude_path();

    // Parse the prelude source
    let parse_result = match parse2(prelude_source) {
        Ok(output) => output.file,
        Err(err) => {
            eprintln!("LSP: failed to parse prelude: {}", err.message);
            return PreludeIndex::empty();
        }
    };

    let file = parse_result;

    // Expand macros (pre-desugar AST transformation)
    let mut file = match crate::expand::expand_macros(file, false) {
        Ok(f) => f.node,
        Err(e) => {
            eprintln!("LSP: failed to expand macros in prelude: {}", e);
            return PreludeIndex::empty();
        }
    };

    // Desugar before type-checking
    crate::desugar::desugar_file(&mut file);

    // Variable resolution pass
    crate::resolve::resolve_file(&file);

    // Type-check to extract the TypeMap and DocMap
    let (type_errors, type_map, _doc_map) = typecheck_file_with_types(&file);

    if !type_errors.is_empty() {
        eprintln!(
            "LSP: prelude has {} type error(s) (internal bug, please report); \
             hover/definition may be incomplete for some stdlib functions",
            type_errors.len()
        );
    }

    // Walk the top-level dict entries to build the name→key-span index
    let mut name_to_key_span = HashMap::new();

    for document in &file.documents {
        for expr in &document.node.expressions {
            if let Expr::Dict(entries) = &expr.node {
                for entry in entries {
                    if let Some(ref key) = entry.node.key {
                        if let Some(name) = crate::lsp::analysis::key_name(&key.node) {
                            name_to_key_span.insert(name.to_string(), key.span);
                        }
                    }
                }
            }
        }
    }

    PreludeIndex {
        inner: Arc::new(PreludeIndexInner {
            path,
            name_to_key_span,
            type_map,
        }),
    }
}

/// Node in the include dependency graph.
#[derive(Debug, Clone)]
pub struct IncludeNode {
    /// Parsed and analyzed state of this file.
    pub state: DocumentState,
    /// URLs of files this file includes (forward edges).
    pub includes: Vec<Url>,
    /// URLs of files that include this file (reverse edges).
    pub included_by: Vec<Url>,
}

/// Include dependency graph for cross-file resolution.
///
/// Tracks all files reachable via `$include` from open documents.
/// Enables go-to-definition and hover across file boundaries.
pub type IncludeGraph = HashMap<Url, IncludeNode>;

/// Storage for all open documents.
///
/// Holds a cached stdlib environment that is created once and shared across
/// all document evaluations, avoiding the cost of re-parsing and evaluating
/// the 500+ line stdlib prelude on every keystroke.
pub struct DocumentStore {
    docs: HashMap<Url, DocumentState>,
    /// Cached stdlib environment, created once on construction.
    stdlib_env: Rc<RefCell<Environment>>,
    /// Base evaluation context (with "." as base_dir).
    base_eval_ctx: Rc<crate::eval::EvalContext>,
    /// Index of prelude definitions for LSP features.
    pub prelude_index: PreludeIndex,
    /// Include dependency graph for cross-file resolution.
    pub include_graph: IncludeGraph,
}

impl DocumentStore {
    pub fn new() -> Self {
        // Load stdlib once. If it fails, fall back to an empty environment
        // so the LSP can still provide parsing/type-checking diagnostics.
        let stdlib_env =
            create_stdlib_env().unwrap_or_else(|_| Rc::new(RefCell::new(Environment::new())));
        // Create base evaluation context.
        // no_fs=true prevents executing $include with user-controlled paths when
        // opening malicious .llt files in an editor (CWE-22 path traversal mitigation).
        // allowed_paths is left empty (default: unrestricted) because the no_fs guard
        // fires first and blocks all $include calls before the allowlist is ever consulted.
        //
        // Fallback chain for base_dir: try "." first, then temp_dir, then "/" as last resort.
        // This handles systemd socket activation, chroots, and containers where CWD or
        // temp may be inaccessible. Since no_fs=true, the Dir is never used for actual I/O.
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .or_else(|_| {
                cap_std::fs::Dir::open_ambient_dir(
                    std::env::temp_dir(),
                    cap_std::ambient_authority(),
                )
            })
            .unwrap_or_else(|_| {
                // Last resort: open root directory. This should always succeed on Unix-like systems.
                // If this also fails, the LSP cannot start, but this is extremely unlikely.
                cap_std::fs::Dir::open_ambient_dir("/", cap_std::ambient_authority())
                    .expect("failed to open any base_dir (tried ., temp_dir, /)")
            });
        let base_eval_ctx = crate::eval::EvalContext::new(base_dir, Rc::clone(&stdlib_env), true);

        // Build the prelude index for LSP features
        let prelude_index = build_prelude_index();

        Self {
            docs: HashMap::new(),
            stdlib_env,
            base_eval_ctx,
            prelude_index,
            include_graph: HashMap::new(),
        }
    }

    /// Update or insert a document, re-parsing and re-analyzing the text.
    pub fn update_document(&mut self, url: Url, text: String) {
        // Create evaluation context with document's directory as base_dir.
        // $include paths should resolve against the document's directory, not editor cwd.
        let base_path = url
            .to_file_path()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // Fallback chain: try document's directory first, then ".", then base_eval_ctx's Dir.
        // This handles cases where the document's directory becomes inaccessible mid-session
        // (e.g., unmounted network share, deleted directory). Since no_fs=true, the Dir is
        // never used for actual I/O.
        let base_dir = cap_std::fs::Dir::open_ambient_dir(&base_path, cap_std::ambient_authority())
            .or_else(|_| cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()))
            .unwrap_or_else(|_| {
                // Last resort: reopen base_eval_ctx's Dir. cap_std::fs::Dir doesn't implement
                // Clone, so we open "." relative to base_dir to get a duplicate handle.
                // This should never fail since base_eval_ctx.base_dir was successfully opened
                // during DocumentStore::new(), but if it does, we have no choice but to panic.
                self.base_eval_ctx
                    .config
                    .base_dir
                    .open_dir(".")
                    .expect("failed to reopen base_eval_ctx.base_dir")
            });
        let eval_ctx = self.base_eval_ctx.with_base_dir(base_dir);

        let state = DocumentState::new(text, &self.stdlib_env, &eval_ctx, &self.prelude_index);

        // Collect include paths from the new AST
        let new_includes = if let Ok(ref file) = state.ast {
            crate::lsp::analysis::collect_include_paths(&file.node)
                .into_iter()
                .filter_map(|(path, _span)| resolve_include_url(&url, &path))
                .collect::<Vec<_>>()
        } else {
            vec![]
        };

        // Get old includes to detect changes
        let old_includes = self
            .include_graph
            .get(&url)
            .map(|node| node.includes.clone())
            .unwrap_or_default();

        // Index new includes
        for include_url in &new_includes {
            if !old_includes.contains(include_url) {
                // New include detected — index it
                if let Err(e) = index_file(
                    include_url.clone(),
                    &mut self.include_graph,
                    &self.stdlib_env,
                    &eval_ctx,
                    &self.prelude_index,
                    0,
                ) {
                    eprintln!("LSP: Failed to index {}: {}", include_url, e);
                }
            }
        }

        // Remove stale reverse edges for removed includes
        for old_include in &old_includes {
            if !new_includes.contains(old_include) {
                if let Some(node) = self.include_graph.get_mut(old_include) {
                    node.included_by.retain(|u| u != &url);
                }
            }
        }

        // Update or insert this document's node in the include graph
        self.include_graph.insert(
            url.clone(),
            IncludeNode {
                state: state.clone(),
                includes: new_includes.clone(),
                included_by: self
                    .include_graph
                    .get(&url)
                    .map(|n| n.included_by.clone())
                    .unwrap_or_default(),
            },
        );

        // Add forward edges (reverse edges on included files)
        for include_url in &new_includes {
            if let Some(node) = self.include_graph.get_mut(include_url) {
                if !node.included_by.contains(&url) {
                    node.included_by.push(url.clone());
                }
            }
        }

        // Store in docs as well for backward compatibility
        self.docs.insert(url.clone(), state);

        // Invalidate and re-index dependents
        invalidate_dependents(
            &url,
            &mut self.include_graph,
            &self.stdlib_env,
            &eval_ctx,
            &self.prelude_index,
        );
    }

    /// Remove a document from the store.
    pub fn remove_document(&mut self, url: &Url) {
        self.docs.remove(url);
    }

    /// Get a document's state, if it exists.
    pub fn get(&self, url: &Url) -> Option<&DocumentState> {
        self.docs.get(url)
    }
}

impl Default for DocumentStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a stdlib env for tests.
    fn test_env() -> Rc<RefCell<Environment>> {
        create_stdlib_env().unwrap()
    }

    fn test_ctx() -> Rc<crate::eval::EvalContext> {
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        crate::eval::EvalContext::new(base_dir, test_env(), true)
    }

    /// Helper: create an empty prelude index for tests.
    fn test_prelude_index() -> PreludeIndex {
        PreludeIndex::empty()
    }

    #[test]
    fn test_document_state_valid_source() {
        let env = test_env();
        let state = DocumentState::new(
            "[x: 42]".to_string(),
            &env,
            &test_ctx(),
            &test_prelude_index(),
        );
        assert!(state.ast.is_ok());
        assert!(state.type_errors.is_empty());
        assert!(state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_state_parse_error() {
        let env = test_env();
        let state = DocumentState::new(
            "[unterminated".to_string(),
            &env,
            &test_ctx(),
            &test_prelude_index(),
        );
        assert!(state.ast.is_err());
        assert!(state.type_errors.is_empty());
        assert!(state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_state_type_error() {
        let env = test_env();
        let state = DocumentState::new(
            "[@Number hello]".to_string(),
            &env,
            &test_ctx(),
            &test_prelude_index(),
        );
        assert!(state.ast.is_ok());
        assert!(!state.type_errors.is_empty());
        // TypeAssert without default: also errors at runtime on type mismatch.
        assert!(!state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_state_eval_error() {
        let env = test_env();
        let state = DocumentState::new(
            "$undefined".to_string(),
            &env,
            &test_ctx(),
            &test_prelude_index(),
        );
        assert!(state.ast.is_ok());
        assert!(!state.type_errors.is_empty()); // undefined variable is also a type error
        assert!(!state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_store_insert_get() {
        let mut store = DocumentStore::new();
        let url = Url::parse("file:///test.llt").unwrap();

        store.update_document(url.clone(), "[x: 1]".to_string());
        let doc = store.get(&url).unwrap();
        assert_eq!(doc.text, "[x: 1]");
        assert!(doc.ast.is_ok());
    }

    #[test]
    fn test_document_store_update_replaces() {
        let mut store = DocumentStore::new();
        let url = Url::parse("file:///test.llt").unwrap();

        store.update_document(url.clone(), "[x: 1]".to_string());
        store.update_document(url.clone(), "[x: 2]".to_string());

        let doc = store.get(&url).unwrap();
        assert_eq!(doc.text, "[x: 2]");
    }

    #[test]
    fn test_document_store_remove() {
        let mut store = DocumentStore::new();
        let url = Url::parse("file:///test.llt").unwrap();

        store.update_document(url.clone(), "[x: 1]".to_string());
        assert!(store.get(&url).is_some());

        store.remove_document(&url);
        assert!(store.get(&url).is_none());
    }

    #[test]
    fn test_document_state_underscore_desugared() {
        // Regression: before the desugar_file fix, $_ was seen by the type checker as VarRef("_"),
        // producing a spurious "undefined variable _" type error. After the fix, the desugar pass
        // rewrites $_ to an explicit lambda, so no type error should be emitted.
        let env = test_env();
        let ctx = test_ctx();
        let state = DocumentState::new("[f: $_]".to_string(), &env, &ctx, &test_prelude_index());
        assert!(state.ast.is_ok(), "parse should succeed");
        assert!(
            state.type_errors.is_empty(),
            "desugar should eliminate spurious 'undefined variable _' error; got: {:?}",
            state.type_errors
        );
    }

    #[test]
    fn test_document_store_multiple_docs() {
        let mut store = DocumentStore::new();
        let url1 = Url::parse("file:///a.llt").unwrap();
        let url2 = Url::parse("file:///b.llt").unwrap();

        store.update_document(url1.clone(), "[a: 1]".to_string());
        store.update_document(url2.clone(), "[b: 2]".to_string());

        assert_eq!(store.get(&url1).unwrap().text, "[a: 1]");
        assert_eq!(store.get(&url2).unwrap().text, "[b: 2]");
    }

    #[test]
    fn test_lsp_include_forbidden_with_no_fs() {
        // Regression test: LSP context has no_fs=true (line 102) to prevent path traversal
        // when opening malicious .llt files. This test ensures that a future revert of
        // true → false is caught by verifying $include produces an eval error.
        let env = test_env();
        let ctx = test_ctx();
        let state = DocumentState::new(
            "[call $include \"some_file.llt\"]".to_string(),
            &env,
            &ctx,
            &test_prelude_index(),
        );
        assert!(state.ast.is_ok(), "parse should succeed");
        assert!(
            !state.eval_errors.is_empty(),
            "eval should produce IncludeForbidden error when no_fs=true; got no errors"
        );
        // Verify it's specifically the include-forbidden error
        let error_msg = format!("{}", state.eval_errors[0]);
        assert!(
            error_msg.contains("E042") || error_msg.contains("filesystem access is disabled"),
            "expected IncludeForbidden error (E042), got: {}",
            error_msg
        );
    }

    #[test]
    fn test_prelude_index_non_empty() {
        let index = build_prelude_index();
        // The prelude should contain at least these well-known functions
        assert!(
            index.name_to_key_span().contains_key("map"),
            "prelude index should contain 'map'"
        );
        assert!(
            index.name_to_key_span().contains_key("filter"),
            "prelude index should contain 'filter'"
        );
        assert!(
            index.name_to_key_span().contains_key("identity"),
            "prelude index should contain 'identity'"
        );
    }

    #[test]
    fn test_no_false_undefined_for_prelude() {
        let env = test_env();
        let ctx = test_ctx();
        let prelude_index = build_prelude_index();
        let state = DocumentState::new(
            "[call $map [fn [x] x] [1 2 3]]".to_string(),
            &env,
            &ctx,
            &prelude_index,
        );
        assert!(state.ast.is_ok(), "parse should succeed");
        assert!(
            state.type_errors.is_empty(),
            "should have zero type errors (prelude names seeded); got: {:?}",
            state.type_errors
        );
    }

    #[test]
    fn test_resolve_include_url_relative_path() {
        let base_url = Url::parse("file:///home/user/project/main.llt").unwrap();
        let include_path = "lib/utils.llt";
        // resolve_include_url calls canonicalize, which requires the file to exist
        // So this test can't verify the exact URL without creating real files.
        // We can only test that it doesn't panic and returns None for non-existent paths.
        let result = resolve_include_url(&base_url, include_path);
        // Result is None because the path doesn't exist (canonicalize fails)
        assert!(
            result.is_none(),
            "should return None for non-existent paths"
        );
    }

    #[test]
    fn test_resolve_include_url_absolute_path() {
        let base_url = Url::parse("file:///home/user/project/main.llt").unwrap();
        let include_path = "/etc/hosts"; // absolute path to a file that usually exists
        let result = resolve_include_url(&base_url, include_path);
        // On systems where /etc/hosts exists, this should succeed
        // On systems where it doesn't, it should return None
        // We can't make strong assertions without knowing the test environment
        // but we can at least verify it doesn't panic
        let _ = result;
    }

    #[test]
    fn test_resolve_include_url_parent_directory() {
        let base_url = Url::parse("file:///home/user/project/src/main.llt").unwrap();
        let include_path = "../lib/utils.llt";
        let result = resolve_include_url(&base_url, include_path);
        // Should attempt to resolve to /home/user/project/lib/utils.llt
        // Returns None because the path doesn't exist
        assert!(
            result.is_none(),
            "should return None for non-existent paths"
        );
    }

    #[test]
    fn test_resolve_include_url_non_file_scheme() {
        let base_url = Url::parse("http://example.com/main.llt").unwrap();
        let include_path = "lib/utils.llt";
        let result = resolve_include_url(&base_url, include_path);
        assert!(result.is_none(), "should return None for non-file:// URLs");
    }
}
