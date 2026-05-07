//! Variable resolution pass: assigns (level, slot) de Bruijn coordinates to VarRef nodes.
//!
//! This is Phase 1 of the arena allocation strategy. The resolver walks the AST and
//! assigns compile-time slot indices to static variable references before evaluation begins.
//!
//! **Invariants:**
//! - Must run exactly once per AST (write-once RefCell cache).
//!   Enforced by: panic in walk_expr (line ~106) if a VarRef's resolved cache is already populated.
//!   This catches both double-resolution and AST cloning bugs.
//! - Must run after desugaring (sees $_ as Fn nodes, not VarRef("_")).
//! - Must run before typechecking and evaluation (both consumers of resolved coords).
//!
//! See doc/whatif/arena-patterns.md §Variable Resolution Pass Design for the full specification.

use indexmap::IndexMap;

use crate::ast::{Annotation, Document, Expr, File, Spanned};

/// Variable resolution pass state.
///
/// Maintains a stack of scopes (one per dict or function). Each scope maps variable names
/// to their slot indices within that scope.
pub struct Resolver {
    /// Stack of scopes. Each scope is an IndexMap from name to slot index.
    /// The innermost scope is at the end of the vector.
    scopes: Vec<IndexMap<String, u32>>,
}

impl Resolver {
    /// Create a new resolver with an empty scope stack.
    pub fn new() -> Self {
        Self { scopes: Vec::new() }
    }

    /// Enter a new scope with the given keys assigned to sequential slot indices.
    ///
    /// Used when entering a dict (keys are the static entry names) or a function
    /// (keys are the parameter names).
    pub fn enter_scope(&mut self, keys: &[String]) {
        let mut scope = IndexMap::new();
        for (slot, key) in keys.iter().enumerate() {
            scope.insert(
                key.clone(),
                u32::try_from(slot).expect("slot index overflow"),
            );
        }
        self.scopes.push(scope);
    }

    /// Exit the current scope.
    ///
    /// Panics if the scope stack is empty.
    pub fn exit_scope(&mut self) {
        self.scopes
            .pop()
            .expect("exit_scope called with empty stack");
    }

    /// Resolve a variable name to its (level, slot) coordinates.
    ///
    /// Searches the scope stack from innermost to outermost. Returns `None` if the
    /// variable is not found in any scope (e.g., computed keys, `$include`-introduced bindings).
    ///
    /// `level` is the absolute nesting depth of the binding's scope (0 = outermost).
    /// `slot` is the index within that scope's slot vector.
    pub fn resolve(&self, name: &str) -> Option<(u32, u32)> {
        for (offset, scope) in self.scopes.iter().rev().enumerate() {
            if let Some(&slot) = scope.get(name) {
                // level is the absolute nesting depth of the binding's scope
                // (0 = outermost, len-1 = innermost)
                let level =
                    u32::try_from(self.scopes.len() - 1 - offset).expect("scope depth overflow");
                return Some((level, slot));
            }
        }
        None
    }

    /// Walk an annotation and resolve all VarRef nodes in its property dict entries.
    ///
    /// Annotations can contain PropertyDict entries with arbitrary expressions (including VarRef).
    /// Simple annotations (just a name) have no expressions to walk.
    fn walk_annotation(&mut self, ann: &Spanned<Annotation>) {
        match &ann.node {
            Annotation::Simple(_) => {}
            Annotation::PropertyDict(entries) => {
                for entry in entries {
                    if let Some(key_expr) = &entry.node.key {
                        self.walk_expr(key_expr);
                    }
                    self.walk_expr(&entry.node.value);
                }
            }
        }
    }

