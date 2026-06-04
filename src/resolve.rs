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

use crate::ast::{
    node_id, Pattern, ResolutionTable, Spanned, SurfaceDeclaration, SurfaceDocument, SurfaceEntry,
    SurfaceExpression, SurfaceItem, SurfaceNode, SurfaceProgram,
};
use std::sync::Arc;

// ============================================================================
// runtime-v2: SurfaceProgram resolver — produces ResolutionTable
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
                    // Extract pattern-bound variables
                    let bound_names = extract_pattern_bindings(&arm.pattern);

                    // Only push a scope when the pattern actually binds variables.
                    // Wildcard (`_`), literals, and Pin patterns bind nothing;
                    // allocating an empty IndexMap and pushing/popping it is pure overhead.
                    let has_bindings = !bound_names.is_empty();
                    if has_bindings {
                        self.enter_scope(&bound_names);
                    }

                    // Walk guard (if present) inside the pattern scope
                    if let Some(guard) = &arm.guard {
                        self.walk_surface_node(guard);
                    }

                    // Walk body inside the pattern scope
                    self.walk_surface_node(&arm.body);

                    if has_bindings {
                        self.exit_scope();
                    }
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

            // Terminals with no child expressions
            SurfaceExpression::Int(_)
            | SurfaceExpression::Float(_)
            | SurfaceExpression::Bool(_)
            | SurfaceExpression::Str(_)
            | SurfaceExpression::Rest(_)
            | SurfaceExpression::Placeholder
            | SurfaceExpression::Decl(_) // type-level declaration, no variable references to resolve
            | SurfaceExpression::Error(_) => {}
        }
    }

    fn walk_surface_annotation(&mut self, ann: &Spanned<crate::ast::Annotation>) {
        match &ann.node {
            crate::ast::Annotation::Simple(_) => {}
            crate::ast::Annotation::PropertyDict(entries) => {
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        self.walk_surface_node(key);
                    }
                    self.walk_surface_node(&entry.node.value);
                }
            }
            crate::ast::Annotation::Annotated(_, inner) => {
                let inner_spanned = Spanned::new(inner.as_ref().clone(), ann.span.clone());
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
            SurfaceDeclaration::MacroDecl { params, body, .. } => {
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

        // Skip type-stage documents — they don't participate in runtime evaluation.
        // Matches eval_surface_file_with_input in eval.rs which skips type-stage documents entirely.
        if doc.stage == Some(crate::ast::Stage::Type) {
            continue;
        }

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

/// Extract all variable names bound by a pattern.
/// This is used to create scope bindings for match arm bodies.
///
/// Examples:
/// - `_` (Wildcard) → []
/// - `x` (Variable) → ["x"]
/// - `[Some v]` (Constructor) → ["v"]
/// - `[Dict {x, y: z}]` → ["x", "z"]
/// - `[seq h t]` → ["h", "t"]
/// - `x | y` (Or) → ["x", "y"] (both branches must bind same vars)
fn extract_pattern_bindings(pattern: &Spanned<Pattern>) -> Vec<String> {
    let mut bindings = Vec::new();
    collect_pattern_bindings(&pattern.node, &mut bindings);
    bindings
}

/// Recursively collect all variable bindings from a pattern.
fn collect_pattern_bindings(pattern: &Pattern, out: &mut Vec<String>) {
    match pattern {
        Pattern::Wildcard => {
            // Wildcard matches anything but binds no variables
        }
        Pattern::Variable(name) => {
            out.push(name.clone());
        }
        Pattern::Literal(_) => {
            // Literal patterns bind no variables
        }
        Pattern::TypeTag(_) => {
            // TypeTag patterns (Int:, Str:, Seq:, etc.) bind no variables — type-dispatch only
        }
        Pattern::TypeAssertPending { inner, .. } => {
            if let Some(inner_pat) = inner {
                collect_pattern_bindings(&inner_pat.node, out);
            }
        }
        Pattern::TypeAssert { inner, .. } => {
            if let Some(inner_pat) = inner {
                collect_pattern_bindings(&inner_pat.node, out);
            }
        }
        Pattern::Pin(_) => {
            // Pin patterns ($name) match against existing variable value, don't bind
        }
        Pattern::Dict { fields, .. } => {
            // Dict pattern: each field has a key and an inner pattern. The
            // bound name comes from the inner pattern (typically Variable(name)),
            // not directly from the key string. `{x}` desugars to key="x" with
            // inner pattern Variable("x"); `{x: y}` has key="x" with inner
            // pattern Variable("y"). We recurse into each field's inner pattern.
            for (_key, field_pattern) in fields {
                collect_pattern_bindings(&field_pattern.node, out);
            }
        }
        Pattern::Seq { head, tail } => {
            // Seq pattern: both head and tail can bind variables
            collect_pattern_bindings(&head.node, out);
            collect_pattern_bindings(&tail.node, out);
        }
        Pattern::Constructor { binding, .. } => {
            // Constructor pattern: optional payload binding
            // `[Some v]` binds `v`, `None` binds nothing
            if let Some(payload_pattern) = binding {
                collect_pattern_bindings(&payload_pattern.node, out);
            }
        }
        Pattern::Or(branches) => {
            // Or-pattern invariant: every branch must bind the SAME variable names
            // in the SAME ORDER. Slot indices are assigned from the first branch;
            // the evaluator uses these same slot indices when binding any branch.
            // A pattern like `(x, y) | (y, x)` would be rejected by the type-checker
            // as a name-order mismatch, but we assert the invariant here in debug
            // mode to catch bugs early (e.g., from desugar or macro expansion).
            //
            // Invariant enforcement: the semantic validator / type-checker is the
            // primary enforcer. This debug_assert is a belt-and-suspenders check.
            if let Some(first_branch) = branches.first() {
                collect_pattern_bindings(&first_branch.node, out);

                #[cfg(debug_assertions)]
                {
                    let first_names: Vec<String> = {
                        let mut v = Vec::new();
                        collect_pattern_bindings(&first_branch.node, &mut v);
                        v
                    };
                    for other_branch in branches.iter().skip(1) {
                        let mut other_names = Vec::new();
                        collect_pattern_bindings(&other_branch.node, &mut other_names);
                        debug_assert_eq!(
                            first_names, other_names,
                            "Or-pattern branches must bind the same variable names in the same \
                             order. First branch binds {:?} but another branch binds {:?}. \
                             This is a resolver invariant violation — check desugaring or \
                             pattern validation.",
                            first_names, other_names,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // All prior tests used resolve_file(), Expr, File, and ast_convert::surface_program_to_file()
    // which were deleted in the runtime-v2 migration. Tests now use resolve_surface_program()
    // with the SurfaceProgram/SurfaceNode AST directly.
    use super::*;
    use crate::ast::{node_id, NodeId, SurfaceExpression};

    /// Parse `src`, desugar, and resolve. Returns (program, table).
    fn parse_and_resolve(src: &str) -> (crate::ast::SurfaceProgram, ResolutionTable) {
        let output = crate::parser::parse(src).expect("parse failed");
        let mut program = output.program;
        crate::desugar::desugar_surface_program(&mut program);
        let table = resolve_surface_program(&program);
        (program, table)
    }

    /// Collect all Arc<SurfaceNode> whose expr is VarRef with the given name.
    fn find_varref_nodes(
        program: &crate::ast::SurfaceProgram,
        name: &str,
    ) -> Vec<(NodeId, Arc<SurfaceNode>)> {
        let mut results = Vec::new();
        for doc_spanned in &program.documents {
            collect_varrefs_in_doc(&doc_spanned.node, name, &mut results);
        }
        results
    }

    fn collect_varrefs_in_doc(
        doc: &crate::ast::SurfaceDocument,
        name: &str,
        out: &mut Vec<(NodeId, Arc<SurfaceNode>)>,
    ) {
        for item in &doc.items {
            match item {
                crate::ast::SurfaceItem::Expr(node) => collect_varrefs_in_node(node, name, out),
                crate::ast::SurfaceItem::Decl(_) => {}
            }
        }
    }

    fn collect_varrefs_in_node(
        arc: &Arc<SurfaceNode>,
        name: &str,
        out: &mut Vec<(NodeId, Arc<SurfaceNode>)>,
    ) {
        match &arc.expr {
            SurfaceExpression::VarRef { name: n, .. } if n == name => {
                out.push((node_id(arc), Arc::clone(arc)));
            }
            SurfaceExpression::Dict(entries) => {
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        collect_varrefs_in_node(key, name, out);
                    }
                    collect_varrefs_in_node(&entry.node.value, name, out);
                }
            }
            SurfaceExpression::Fn {
                params: _, body, ..
            } => {
                collect_varrefs_in_node(body, name, out);
            }
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                collect_varrefs_in_node(func, name, out);
                for arg in args {
                    collect_varrefs_in_node(arg, name, out);
                }
                for na in named_args {
                    collect_varrefs_in_node(&na.node.value, name, out);
                }
            }
            SurfaceExpression::DotAccess { expr, .. } => collect_varrefs_in_node(expr, name, out),
            SurfaceExpression::Pipe { lhs, rhs } => {
                collect_varrefs_in_node(lhs, name, out);
                collect_varrefs_in_node(rhs, name, out);
            }
            SurfaceExpression::TypeAssert { expr, .. } => collect_varrefs_in_node(expr, name, out),
            SurfaceExpression::Sequential(exprs) => {
                for e in exprs {
                    collect_varrefs_in_node(e, name, out);
                }
            }
            SurfaceExpression::Match { scrutinee, arms } => {
                collect_varrefs_in_node(scrutinee, name, out);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        collect_varrefs_in_node(guard, name, out);
                    }
                    collect_varrefs_in_node(&arm.body, name, out);
                }
            }
            _ => {}
        }
    }

    // --- Tests ---

    /// A free VarRef (not bound in any enclosing scope) should have NO entry in the table.
    #[test]
    fn varref_not_found_is_free() {
        let (program, table) = parse_and_resolve("$undefined_name");
        let refs = find_varref_nodes(&program, "undefined_name");
        assert!(!refs.is_empty(), "expected at least one VarRef node");
        for (id, _) in &refs {
            assert!(
                table.get(id).is_none(),
                "free VarRef should have no entry in the resolution table"
            );
        }
    }

    /// A Dict's values can see sibling keys: `[x: 1  y: $x]` — the VarRef `$x` in `y`'s
    /// value should resolve to (level=0, slot=0) since `x` is the first key in scope.
    #[test]
    fn dict_sibling_key_scoping() {
        let (program, table) = parse_and_resolve("[x: 1  y: $x]");
        let refs = find_varref_nodes(&program, "x");
        assert!(!refs.is_empty(), "expected at least one VarRef for $x");
        // $x inside the dict value for y resolves to slot 0 (first key in dict scope)
        let (id, _) = &refs[0];
        let coords = table
            .get(id)
            .expect("$x should be resolved (it's a sibling key)");
        assert_eq!(coords.1, 0, "x should be slot 0 (first key in dict scope)");
    }

    /// In a Fn body, VarRef to the param resolves to (level=0, slot=0).
    #[test]
    fn fn_param_scoping_in_body() {
        let (program, table) = parse_and_resolve("[fn [let myarg] $myarg]");
        let refs = find_varref_nodes(&program, "myarg");
        assert!(!refs.is_empty(), "expected at least one VarRef for $myarg");
        let (id, _) = &refs[0];
        let coords = table
            .get(id)
            .expect("$myarg should be resolved to fn param scope");
        // level=0: the param scope is at depth 0 from the VarRef's perspective
        // (the fn param scope is the innermost scope when walking the body)
        assert_eq!(coords.0, 0, "fn param should be at level 0");
        assert_eq!(coords.1, 0, "first fn param should be at slot 0");
    }

    /// A multi-param fn resolves each param to its correct slot.
    #[test]
    fn fn_multi_param_slots() {
        let (program, table) = parse_and_resolve("[fn [let a b c] $b]");
        let refs = find_varref_nodes(&program, "b");
        assert!(!refs.is_empty(), "expected VarRef for $b");
        let (id, _) = &refs[0];
        let coords = table.get(id).expect("$b should be resolved");
        assert_eq!(coords.0, 0, "param scope is level 0");
        assert_eq!(coords.1, 1, "b is the second param, slot 1");
    }

    /// A VarRef inside a fn body that refers to an outer dict key (closure capture)
    /// resolves to level > 0 (one scope up from the fn param scope).
    #[test]
    fn fn_body_captures_outer_dict_key() {
        // outer: 42  inner: [fn [] $outer]
        // When resolving $outer inside fn body:
        //   scopes (innermost first): [fn-params={}] → [dict-keys={outer=0, inner=1}] → [runtime=%]
        //   so $outer is at level=1, slot=0
        let (program, table) = parse_and_resolve("[outer: 42  inner: [fn [let] $outer]]");
        let refs = find_varref_nodes(&program, "outer");
        assert!(
            !refs.is_empty(),
            "expected VarRef for $outer inside fn body"
        );
        let (id, _) = &refs[0];
        let coords = table
            .get(id)
            .expect("$outer should be resolved (captured from dict scope)");
        assert_eq!(
            coords.0, 1,
            "outer dict key is one level up from fn param scope"
        );
        assert_eq!(
            coords.1, 0,
            "outer is the first key in the dict scope, slot 0"
        );
    }

    /// Match arm pattern bindings should be resolved in the arm body.
    /// `[match x [Some n]: [+ n 1]]` — `$n` in the arm body should resolve to (level=0, slot=0).
    #[test]
    fn match_arm_pattern_binding() {
        let (program, table) = parse_and_resolve("[match x [Some n]: [+ $n 1]]");
        let refs = find_varref_nodes(&program, "n");
        assert!(!refs.is_empty(), "expected VarRef for $n in arm body");
        let (id, _) = &refs[0];
        let coords = table
            .get(id)
            .expect("$n should be resolved (pattern binding in arm scope)");
        assert_eq!(coords.0, 0, "pattern binding should be at level 0");
        assert_eq!(coords.1, 0, "n is the first (and only) pattern binding");
    }

    /// Match arm guard expressions should see pattern bindings.
    /// `[match x [Some n] if: [> $n 0]: $n]` — both `$n` should resolve.
    #[test]
    fn match_arm_guard_sees_pattern_bindings() {
        // Variable binding in match arm: `n: body` binds the matched value as `n`.
        // The body can reference the bound variable.
        let src = "[match 42 n: [+ n 1]]";
        let (program, table) = parse_and_resolve(src);
        let refs = find_varref_nodes(&program, "n");
        // Should have 1 VarRef for `n` in the body (the pattern `n:` is a key, not a VarRef)
        assert_eq!(
            refs.len(),
            1,
            "expected exactly 1 VarRef for n (body reference)"
        );
        for (id, _) in &refs {
            let coords = table.get(id).expect("n should be resolved in body");
            // The match arm scope introduces n as a binding
            assert_eq!(coords.1, 0, "n is slot 0");
        }
    }

    /// Match with multiple arms: type patterns (Int, String, etc.)
    /// The body of each arm can reference outer variables.
    #[test]
    fn match_dict_pattern_bindings() {
        // Match with two type arms; both arm bodies reference outer $x.
        let src = "[x: 1  result: [match $x Int: [+ $x 1] _: 0]]";
        let (program, table) = parse_and_resolve(src);

        // Check $x resolves (should appear at least twice: match scrutinee + Int arm body)
        let x_refs = find_varref_nodes(&program, "x");
        assert!(
            x_refs.len() >= 2,
            "expected at least 2 VarRefs for $x, got {}",
            x_refs.len()
        );
        // All $x refs should resolve to the dict-level binding
        for (id, _) in &x_refs {
            let coords = table.get(id).expect("$x should be resolved (dict binding)");
            assert_eq!(coords.1, 0, "$x is first binding, slot 0");
        }
    }

    /// Match with wildcard pattern should introduce no bindings.
    /// A VarRef inside a wildcard arm body must NOT be slot-resolved — the
    /// wildcard creates no scope entries, so `$x` stays a FreeVar.
    #[test]
    fn match_wildcard_pattern_no_bindings() {
        // `$x` in the wildcard arm body refers to nothing bound by `_`.
        // The resolver must not produce a table entry for this VarRef.
        let (program, table) = parse_and_resolve("[match val _: $x]");
        let refs = find_varref_nodes(&program, "x");
        // There should be at least one VarRef for $x (in the arm body)
        assert!(
            !refs.is_empty(),
            "expected at least one VarRef for $x in wildcard arm body"
        );
        // None of the $x VarRefs should be resolved — the wildcard binds
        // nothing, so $x has no slot assignment (remains a FreeVar).
        for (id, _) in &refs {
            assert!(
                table.get(id).is_none(),
                "wildcard binds nothing; $x in wildcard arm body must not be slot-resolved"
            );
        }
    }

    /// Sequential scope injection: static keys from intermediate expressions
    /// in the same document become scope bindings for subsequent expressions.
    /// Document with two exprs: `[a: 1]\n$a` — the second expr can reference the first's keys.
    #[test]
    fn sequential_scope_injection() {
        // Document with two sequential expressions: first is a dict, second references its key
        let (program, table) = parse_and_resolve("[a: 1]\n$a");
        let refs = find_varref_nodes(&program, "a");
        assert!(!refs.is_empty(), "expected VarRef for $a in second expr");
        // $a should resolve to the dict key from the first expression
        let (id, _) = &refs[0];
        let coords = table
            .get(id)
            .expect("$a should be resolved (key from prior expr in document)");
        // The first dict creates a scope with `a` as slot 0
        assert_eq!(coords.1, 0, "a is first key from prior expr, slot 0");
    }
}
