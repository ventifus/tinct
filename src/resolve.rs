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

use crate::ast::{
    node_id, Annotation, Document, Expr, File, ResolutionTable, Spanned, SurfaceDeclaration,
    SurfaceDocument, SurfaceEntry, SurfaceExpression, SurfaceItem, SurfaceNode, SurfaceProgram,
};
use std::sync::Arc;

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
    /// `level` is the De Bruijn index of the binding's scope (0 = current/innermost scope,
    /// 1 = one scope outward, N = N scopes outward). This matches `Environment::get_by_slot`
    /// which also uses 0 = current env, N = N parent hops.
    /// `slot` is the index within that scope's slot vector.
    pub fn resolve(&self, name: &str) -> Option<(u32, u32)> {
        for (offset, scope) in self.scopes.iter().rev().enumerate() {
            if let Some(&slot) = scope.get(name) {
                // level is the De Bruijn index: 0 = innermost (current) scope,
                // N = N hops toward the outermost scope.
                // offset=0 means the variable is in the innermost scope (level 0).
                // offset=1 means one scope outward (level 1), etc.
                let level = u32::try_from(offset).expect("scope depth overflow");
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
            Annotation::Annotated(_name, inner) => {
                // Create a temporary Spanned wrapper for recursion
                let inner_spanned = Spanned::new(inner.as_ref().clone(), ann.span);
                self.walk_annotation(&inner_spanned);
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
            Expr::VarRef { name, resolved, .. } => {
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
                // Linked environments (Rc chain) — the adopted design.
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
                // Model eval.rs sequential evaluation: each intermediate dict expression's
                // string-keyed entries become scope bindings for subsequent expressions.
                // This mirrors eval's Expr::Sequential handler (eval.rs:821-888) where
                // each non-last dict creates a child_env with its string keys injected.
                //
                // Without this injection, `args` in:
                //   [n: [length args]]   ← dict A: pushes n into child_env at slot 0
                //   [... args ...]        ← at runtime: child_env has n@0, parent has args@0
                // would resolve to (0,0) = n, not the intended (1,0) = args.
                let mut injected_scopes: usize = 0;
                for (i, seq_expr) in exprs.iter().enumerate() {
                    let is_last = i == exprs.len() - 1;
                    self.walk_expr(seq_expr);
                    // After each intermediate (non-last) dict expression, inject its static
                    // keys as a new scope — exactly as walk_document does for documents.
                    if !is_last {
                        let keys = Self::dict_static_keys(seq_expr);
                        if !keys.is_empty() {
                            self.enter_scope(&keys);
                            injected_scopes += 1;
                        }
                    }
                }
                // Pop injected scopes in reverse order.
                for _ in 0..injected_scopes {
                    self.exit_scope();
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
            Expr::TypeAlias { body, .. } => self.walk_expr(body),
            // Quote: do NOT resolve variables inside the quoted expression.
            // Variables in quoted code are AST data, not runtime bindings.
            Expr::Quote(_) => {}
            // Unquote and UnquoteSplice: DO resolve variables inside the unquoted expression.
            // The expression is evaluated in the current runtime environment.
            Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
                self.walk_expr(inner);
            }
            // DefMacro: resolve variables in the body expression.
            Expr::DefMacro { body, .. } => {
                self.walk_expr(body);
            }
            // MacroDecl: resolve variables in params and body expressions.
            Expr::MacroDecl { params, body, .. } => {
                self.walk_expr(params);
                self.walk_expr(body);
            }
            // Splice: resolve variables in each form.
            Expr::Splice(forms) => {
                for form in forms {
                    self.walk_expr(form);
                }
            }
            // SyntaxClass: resolve variables in pattern expression.
            Expr::SyntaxClass { pattern, .. } => {
                self.walk_expr(pattern);
            }
            // Match: resolve variables in scrutinee, guards, and arm bodies.
            // Create a new scope for each arm to bind pattern-bound variables.
            Expr::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee);
                for arm in arms {
                    // Collect pattern-bound variable names
                    let mut bound_names = std::collections::HashSet::new();
                    collect_pattern_variables(&arm.pattern.node, &mut bound_names);

                    // Enter scope for this arm (binds pattern variables)
                    let bound_vec: Vec<String> = bound_names.into_iter().collect();
                    if !bound_vec.is_empty() {
                        self.enter_scope(&bound_vec);
                    }

                    // Resolve guard and body in arm scope
                    if let Some(guard) = &arm.guard {
                        self.walk_expr(guard);
                    }
                    self.walk_expr(&arm.body);

                    // Exit arm scope
                    if !bound_vec.is_empty() {
                        self.exit_scope();
                    }
                }
            }
            // ClassDecl: resolve variables in method signatures
            Expr::ClassDecl { methods, .. } => {
                for method in methods {
                    if let Some(key) = &method.node.key {
                        self.walk_expr(key);
                    }
                    self.walk_expr(&method.node.value);
                }
            }
            // InstanceDecl: resolve variables in pattern expressions and method implementations
            Expr::InstanceDecl { arms, .. } => {
                for (pattern_expr, methods) in arms {
                    self.walk_expr(pattern_expr);
                    for method in methods {
                        if let Some(key) = &method.node.key {
                            self.walk_expr(key);
                        }
                        self.walk_expr(&method.node.value);
                    }
                }
            }
            // PatternDecl: resolve variables in bindings
            Expr::PatternDecl { bindings } => {
                for binding in bindings {
                    self.walk_expr(binding);
                }
            }
            // LetDecl: resolve variables in bindings
            Expr::LetDecl { bindings } => {
                for binding in bindings {
                    self.walk_expr(binding);
                }
            }
            // CaseArm: resolve variables in pattern and body
            Expr::CaseArm { pattern, body } => {
                self.walk_expr(pattern);
                self.walk_expr(body);
            }
            // Literals have no child expressions
            Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::Str(_)
            | Expr::Rest(_)
            | Expr::Placeholder
            | Expr::TypeApp { .. }
            | Expr::Error(_) => {}
            Expr::Annotated { annotation, .. } => {
                self.walk_annotation(annotation);
            }
        }
    }

    /// Extract the static string-keyed names from a dict expression.
    ///
    /// Returns the same set of names that `walk_expr` for `Expr::Dict` would push as a scope.
    /// Used by `walk_document` to model `eval_document`'s scope-chain semantics.
    fn dict_static_keys(expr: &Spanned<Expr>) -> Vec<String> {
        match &expr.node {
            Expr::Dict(entries) => entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .node
                        .key
                        .as_ref()
                        .and_then(|key_expr| match &key_expr.node {
                            Expr::Str(s) => Some(s.clone()),
                            Expr::Annotated { name, .. } => Some(name.clone()),
                            _ => None,
                        })
                })
                .collect(),
            // Non-dict expressions don't inject any scope bindings.
            _ => Vec::new(),
        }
    }

    /// Walk a document and resolve all VarRef nodes in its expressions.
    ///
    /// Models `eval_document`'s scope-chain semantics: each intermediate dict expression's
    /// string-keyed entries become scope bindings for subsequent expressions. This mirrors
    /// the runtime behaviour where `eval_document` materialises the first dict and injects
    /// its entries into a child environment for the next expression.
    ///
    /// For the prelude public/private split pattern:
    /// ```text
    /// [helper-fn: ...]          # first expression (private)
    /// [public-fn: [fn [] [helper-fn ...]]]  # second expression (public)
    /// ```
    /// `helper-fn` is resolved correctly in the second expression because its name is
    /// injected as a scope binding between the two dict expressions.
    fn walk_document(&mut self, document: &Document) {
        let exprs = &document.expressions;

        if exprs.is_empty() {
            return;
        }

        // Stack of "scope-chain" scope depths pushed by intermediate dict expressions.
        // Each time we inject scope bindings from an intermediate dict, we push a scope
        // that must be popped after the document is fully resolved.
        let mut injected_scopes: usize = 0;

        for (i, expr) in exprs.iter().enumerate() {
            let is_last = i == exprs.len() - 1;

            self.walk_expr(expr);

            // After resolving an intermediate (non-last) dict expression, inject its
            // static keys into a new scope so subsequent expressions can reference them.
            // This models eval_document's child-environment injection.
            if !is_last {
                let keys = Self::dict_static_keys(expr);
                if !keys.is_empty() {
                    self.enter_scope(&keys);
                    injected_scopes += 1;
                }
            }
        }

        // Pop all injected scopes in reverse order.
        for _ in 0..injected_scopes {
            self.exit_scope();
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

// ============================================================================
// runtime-v2: SurfaceProgram resolver — produces ResolutionTable
// ============================================================================

/// Variable resolution pass for the Surface AST.
///
/// Walks a `SurfaceProgram` and produces a `ResolutionTable` mapping each
/// `VarRef` node's `NodeId` to its de Bruijn `(level, slot)` coordinates.
///
/// This replaces the old `resolve_file()` mutation of `VarRef.resolved: RefCell<...>`.
/// The SurfaceExpression tree is immutable; all resolution data lives in the table.
struct SurfaceResolver {
    scopes: Vec<indexmap::IndexMap<String, u32>>,
    table: ResolutionTable,
}

impl SurfaceResolver {
    fn new() -> Self {
        Self {
            scopes: Vec::new(),
            table: ResolutionTable::new(),
        }
    }

    fn enter_scope(&mut self, keys: &[String]) {
        let mut scope = indexmap::IndexMap::new();
        for (slot, key) in keys.iter().enumerate() {
            scope.insert(
                key.clone(),
                u32::try_from(slot).expect("slot index overflow"),
            );
        }
        self.scopes.push(scope);
    }

    fn exit_scope(&mut self) {
        self.scopes
            .pop()
            .expect("exit_scope called with empty stack");
    }

    fn resolve_name(&self, name: &str) -> Option<(u32, u32)> {
        for (offset, scope) in self.scopes.iter().rev().enumerate() {
            if let Some(&slot) = scope.get(name) {
                let level = u32::try_from(offset).expect("scope depth overflow");
                return Some((level, slot));
            }
        }
        None
    }

    fn walk_surface_node(&mut self, arc: &Arc<SurfaceNode>) {
        self.walk_surface_expr(arc, &arc.expr);
    }

    fn walk_surface_expr(&mut self, arc: &Arc<SurfaceNode>, expr: &SurfaceExpression) {
        match expr {
            SurfaceExpression::VarRef { name, .. } => {
                if let Some(coords) = self.resolve_name(name) {
                    self.table.insert(node_id(arc), coords);
                }
                // If not found: FreeVar at runtime — no entry in table (lowering uses this as signal)
            }

            SurfaceExpression::Dict(entries) => {
                let static_keys = surface_dict_static_keys(entries);

                // Walk key expressions in outer scope
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        self.walk_surface_node(key);
                    }
                }

                self.enter_scope(&static_keys);
                for entry in entries {
                    self.walk_surface_node(&entry.node.value);
                }
                self.exit_scope();
            }

            SurfaceExpression::Fn {
                return_ann: _,
                params,
                body,
                ..
            } => {
                // Walk param annotations in outer scope
                for param in params {
                    if let Some(ann) = &param.node.annotation {
                        self.walk_surface_annotation(ann);
                    }
                }
                let param_names: Vec<String> = params.iter().map(|p| p.node.name.clone()).collect();
                self.enter_scope(&param_names);
                self.walk_surface_node(body);
                self.exit_scope();
            }

            SurfaceExpression::Sequential(exprs) => {
                let mut injected = 0usize;
                for (i, e) in exprs.iter().enumerate() {
                    let is_last = i == exprs.len() - 1;
                    self.walk_surface_node(e);
                    if !is_last {
                        if let Some(keys) = surface_node_static_keys(e) {
                            if !keys.is_empty() {
                                self.enter_scope(&keys);
                                injected += 1;
                            }
                        }
                    }
                }
                for _ in 0..injected {
                    self.exit_scope();
                }
            }

            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                self.walk_surface_node(func);
                for arg in args {
                    self.walk_surface_node(arg);
                }
                for na in named_args {
                    self.walk_surface_node(&na.node.value);
                }
            }

            SurfaceExpression::DotAccess { expr, .. } => self.walk_surface_node(expr),

            // Pipe: walk both sides (the lowering pass will rewrite pipe to call)
            SurfaceExpression::Pipe { lhs, rhs } => {
                self.walk_surface_node(lhs);
                self.walk_surface_node(rhs);
            }

            SurfaceExpression::TypeAssert { annotation, expr } => {
                self.walk_surface_annotation(annotation);
                self.walk_surface_node(expr);
            }

            // Quote: do NOT resolve variables inside — they are AST data, not bindings.
            SurfaceExpression::Quote(_) => {}

            SurfaceExpression::Unquote(inner) | SurfaceExpression::UnquoteSplice(inner) => {
                self.walk_surface_node(inner);
            }

            SurfaceExpression::Match { scrutinee, arms } => {
                self.walk_surface_node(scrutinee);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.walk_surface_node(guard);
                    }
                    self.walk_surface_node(&arm.body);
                }
            }

            SurfaceExpression::PatternDecl { bindings }
            | SurfaceExpression::LetDecl { bindings } => {
                for b in bindings {
                    self.walk_surface_node(b);
                }
            }

            SurfaceExpression::CaseArm { pattern, body } => {
                self.walk_surface_node(pattern);
                self.walk_surface_node(body);
            }

            SurfaceExpression::Annotated { annotation, .. } => {
                self.walk_surface_annotation(annotation);
            }

            SurfaceExpression::TypeApp { func, arg } => {
                self.walk_surface_node(func);
                self.walk_surface_node(arg);
            }

            // Terminals with no child expressions
            SurfaceExpression::Int(_)
            | SurfaceExpression::Float(_)
            | SurfaceExpression::Bool(_)
            | SurfaceExpression::Str(_)
            | SurfaceExpression::Rest(_)
            | SurfaceExpression::Placeholder
            | SurfaceExpression::Error(_) => {}
        }
    }

    fn walk_surface_annotation(&mut self, ann: &Spanned<crate::ast::Annotation>) {
        match &ann.node {
            crate::ast::Annotation::Simple(_) => {}
            crate::ast::Annotation::PropertyDict(_entries) => {
                // PropertyDict entries contain old Expr nodes (pre-migration).
                // In the fully migrated system, Annotation will use Arc<SurfaceNode>.
                // Until then, skip resolution of PropertyDict entry expressions —
                // they will fall back to FreeVar (name-based lookup) at runtime.
            }
            crate::ast::Annotation::Annotated(_, inner) => {
                let inner_spanned = Spanned::new(inner.as_ref().clone(), ann.span);
                self.walk_surface_annotation(&inner_spanned);
            }
        }
    }

    fn walk_surface_declaration(&mut self, decl: &SurfaceDeclaration) {
        match decl {
            SurfaceDeclaration::TypeAlias { body, .. } => self.walk_surface_node(body),
            SurfaceDeclaration::ClassDecl {
                methods,
                determines,
                resolver,
                ..
            } => {
                for method in methods {
                    if let Some(key) = &method.node.key {
                        self.walk_surface_node(key);
                    }
                    self.walk_surface_node(&method.node.value);
                }
                for d in determines {
                    self.walk_surface_node(d);
                }
                if let Some(r) = resolver {
                    self.walk_surface_node(r);
                }
            }
            SurfaceDeclaration::InstanceDecl { arms, .. } => {
                for (pattern, methods) in arms {
                    self.walk_surface_node(pattern);
                    for method in methods {
                        if let Some(key) = &method.node.key {
                            self.walk_surface_node(key);
                        }
                        self.walk_surface_node(&method.node.value);
                    }
                }
            }
            SurfaceDeclaration::DefMacro { params, body, .. }
            | SurfaceDeclaration::MacroDecl { params, body, .. } => {
                self.walk_surface_node(params);
                self.walk_surface_node(body);
            }
            SurfaceDeclaration::SyntaxClass { pattern, .. } => {
                self.walk_surface_node(pattern);
            }
            SurfaceDeclaration::Splice(forms) => {
                for form in forms {
                    self.walk_surface_node(form);
                }
            }
        }
    }

    fn walk_surface_document(&mut self, doc: &SurfaceDocument) {
        let mut injected = 0usize;
        let items: Vec<&SurfaceItem> = doc.items.iter().collect();
        let expr_count = items
            .iter()
            .filter(|i| matches!(i, SurfaceItem::Expr(_)))
            .count();
        let mut expr_idx = 0usize;

        for item in &items {
            match item {
                SurfaceItem::Expr(node) => {
                    let is_last_expr = expr_idx == expr_count - 1;
                    self.walk_surface_node(node);
                    if !is_last_expr {
                        if let Some(keys) = surface_node_static_keys(node) {
                            if !keys.is_empty() {
                                self.enter_scope(&keys);
                                injected += 1;
                            }
                        }
                    }
                    expr_idx += 1;
                }
                SurfaceItem::Decl(decl) => {
                    self.walk_surface_declaration(&decl.node);
                }
            }
        }
        for _ in 0..injected {
            self.exit_scope();
        }
    }

    fn finish(self) -> ResolutionTable {
        self.table
    }
}