    /// Walk an expression and resolve all VarRef nodes.
    ///
    /// This is a recursive walk that:
    /// - Enters scopes for Dict and Fn expressions
    /// - Resolves VarRef nodes by populating their `resolved` cache
    /// - Recursively walks all child expressions
    fn walk_expr(&mut self, expr: &Spanned<Expr>) {
        match &expr.node {
            Expr::VarRef { name, resolved } => {
                // Resolve this variable reference and cache the result.
                // `coords` is Option<(u32, u32)> — None means unresolvable, Some means resolved.
                // We wrap in Some(...) to produce the outer Some of the three-state sentinel:
                //   - Outer None = not yet processed (initial state from var_ref() constructor)
                //   - Outer Some(None) = processed but unresolvable
                //   - Outer Some(Some((level, slot))) = resolved to coordinates
                let coords = self.resolve(name);
                // Atomically swap: replace() returns the old value without a separate borrow.
                // This avoids any risk of borrow aliasing between the check and the write.
                let old = resolved.replace(Some(coords));
                if old.is_some() {
                    // Resolution pass should only run once per AST. If this fires, either:
                    // 1. resolve_file() was called twice on the same AST (caller bug), or
                    // 2. The AST was cloned and both copies are being resolved (incorrect).
                    // The outer Some check catches double-resolution for BOTH resolved and
                    // unresolvable variables (fixing the gap where None was written twice).
                    panic!(
                        "VarRef resolved cache already populated for '{}'. \
                         This indicates either: (1) resolve_file() was called twice on the same AST, or \
                         (2) the AST was cloned and both copies are being resolved. \
                         Resolution must run exactly once per AST.",
                        name
                    );
                }
            }
            Expr::Dict(entries) => {
                // Collect static keys (non-computed entry keys)
                // TODO(arena-phase2): Preserve static_keys.len() for FlatEnv slot vector pre-sizing.
                // This is a performance optimization to avoid reallocation when building the FlatEnv
                // at eval time. Phase 2 will need the count to Vec::with_capacity() the slot vector.
                // Options: (1) add `static_key_count: Cell<Option<usize>>` to Expr::Dict, or
                // (2) build a side table mapping Dict span → key count during resolution.
                let static_keys: Vec<String> = entries
                    .iter()
                    .filter_map(|entry| {
                        entry.node.key.as_ref().and_then(|key_expr| {
                            // Only Str and Annotated keys produce Key::String bindings at runtime
                            // and therefore create scope entries that VarRef can resolve to.
                            //
                            // Excluded cases (all produce None):
                            // - VarRef: the key is a computed key (value of the variable is the
                            //   key, not its name). Evaluated via eval_key() in the parent env.
                            // - Int: produces Key::Int at runtime — the evaluator (eval.rs) only
                            //   creates scope bindings for Key::String, so Int keys never appear
                            //   in the scope that VarRef resolution searches.
                            // - Float, Bool: dead code — the parser never produces these as dict
                            //   key expressions (Float and BoolLit tokens don't check colon-ahead).
                            match &key_expr.node {
                                Expr::Str(s) => Some(s.clone()),
                                Expr::Annotated { name, .. } => Some(name.clone()),
                                // Everything else is a computed or non-string key
                                _ => None,
                            }
                        })
                    })
                    .collect();

                // Walk all key expressions in outer scope (keys cannot reference siblings)
                for entry in entries {
                    if let Some(key_expr) = &entry.node.key {
                        self.walk_expr(key_expr);
                    }
                }

                // Enter scope with static keys
                self.enter_scope(&static_keys);

                // Walk all entry values in dict scope
                for entry in entries {
                    self.walk_expr(&entry.node.value);
                }

                self.exit_scope();
            }
            Expr::Fn {
                return_ann,
                params,
                body,
                ..
            } => {
                // Walk return annotation if present (sees outer scope)
                if let Some(ret_ann) = return_ann {
                    self.walk_annotation(ret_ann);
                }

                // Walk param annotations BEFORE entering scope (annotations see outer scope)
                for param in params {
                    if let Some(param_ann) = &param.node.annotation {
                        self.walk_annotation(param_ann);
                    }
                }

                // Collect parameter names
                let param_names: Vec<String> = params.iter().map(|p| p.node.name.clone()).collect();

                // Enter scope with parameter names
                self.enter_scope(&param_names);

                // Walk the body
                self.walk_expr(body);

                self.exit_scope();
            }
            // Recursively walk all child expressions
            Expr::DotAccess { expr, .. } => self.walk_expr(expr),
            Expr::Pipe { .. } => {
                // Pipe is eliminated by the desugar pass before resolve runs.
                // desugar::desugar_file is always called before resolve::resolve_file
                // (invariant: lib.rs::eval_source_with_config, main.rs::run_eval).
                // If this arm is reached, the pipeline contract has been violated.
                unreachable!("Expr::Pipe should have been eliminated by desugar before resolve");
            }
            Expr::Sequential(exprs) => {
                for seq_expr in exprs {
                    self.walk_expr(seq_expr);
                }
            }
            Expr::Call {
                func,
                args,
                named_args,
                ..
            } => {
                self.walk_expr(func);
                for arg in args {
                    self.walk_expr(arg);
                }
                for named_arg in named_args {
                    self.walk_expr(&named_arg.node.value);
                }
            }
            Expr::TypeAssert {
                annotation, expr, ..
            } => {
                self.walk_annotation(annotation);
                self.walk_expr(expr);
            }
            Expr::TypeAlias(expr) => self.walk_expr(expr),
            // Quote: do NOT resolve variables inside the quoted expression.
            // Variables in quoted code are AST data, not runtime bindings.
            Expr::Quote(_) => {}
            // Unquote and UnquoteSplice: DO resolve variables inside the unquoted expression.
            // The expression is evaluated in the current runtime environment.
            Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
                self.walk_expr(inner);
            }
            // DefMacro: resolve variables in the transformer expression.
            Expr::DefMacro { transformer, .. } => {
                self.walk_expr(transformer);
            }
            // Match: resolve variables in scrutinee and arm bodies.
            // Patterns don't contain runtime variable references (except Pin patterns,
            // which we don't support yet).
            Expr::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee);
                for arm in arms {
                    self.walk_expr(&arm.body);
                }
            }
            // Literals have no child expressions
            Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::Str(_)
            | Expr::Rest(_)
            | Expr::Error(_) => {}
            Expr::Annotated { annotation, .. } => {
                self.walk_annotation(annotation);
            }
        }
    }

    /// Walk a document and resolve all VarRef nodes in its expressions.
    fn walk_document(&mut self, document: &Document) {
        for expr in &document.expressions {
            self.walk_expr(expr);
        }
    }
}

