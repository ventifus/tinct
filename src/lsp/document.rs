//! Document storage and incremental re-parsing for the LSP.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use lsp_types::Url;

use crate::ast::{File, Spanned};
use crate::builtins::create_stdlib_env;
use crate::error::EvalError;
use crate::eval::{eval_file, materialize};
use crate::parser::{parse, ParseError};
use crate::typecheck::{typecheck_file_with_types, TypeMap};
use crate::types::TypeError;
use crate::value::Environment;

/// The parsed and analyzed state of a single document.
#[derive(Debug, Clone)]
pub struct DocumentState {
    /// The original source text.
    pub text: String,
    /// Parsed AST (if parsing succeeded).
    pub ast: Result<Spanned<File>, ParseError>,
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
    pub fn new(
        text: String,
        stdlib_env: &Rc<RefCell<Environment>>,
        eval_ctx: &Rc<crate::eval::EvalContext>,
    ) -> Self {
        let mut ast = parse(&text);
        let mut type_errors = Vec::new();
        let mut eval_errors = Vec::new();
        let mut type_map = TypeMap::new();

        if let Ok(ref mut file) = ast {
            // Desugar before type check and eval: rewrites $_ implicit lambdas to explicit forms.
            // This matches the pipeline used by all other entry points (main.rs, repl.rs, lib.rs,
            // builtins.rs). Without this pass the type checker sees VarRef("_") instead of Fn nodes,
            // producing spurious "undefined variable _" errors for any $_ expression.
            crate::desugar::desugar_file(&mut file.node);

            // Run type checker (advisory), collecting the span-to-type map for hover.
            let (errs, map) = typecheck_file_with_types(&file.node);
            type_errors = errs;
            type_map = map;

            // Attempt evaluation to catch runtime errors early (child scope of cached stdlib env).
            // Materialize the result to force lazy thunks — errors like IncludeForbidden only
            // surface when the thunk is forced, not during the initial (lazy) eval_file call.
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
            type_errors,
            eval_errors,
            type_map,
        }
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
    /// Evaluation context for LSP sessions.
    eval_ctx: Rc<crate::eval::EvalContext>,
}

impl DocumentStore {
    pub fn new() -> Self {
        // Load stdlib once. If it fails, fall back to an empty environment
        // so the LSP can still provide parsing/type-checking diagnostics.
        let stdlib_env =
            create_stdlib_env().unwrap_or_else(|_| Rc::new(RefCell::new(Environment::new())));
        // Create evaluation context (current directory for LSP, sandboxed).
        // no_fs=true prevents executing $include with user-controlled paths when
        // opening malicious .llt files in an editor (CWE-22 path traversal mitigation).
        let eval_ctx = crate::eval::EvalContext::new(
            std::path::PathBuf::from("."),
            Rc::clone(&stdlib_env),
            true,
        );
        Self {
            docs: HashMap::new(),
            stdlib_env,
            eval_ctx,
        }
    }

    /// Update or insert a document, re-parsing and re-analyzing the text.
    pub fn update_document(&mut self, url: Url, text: String) {
        self.docs.insert(
            url,
            DocumentState::new(text, &self.stdlib_env, &self.eval_ctx),
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
        crate::eval::EvalContext::new(std::path::PathBuf::from("."), test_env(), true)
    }

    #[test]
    fn test_document_state_valid_source() {
        let env = test_env();
        let state = DocumentState::new("[x: 42]".to_string(), &env, &test_ctx());
        assert!(state.ast.is_ok());
        assert!(state.type_errors.is_empty());
        assert!(state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_state_parse_error() {
        let env = test_env();
        let state = DocumentState::new("[unterminated".to_string(), &env, &test_ctx());
        assert!(state.ast.is_err());
        assert!(state.type_errors.is_empty());
        assert!(state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_state_type_error() {
        let env = test_env();
        let state = DocumentState::new("[@Number hello]".to_string(), &env, &test_ctx());
        assert!(state.ast.is_ok());
        assert!(!state.type_errors.is_empty());
        // TypeAssert without default: also errors at runtime on type mismatch.
        assert!(!state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_state_eval_error() {
        let env = test_env();
        let state = DocumentState::new("$undefined".to_string(), &env, &test_ctx());
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
        let state = DocumentState::new("[f: $_]".to_string(), &env, &ctx);
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
        let state = DocumentState::new("[call $include \"some_file.llt\"]".to_string(), &env, &ctx);
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
}