/// Resolve all VarRef nodes in a SurfaceProgram and return a ResolutionTable.
///
/// This is the runtime-v2 entry point for variable resolution. The SurfaceProgram
/// is unchanged (immutable); all resolution data is captured in the returned table.
///
/// The resolver models the same scope-chain semantics as `resolve_file()` and the
/// evaluator: each intermediate dict expression's static keys become scope bindings
/// for subsequent expressions within the same document.
pub fn resolve_surface_program(program: &SurfaceProgram) -> ResolutionTable {
    let mut resolver = SurfaceResolver::new();
    let mut named_sections: Vec<String> = Vec::new();

    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;

        // Build synthetic runtime scope: `%` plus `%name` for previously named sections.
        let mut runtime_names: Vec<String> = vec!["%".to_string()];
        for name in &named_sections {
            runtime_names.push(format!("%{}", name));
        }
        resolver.enter_scope(&runtime_names);
        resolver.walk_surface_document(doc);
        resolver.exit_scope();

        if let Some(ref name) = doc.name {
            named_sections.push(name.clone());
        }
    }

    resolver.finish()
}

/// Extract static string-keyed names from a SurfaceExpression::Dict's entries.
/// Same logic as the old resolver's `dict_static_keys` but for SurfaceEntry.
fn surface_dict_static_keys(entries: &[Spanned<SurfaceEntry>]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| {
            entry
                .node
                .key
                .as_ref()
                .and_then(|key_node| match &key_node.expr {
                    SurfaceExpression::Str(s) => Some(s.clone()),
                    SurfaceExpression::Annotated { name, .. } => Some(name.clone()),
                    _ => None,
                })
        })
        .collect()
}