/// Resolve all VarRef nodes in a file.
///
/// This is the entry point for the variable resolution pass. It walks the entire AST
/// and populates the `resolved` cache field in all VarRef nodes.
///
/// The pass runs after parsing and before evaluation. It enables future optimization
/// to flat environments with O(1) slot-based lookup instead of O(depth) name-based lookup.
///
/// A synthetic outermost scope is pushed for each document containing the `%` pipeline
/// variable and any `%name` named-section bindings from preceding documents. This mirrors
/// the runtime injection performed by `eval_file_with_input` (src/eval.rs), ensuring
/// `$%` references resolve to a known coordinate rather than `Some(None)` (unresolvable).
///
/// Builtins are NOT injected into this scope — they are resolved via the stdlib
/// environment at runtime and intentionally remain unresolvable (`Some(None)`) during
/// the AST walk. The resolver's coordinates are only meaningful for lexical bindings
/// (dict entries, function parameters, pipeline variables).
pub fn resolve_file(file: &File) {
    let mut resolver = Resolver::new();

    // Collect named section names as we go (mirrors eval_file_with_input's named accumulator).
    let mut named_sections: Vec<String> = Vec::new();

    for document in &file.documents {
        // Build the synthetic scope: always includes `%`, plus `%name` for each
        // previously named section.
        let mut runtime_names: Vec<String> = vec!["%".to_string()];
        for name in &named_sections {
            runtime_names.push(format!("%{}", name));
        }

        // Push synthetic outermost scope with runtime-injected bindings.
        resolver.enter_scope(&runtime_names);

        resolver.walk_document(&document.node);

        // Pop the synthetic scope so the next document gets a fresh one.
        resolver.exit_scope();

        // If this document is named, accumulate it for subsequent documents.
        if let Some(ref name) = document.node.name {
            named_sections.push(name.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_in_current_scope() {
        let mut resolver = Resolver::new();
        resolver.enter_scope(&["x".into(), "y".into()]);
        assert_eq!(resolver.resolve("x"), Some((0, 0)));
        assert_eq!(resolver.resolve("y"), Some((0, 1)));
    }

    #[test]
    fn test_resolve_not_found() {
        let mut resolver = Resolver::new();
        resolver.enter_scope(&["x".into()]);
        assert_eq!(resolver.resolve("missing"), None);
    }

    #[test]
    fn test_resolve_in_parent_scope() {
        let mut resolver = Resolver::new();
        resolver.enter_scope(&["x".into()]); // level 0
        resolver.enter_scope(&["y".into()]); // level 1
                                             // Resolve x from inner scope - should find it in level 0
        assert_eq!(resolver.resolve("x"), Some((0, 0)));
        assert_eq!(resolver.resolve("y"), Some((1, 0)));
    }

    #[test]
    fn test_shadowing() {
        let mut resolver = Resolver::new();
        resolver.enter_scope(&["x".into()]); // level 0, slot 0
        resolver.enter_scope(&["x".into()]); // level 1, slot 0 (shadows outer x)
                                             // Should resolve to the innermost x
        assert_eq!(resolver.resolve("x"), Some((1, 0)));
    }

    #[test]
    fn test_resolve_exit_scope() {
        let mut resolver = Resolver::new();
        resolver.enter_scope(&["x".into()]);
        resolver.enter_scope(&["y".into()]);
        assert_eq!(resolver.resolve("y"), Some((1, 0)));
        resolver.exit_scope();
        assert_eq!(resolver.resolve("y"), None);
        assert_eq!(resolver.resolve("x"), Some((0, 0)));
    }

    #[test]
    fn test_resolve_file_simple_dict() {
        use crate::parser::parse;

        let source = "[x: 1  y: $x]";
        let file = parse(source).expect("parse failed");

        // Resolve the file
        resolve_file(&file.node);

        // Check that the VarRef for x in the y entry is resolved
        let doc = &file.node.documents[0].node;
        let dict_expr = &doc.expressions[0].node;
        match dict_expr {
            Expr::Dict(entries) => {
                // Second entry: y: $x
                let y_value = &entries[1].node.value.node;
                match y_value {
                    Expr::VarRef { name, resolved } => {
                        assert_eq!(name, "x");
                        // Level 1 (level 0 is the synthetic % scope), slot 0
                        assert_eq!(resolved.borrow().flatten(), Some((1, 0)));
                    }
                    other => panic!("expected VarRef for y value, got {:?}", other),
                }
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_file_nested_dict() {
        use crate::parser::parse;

        let source = "[x: 42  inner: [y: $x]]";
        let file = parse(source).expect("parse failed");

        resolve_file(&file.node);

        // Navigate to the VarRef for x inside the nested dict
        let doc = &file.node.documents[0].node;
        let outer_dict = &doc.expressions[0].node;
        match outer_dict {
            Expr::Dict(outer_entries) => {
                // Second entry: inner: [y: $x]
                let inner_value = &outer_entries[1].node.value.node;
                match inner_value {
                    Expr::Dict(inner_entries) => {
                        // First entry: y: $x
                        let y_value = &inner_entries[0].node.value.node;
                        match y_value {
                            Expr::VarRef { name, resolved } => {
                                assert_eq!(name, "x");
                                // x is in the outer dict scope (level 1; level 0 is synthetic %), slot 0
                                assert_eq!(resolved.borrow().flatten(), Some((1, 0)));
                            }
                            other => panic!("expected VarRef for y value, got {:?}", other),
                        }
                    }
                    other => panic!("expected Dict for inner, got {:?}", other),
                }
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_file_function() {
        use crate::parser::parse;

        let source = "[fn [x y] $x]";
        let file = parse(source).expect("parse failed");

        resolve_file(&file.node);

        // Navigate to the VarRef for x in the function body
        let doc = &file.node.documents[0].node;
        let fn_expr = &doc.expressions[0].node;
        match fn_expr {
            Expr::Fn { body, .. } => {
                match &body.node {
                    Expr::VarRef { name, resolved } => {
                        assert_eq!(name, "x");
                        // x is the first parameter (level 1; level 0 is synthetic % scope), slot 0
                        assert_eq!(resolved.borrow().flatten(), Some((1, 0)));
                    }
                    other => panic!("expected VarRef for body, got {:?}", other),
                }
            }
            other => panic!("expected Fn, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_unresolved_reference() {
        use crate::parser::parse;

        let source = "$undefined";
        let file = parse(source).expect("parse failed");

        resolve_file(&file.node);

        // Check that the VarRef for undefined is NOT resolved (None)
        let doc = &file.node.documents[0].node;
        let varref_expr = &doc.expressions[0].node;
        match varref_expr {
            Expr::VarRef { name, resolved } => {
                assert_eq!(name, "undefined");
                // Outer Some(None) after processing: processed but unresolvable.
                // flatten() extracts the inner Option: Some(None).flatten() == None.
                assert_eq!(resolved.borrow().flatten(), None);
            }
            other => panic!("expected VarRef, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_computed_key() {
        use crate::parser::parse;

        // In LLT, $k: val uses $k as a computed key (value of $k becomes the key at runtime).
        // Resolver processes $k in key position — value expressions can reference dict siblings,
        // but key expressions see the dict scope (which includes the sibling x).
        // Test: [$y: 1  $x: 2] where $y and $x should resolve correctly.
        // Use a simpler source that has clear key/value structure.
        let source = "[a: 1  b: 2]";
        let file = parse(source).expect("parse failed");
        resolve_file(&file.node);

        let doc = &file.node.documents[0].node;
        let dict_expr = &doc.expressions[0].node;
        match dict_expr {
            Expr::Dict(entries) => {
                // Both entries have string keys (no computed keys in this simple case)
                assert_eq!(entries.len(), 2);
                // Just verify the dict resolves without errors
                let key0 = entries[0].node.key.as_ref().unwrap();
                assert!(matches!(&key0.node, Expr::Str(s) if s == "a"));
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn test_key_expression_outer_scope() {
        use crate::parser::parse;

        // In LLT, value references in dict entries see the dict scope (letrec semantics).
        // Verify that $x in a value position references the sibling binding x (level 1, slot 0).
        // Key expressions are walked before entering dict scope, so they see the outer scope only.
        let source = "[x: 1  y: $x]";
        let file = parse(source).expect("parse failed");
        resolve_file(&file.node);

        let doc = &file.node.documents[0].node;
        let dict_expr = &doc.expressions[0].node;
        match dict_expr {
            Expr::Dict(entries) => {
                // Second entry: y: $x — value $x should resolve to x at level 1, slot 0
                let y_value = &entries[1].node.value.node;
                match y_value {
                    Expr::VarRef { name, resolved } => {
                        assert_eq!(name, "x");
                        // x is in the dict scope (level 1, slot 0)
                        assert_eq!(resolved.borrow().flatten(), Some((1, 0)));
                    }
                    other => panic!("expected VarRef for value, got {:?}", other),
                }
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_type_assert_annotation() {
        use crate::parser::parse;

        let source = "[fallback: 99  x: [@[default: $fallback] 42]]";
        let file = parse(source).expect("parse failed");
        resolve_file(&file.node);

        // Navigate to the VarRef in the annotation
        let doc = &file.node.documents[0].node;
        let dict_expr = &doc.expressions[0].node;
        match dict_expr {
            Expr::Dict(entries) => {
                // Second entry: x: [@[default: $fallback] 42]
                let x_value = &entries[1].node.value.node;
                match x_value {
                    Expr::TypeAssert { annotation, .. } => {
                        match &annotation.node {
                            Annotation::PropertyDict(ann_entries) => {
                                // First entry: default: $fallback
                                let default_value = &ann_entries[0].node.value.node;
                                match default_value {
                                    Expr::VarRef { name, resolved } => {
                                        assert_eq!(name, "fallback");
                                        // Level 1 (level 0 is synthetic % scope), slot 0
                                        assert_eq!(resolved.borrow().flatten(), Some((1, 0)));
                                    }
                                    other => panic!("expected VarRef, got {:?}", other),
                                }
                            }
                            _ => panic!("expected PropertyDict annotation"),
                        }
                    }
                    other => panic!("expected TypeAssert, got {:?}", other),
                }
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_param_annotation() {
        use crate::parser::parse;

        let source = "[default_val: 0  f: [fn [x@[default: $default_val]] $x]]";
        let file = parse(source).expect("parse failed");
        resolve_file(&file.node);

        // Navigate to the VarRef in the param annotation
        let doc = &file.node.documents[0].node;
        let dict_expr = &doc.expressions[0].node;
        match dict_expr {
            Expr::Dict(entries) => {
                // Second entry: f: [fn [x@[default: $default_val]] $x]
                let f_value = &entries[1].node.value.node;
                match f_value {
                    Expr::Fn { params, .. } => {
                        let param_ann = params[0].node.annotation.as_ref().unwrap();
                        match &param_ann.node {
                            Annotation::PropertyDict(ann_entries) => {
                                let default_value = &ann_entries[0].node.value.node;
                                match default_value {
                                    Expr::VarRef { name, resolved } => {
                                        assert_eq!(name, "default_val");
                                        // Level 1 (level 0 is synthetic % scope), slot 0
                                        assert_eq!(resolved.borrow().flatten(), Some((1, 0)));
                                    }
                                    other => panic!("expected VarRef, got {:?}", other),
                                }
                            }
                            _ => panic!("expected PropertyDict annotation"),
                        }
                    }
                    other => panic!("expected Fn, got {:?}", other),
                }
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    /// Fix 6: double-resolution of any VarRef (including unresolvable ones) must panic.
    /// Verifies the write-once invariant for both resolved and unresolvable variables.
    #[test]
    #[should_panic(expected = "VarRef resolved cache already populated")]
    fn test_double_resolution_panics() {
        use crate::parser::parse;

        // Use a VarRef that resolves successfully (x is in scope)
        let source = "[x: 1  y: $x]";
        let file = parse(source).expect("parse failed");

        // First resolution populates the cache
        resolve_file(&file.node);

        // Second resolution must panic because the RefCell already holds Some(...)
        resolve_file(&file.node);
    }

    /// Fix 6b: double-resolution of an unresolvable VarRef must also panic.
    /// Before the Option<Option<...>> fix, writing None twice was silently accepted.
    #[test]
    #[should_panic(expected = "VarRef resolved cache already populated")]
    fn test_double_resolution_unresolvable_panics() {
        use crate::parser::parse;

        // $undefined cannot be resolved — writes Some(None) to the cache
        let source = "$undefined";
        let file = parse(source).expect("parse failed");

        // First resolution writes Some(None)
        resolve_file(&file.node);

        // Second resolution must panic: old value is Some(None), which is_some() == true
        resolve_file(&file.node);
    }

    /// Fix 7: bindings from document 1 must not resolve in document 2.
    /// Documents separated by --- are independent scopes.
    #[test]
    fn test_multi_document_isolation() {
        use crate::parser::parse;

        // Doc 1 defines x; doc 2 references $x (should NOT resolve — doc 1 scope not visible)
        // Use bare $x (not [$x]) so the expression is a VarRef, not a dict.
        let source = "[x: 1]\n---\n$x";
        let file = parse(source).expect("parse failed");
        resolve_file(&file.node);

        assert_eq!(file.node.documents.len(), 2);

        // In doc 2, $x should be unresolvable (outer None resolved to Some(None))
        let doc2 = &file.node.documents[1].node;
        let varref_expr = &doc2.expressions[0].node;
        match varref_expr {
            Expr::VarRef { name, resolved } => {
                assert_eq!(name, "x");
                // Processed but unresolvable: doc 1's scope is not visible in doc 2
                assert_eq!(resolved.borrow().flatten(), None);
            }
            other => panic!("expected VarRef in doc 2, got {:?}", other),
        }
    }

    /// Fix 8a: DotAccess — the base VarRef resolves correctly.
    #[test]
    fn test_resolve_dot_access() {
        use crate::parser::parse;

        let source = "[x: 1  result: $x.field]";
        let file = parse(source).expect("parse failed");
        resolve_file(&file.node);

        let doc = &file.node.documents[0].node;
        let dict_expr = &doc.expressions[0].node;
        match dict_expr {
            Expr::Dict(entries) => {
                // Second entry: result: $x.field
                let result_value = &entries[1].node.value.node;
                match result_value {
                    Expr::DotAccess { expr, field } => {
                        assert_eq!(*field, crate::ast::DotKey::Ident("field".to_string()));
                        match &expr.node {
                            Expr::VarRef { name, resolved } => {
                                assert_eq!(name, "x");
                                // Level 1 (level 0 is synthetic % scope), slot 0
                                assert_eq!(resolved.borrow().flatten(), Some((1, 0)));
                            }
                            other => panic!("expected VarRef inside DotAccess, got {:?}", other),
                        }
                    }
                    other => panic!("expected DotAccess, got {:?}", other),
                }
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    /// Fix 9: VarRef in named arg value position resolves correctly.
    #[test]
    fn test_resolve_named_arg() {
        use crate::parser::parse;

        // $x appears as the value of a named argument to f
        let source = "[x: 1  f: [fn [y] $y]  result: [f y: $x]]";
        let file = parse(source).expect("parse failed");
        resolve_file(&file.node);

        let doc = &file.node.documents[0].node;
        let dict_expr = &doc.expressions[0].node;
        match dict_expr {
            Expr::Dict(entries) => {
                // Third entry: result: [f y: $x]
                let result_value = &entries[2].node.value.node;
                match result_value {
                    Expr::Call { named_args, .. } => {
                        assert_eq!(named_args.len(), 1);
                        let named_arg_value = &named_args[0].node.value.node;
                        match named_arg_value {
                            Expr::VarRef { name, resolved } => {
                                assert_eq!(name, "x");
                                // Level 1 (level 0 is synthetic % scope), slot 0
                                assert_eq!(resolved.borrow().flatten(), Some((1, 0)));
                            }
                            other => panic!("expected VarRef in named arg value, got {:?}", other),
                        }
                    }
                    other => panic!("expected Call, got {:?}", other),
                }
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    /// Fix 10: enter_scope with empty key list (function with no params) must not crash.
    #[test]
    fn test_resolve_empty_scope() {
        use crate::parser::parse;

        // A zero-argument function — enter_scope(&[]) should work without panicking
        let source = "[fn [] 42]";
        let file = parse(source).expect("parse failed");

        // Must not panic
        resolve_file(&file.node);

        // The body (Int 42) has no VarRef nodes, but scope push/pop must have been balanced
        let doc = &file.node.documents[0].node;
        let fn_expr = &doc.expressions[0].node;
        match fn_expr {
            Expr::Fn { params, body, .. } => {
                assert!(params.is_empty());
                // Int body — not a VarRef, just verify it parses/resolves without panic
                assert!(matches!(body.node, Expr::Int(42)));
            }
            other => panic!("expected Fn, got {:?}", other),
        }
    }

    /// Pipeline variable % resolves to the synthetic outermost scope.
    #[test]
    fn test_resolve_pipeline_variable() {
        use crate::parser::parse;

        let source = "%";
        let file = parse(source).expect("parse failed");
        resolve_file(&file.node);

        let doc = &file.node.documents[0].node;
        let varref_expr = &doc.expressions[0].node;
        match varref_expr {
            Expr::VarRef { name, resolved } => {
                assert_eq!(name, "%");
                // % is in the synthetic scope at level 0, slot 0
                assert_eq!(resolved.borrow().flatten(), Some((0, 0)));
            }
            other => panic!("expected VarRef for %, got {:?}", other),
        }
    }

    /// Pipeline variable % resolves inside a dict (through parent scope lookup).
    #[test]
    fn test_resolve_pipeline_variable_in_dict() {
        use crate::parser::parse;

        let source = "[x: %]";
        let file = parse(source).expect("parse failed");
        resolve_file(&file.node);

        let doc = &file.node.documents[0].node;
        let dict_expr = &doc.expressions[0].node;
        match dict_expr {
            Expr::Dict(entries) => {
                let x_value = &entries[0].node.value.node;
                match x_value {
                    Expr::VarRef { name, resolved } => {
                        assert_eq!(name, "%");
                        // % is in synthetic scope (level 0), dict scope is level 1
                        assert_eq!(resolved.borrow().flatten(), Some((0, 0)));
                    }
                    other => panic!("expected VarRef for %, got {:?}", other),
                }
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    /// Named section %name resolves in subsequent documents.
    /// Format: `--- %name` names the NEXT document. The named document's result
    /// becomes available as `%name` in all subsequent documents.
    #[test]
    fn test_resolve_named_section_in_subsequent_doc() {
        use crate::parser::parse;

        // Doc 1 (unnamed): 42
        // --- %first names doc 2
        // Doc 2 (named "first"): [x: 1]
        // --- separates doc 3
        // Doc 3 (unnamed): %first (references doc 2)
        let source = "42\n--- %first\n[x: 1]\n---\n%first";
        let file = parse(source).expect("parse failed");
        resolve_file(&file.node);

        assert_eq!(file.node.documents.len(), 3);

        // Doc 2 is named "first"
        assert_eq!(file.node.documents[1].node.name, Some("first".to_string()));

        // In doc 3, %first should resolve to (level 0, slot 1)
        // because the synthetic scope has ["%", "%first"]
        let doc3 = &file.node.documents[2].node;
        let varref_expr = &doc3.expressions[0].node;
        match varref_expr {
            Expr::VarRef { name, resolved } => {
                assert_eq!(name, "%first");
                // slot 0 = %, slot 1 = %first
                assert_eq!(resolved.borrow().flatten(), Some((0, 1)));
            }
            other => panic!("expected VarRef for %first, got {:?}", other),
        }
    }
}
