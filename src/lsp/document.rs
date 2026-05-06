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
use crate::typecheck::{typecheck_file_with_types, typecheck_file_with_types_and_env, TypeMap};
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

        if let Ok(ref mut file) = ast {
            // Desugar before type check and eval: rewrites $_ implicit lambdas to explicit forms.
            // This matches the pipeline used by all other entry points (main.rs, repl.rs, lib.rs,
            // builtins.rs). Without this pass the type checker sees VarRef("_") instead of Fn nodes,
            // producing spurious "undefined variable _" errors for any $_ expression.
            crate::desugar::desugar_file(&mut file.node);

            // Variable resolution pass (Phase 1 of arena allocation strategy).
            crate::resolve::resolve_file(&file.node);

            // Run type checker (advisory), collecting the span-to-type map for hover.
            // Seed the type environment with prelude names to suppress false "undefined variable" errors.
            let prelude_names: Vec<&str> = prelude_index
                .name_to_key_span()
                .keys()
                .map(|s| s.as_str())
                .collect();
            let base_env = Rc::new(TypeEnv::with_builtins());
            let seeded_env = Rc::new(base_env.with_prelude_names(&prelude_names));
            let (errs, map) = typecheck_file_with_types_and_env(&file.node, seeded_env);
            type_errors = errs;
            type_map = map;

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
        }

        Self {
            text,
            ast,
            parse_errors,
            type_errors,
            eval_errors,
            type_map,
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
                    Some(prefix.join("share").join("tinct").join("stdlib").join("prelude.llt"))
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

    let mut file = parse_result.node;

    // Desugar before type-checking
    crate::desugar::desugar_file(&mut file);

    // Variable resolution pass
    crate::resolve::resolve_file(&file);

    // Type-check to extract the TypeMap
    let (type_errors, type_map) = typecheck_file_with_types(&file);

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

        self.docs.insert(
            url,
            DocumentState::new(text, &self.stdlib_env, &eval_ctx, &self.prelude_index),
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
        let state = DocumentState::new("[x: 42]".to_string(), &env, &test_ctx(), &test_prelude_index());
        assert!(state.ast.is_ok());
        assert!(state.type_errors.is_empty());
        assert!(state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_state_parse_error() {
        let env = test_env();
        let state = DocumentState::new("[unterminated".to_string(), &env, &test_ctx(), &test_prelude_index());
        assert!(state.ast.is_err());
        assert!(state.type_errors.is_empty());
        assert!(state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_state_type_error() {
        let env = test_env();
        let state = DocumentState::new("[@Number hello]".to_string(), &env, &test_ctx(), &test_prelude_index());
        assert!(state.ast.is_ok());
        assert!(!state.type_errors.is_empty());
        // TypeAssert without default: also errors at runtime on type mismatch.
        assert!(!state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_state_eval_error() {
        let env = test_env();
        let state = DocumentState::new("$undefined".to_string(), &env, &test_ctx(), &test_prelude_index());
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
        let state = DocumentState::new("[call $include \"some_file.llt\"]".to_string(), &env, &ctx, &test_prelude_index());
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
}