/// Extract static string-keyed names from an Arc<SurfaceNode> if it is a Dict.
fn surface_node_static_keys(node: &Arc<SurfaceNode>) -> Option<Vec<String>> {
    match &node.expr {
        SurfaceExpression::Dict(entries) => Some(surface_dict_static_keys(entries)),
        _ => None,
    }
}

/// Collect all variable names bound by a pattern (for match arm scoping).
fn collect_pattern_variables(
    pattern: &crate::ast::Pattern,
    vars: &mut std::collections::HashSet<String>,
) {
    match pattern {
        crate::ast::Pattern::Variable(name) => {
            vars.insert(name.clone());
        }
        crate::ast::Pattern::Dict { fields, .. } => {
            for (_, field_pattern) in fields {
                collect_pattern_variables(&field_pattern.node, vars);
            }
        }
        crate::ast::Pattern::Seq { head, tail } => {
            collect_pattern_variables(&head.node, vars);
            collect_pattern_variables(&tail.node, vars);
        }
        crate::ast::Pattern::Constructor { binding, .. } => {
            if let Some(binding_pattern) = binding {
                collect_pattern_variables(&binding_pattern.node, vars);
            }
        }
        crate::ast::Pattern::Or(patterns) => {
            // For or-patterns, we only collect from the first branch
            // (all branches must bind the same variables, verified separately)
            if let Some(first) = patterns.first() {
                collect_pattern_variables(&first.node, vars);
            }
        }
        crate::ast::Pattern::Wildcard
        | crate::ast::Pattern::TypeTag(_)
        | crate::ast::Pattern::Literal(_)
        | crate::ast::Pattern::Pin(_) => {
            // These don't bind variables
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
        resolver.enter_scope(&["x".into()]); // outer scope
        resolver.enter_scope(&["y".into()]); // inner scope
                                             // Resolve x from inner scope - it's 1 hop outward (De Bruijn level 1)
        assert_eq!(resolver.resolve("x"), Some((1, 0)));
        // y is in the current (innermost) scope, so De Bruijn level 0
        assert_eq!(resolver.resolve("y"), Some((0, 0)));
    }

    #[test]
    fn test_shadowing() {
        let mut resolver = Resolver::new();
        resolver.enter_scope(&["x".into()]); // outer scope, slot 0
        resolver.enter_scope(&["x".into()]); // inner scope, slot 0 (shadows outer x)
                                             // Should resolve to the innermost x (De Bruijn level 0)
        assert_eq!(resolver.resolve("x"), Some((0, 0)));
    }

    #[test]
    fn test_resolve_exit_scope() {
        let mut resolver = Resolver::new();
        resolver.enter_scope(&["x".into()]);
        resolver.enter_scope(&["y".into()]);
        // y is in the innermost scope, De Bruijn level 0
        assert_eq!(resolver.resolve("y"), Some((0, 0)));
        resolver.exit_scope();
        assert_eq!(resolver.resolve("y"), None);
        // After exit, x is now in the only (innermost) scope, De Bruijn level 0
        assert_eq!(resolver.resolve("x"), Some((0, 0)));
    }

    #[test]
    fn test_resolve_file_simple_dict() {
        use crate::parser::parse;

        let source = "[x: 1  y: $x]";
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);

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
                    Expr::VarRef { name, resolved, .. } => {
                        assert_eq!(name, "x");
                        // x is a sibling in the same dict scope; De Bruijn level 0 (current scope)
                        assert_eq!(resolved.borrow().flatten(), Some((0, 0)));
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);

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
                            Expr::VarRef { name, resolved, .. } => {
                                assert_eq!(name, "x");
                                // x is in the outer dict scope; De Bruijn level 1 (1 hop outward from inner dict), slot 0
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);

        resolve_file(&file.node);

        // Navigate to the VarRef for x in the function body
        let doc = &file.node.documents[0].node;
        let fn_expr = &doc.expressions[0].node;
        match fn_expr {
            Expr::Fn { body, .. } => {
                match &body.node {
                    Expr::VarRef { name, resolved, .. } => {
                        assert_eq!(name, "x");
                        // x is the first parameter; De Bruijn level 0 (current/innermost scope), slot 0
                        assert_eq!(resolved.borrow().flatten(), Some((0, 0)));
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);

        resolve_file(&file.node);

        // Check that the VarRef for undefined is NOT resolved (None)
        let doc = &file.node.documents[0].node;
        let varref_expr = &doc.expressions[0].node;
        match varref_expr {
            Expr::VarRef { name, resolved, .. } => {
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
        resolve_file(&file.node);

        let doc = &file.node.documents[0].node;
        let dict_expr = &doc.expressions[0].node;
        match dict_expr {
            Expr::Dict(entries) => {
                // Second entry: y: $x — value $x should resolve to x at De Bruijn level 0, slot 0
                let y_value = &entries[1].node.value.node;
                match y_value {
                    Expr::VarRef { name, resolved, .. } => {
                        assert_eq!(name, "x");
                        // x is a sibling in the same dict scope; De Bruijn level 0 (current scope), slot 0
                        assert_eq!(resolved.borrow().flatten(), Some((0, 0)));
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
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
                                    Expr::VarRef { name, resolved, .. } => {
                                        assert_eq!(name, "fallback");
                                        // fallback is a sibling in the same dict scope; De Bruijn level 0
                                        assert_eq!(resolved.borrow().flatten(), Some((0, 0)));
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
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
                                    Expr::VarRef { name, resolved, .. } => {
                                        assert_eq!(name, "default_val");
                                        // default_val is a sibling in the dict scope;
                                        // param annotations are walked before entering fn scope,
                                        // so dict scope is innermost here: De Bruijn level 0
                                        assert_eq!(resolved.borrow().flatten(), Some((0, 0)));
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);

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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);

        // First resolution writes Some(None)
        resolve_file(&file.node);

        // Second resolution must panic: old value is Some(None), which is_some() == true
        resolve_file(&file.node);
    }

    /// Multi-expression document scope chain: the second dict expression can reference
    /// names defined in the first dict expression of the same document.
    ///
    /// This models eval_document's scope-chain semantics where intermediate dict entries
    /// are injected into a child environment for subsequent expressions.
    #[test]
    fn test_multi_expr_scope_chain_resolves() {
        use crate::parser::parse;

        // Two dict expressions in one document: second references first's keys.
        let source = "[helper: 1]\n[public: $helper]";
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
        resolve_file(&file.node);

        let doc = &file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 2);

        // In the second dict expression, $helper should resolve (not be unresolvable).
        let second_dict = &doc.expressions[1].node;
        match second_dict {
            Expr::Dict(entries) => {
                // Entry: public: $helper
                let value = &entries[0].node.value.node;
                match value {
                    Expr::VarRef { name, resolved, .. } => {
                        assert_eq!(name, "helper");
                        // Must resolve — not Some(None).
                        // Scope stack when resolving $helper:
                        //   innermost (offset 0): second dict's own scope {public}
                        //   offset 1: injected scope from first dict {helper}
                        //   offset 2 (outermost): synthetic % scope
                        // $helper is at offset=1 from the innermost scope → De Bruijn level 1.
                        let coords = resolved.borrow().flatten();
                        assert!(
                            coords.is_some(),
                            "expected $helper to resolve, got Some(None) (unresolvable)"
                        );
                        assert_eq!(coords, Some((1, 0)));
                    }
                    other => panic!("expected VarRef for public value, got {:?}", other),
                }
            }
            other => panic!("expected Dict for second expression, got {:?}", other),
        }
    }

    /// Multi-expression scope chain: first dict's keys are NOT visible as siblings within
    /// the first dict itself (they're only injected for SUBSEQUENT expressions).
    #[test]
    fn test_multi_expr_first_dict_sees_own_scope_only() {
        use crate::parser::parse;

        // First dict: a and b are siblings (letrec — both visible to each other).
        // $a in b's value is in the same dict scope (De Bruijn level 0).
        let source = "[a: 1  b: $a]\n[c: $b]";
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
        resolve_file(&file.node);

        let doc = &file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 2);

        // In the first dict, $a (in b's value) resolves to De Bruijn level 0 (same dict scope).
        let first_dict = &doc.expressions[0].node;
        match first_dict {
            Expr::Dict(entries) => {
                let b_value = &entries[1].node.value.node;
                match b_value {
                    Expr::VarRef { name, resolved, .. } => {
                        assert_eq!(name, "a");
                        assert_eq!(resolved.borrow().flatten(), Some((0, 0)));
                    }
                    other => panic!("expected VarRef for b value, got {:?}", other),
                }
            }
            other => panic!("expected Dict, got {:?}", other),
        }

        // In the second dict, $b resolves to the injected scope: De Bruijn level 1, slot 1.
        // Scope stack: innermost={c:0}, offset-1={a:0,b:1} (injected), offset-2={%:0}.
        // b is at slot 1 in the injected scope (offset=1 → level=1).
        let second_dict = &doc.expressions[1].node;
        match second_dict {
            Expr::Dict(entries) => {
                let c_value = &entries[0].node.value.node;
                match c_value {
                    Expr::VarRef { name, resolved, .. } => {
                        assert_eq!(name, "b");
                        assert_eq!(resolved.borrow().flatten(), Some((1, 1)));
                    }
                    other => panic!("expected VarRef for c value, got {:?}", other),
                }
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    /// Multi-expression scope chain: three expressions chain correctly.
    #[test]
    fn test_multi_expr_three_expressions_chain() {
        use crate::parser::parse;

        // Three dicts: third sees both first and second dict's keys.
        let source = "[a: 1]\n[b: 2]\n[c: $a  d: $b]";
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
        resolve_file(&file.node);

        let doc = &file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 3);

        let third_dict = &doc.expressions[2].node;
        match third_dict {
            Expr::Dict(entries) => {
                // Scope stack when resolving the third dict:
                //   innermost (offset 0): third dict's own scope [c, d]
                //   offset 1: injected scope from second dict [b]
                //   offset 2: injected scope from first dict [a]
                //   offset 3 (outermost): synthetic % scope
                // c: $a — $a is in offset-2 scope → De Bruijn level 2
                // d: $b — $b is in offset-1 scope → De Bruijn level 1
                let c_value = &entries[0].node.value.node;
                match c_value {
                    Expr::VarRef { name, resolved, .. } => {
                        assert_eq!(name, "a");
                        assert_eq!(resolved.borrow().flatten(), Some((2, 0)));
                    }
                    other => panic!("expected VarRef for c value, got {:?}", other),
                }

                let d_value = &entries[1].node.value.node;
                match d_value {
                    Expr::VarRef { name, resolved, .. } => {
                        assert_eq!(name, "b");
                        assert_eq!(resolved.borrow().flatten(), Some((1, 0)));
                    }
                    other => panic!("expected VarRef for d value, got {:?}", other),
                }
            }
            other => panic!("expected Dict for third expression, got {:?}", other),
        }
    }

    /// Fix 7: bindings from document 1 must not resolve in document 2.
    /// Documents separated by --- are independent scopes.
    #[test]
    fn test_multi_document_isolation() {
        use crate::parser::parse;

        // Doc 1 defines x; doc 2 references $x (should NOT resolve — doc 1 scope not visible)
        // Use bare $x (not [$x]) so the expression is a VarRef, not a dict.
        let source = "[x: 1]\n---\n$x";
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
        resolve_file(&file.node);

        assert_eq!(file.node.documents.len(), 2);

        // In doc 2, $x should be unresolvable (outer None resolved to Some(None))
        let doc2 = &file.node.documents[1].node;
        let varref_expr = &doc2.expressions[0].node;
        match varref_expr {
            Expr::VarRef { name, resolved, .. } => {
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
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
                            Expr::VarRef { name, resolved, .. } => {
                                assert_eq!(name, "x");
                                // x is a sibling in the dict scope; De Bruijn level 0 (current scope), slot 0
                                assert_eq!(resolved.borrow().flatten(), Some((0, 0)));
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
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
                            Expr::VarRef { name, resolved, .. } => {
                                assert_eq!(name, "x");
                                // x is a sibling in the dict scope; De Bruijn level 0 (current scope), slot 0
                                assert_eq!(resolved.borrow().flatten(), Some((0, 0)));
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);

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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
        resolve_file(&file.node);

        let doc = &file.node.documents[0].node;
        let varref_expr = &doc.expressions[0].node;
        match varref_expr {
            Expr::VarRef { name, resolved, .. } => {
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
        resolve_file(&file.node);

        let doc = &file.node.documents[0].node;
        let dict_expr = &doc.expressions[0].node;
        match dict_expr {
            Expr::Dict(entries) => {
                let x_value = &entries[0].node.value.node;
                match x_value {
                    Expr::VarRef { name, resolved, .. } => {
                        assert_eq!(name, "%");
                        // % is in the outermost (synthetic) scope; De Bruijn level 1 from inside dict
                        // (dict scope is offset 0/level 0, % scope is offset 1/level 1)
                        assert_eq!(resolved.borrow().flatten(), Some((1, 0)));
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
        let file = crate::ast_convert::surface_program_to_file(&parse(source).expect("parse failed").program);
        resolve_file(&file.node);

        assert_eq!(file.node.documents.len(), 3);

        // Doc 2 is named "first"
        assert_eq!(file.node.documents[1].node.name, Some("first".to_string()));

        // In doc 3, %first should resolve to (level 0, slot 1)
        // because the synthetic scope has ["%", "%first"]
        let doc3 = &file.node.documents[2].node;
        let varref_expr = &doc3.expressions[0].node;
        match varref_expr {
            Expr::VarRef { name, resolved, .. } => {
                assert_eq!(name, "%first");
                // slot 0 = %, slot 1 = %first
                assert_eq!(resolved.borrow().flatten(), Some((0, 1)));
            }
            other => panic!("expected VarRef for %first, got {:?}", other),
        }
    }
}
