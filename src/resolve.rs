//! Variable resolution pass: assigns (level, slot) de Bruijn coordinates to VarRef nodes.
//!
//! This is Phase 1 of the arena allocation strategy. The resolver walks the AST and
//! assigns compile-time slot indices to static variable references before evaluation begins.
//!
//! **Invariants:**
//! - Writes de Bruijn coordinates inline to the `resolution` field of `VarRef` and
//!   leading-dot `DotAccess` nodes. The OnceLock ensures write-once semantics.
//! - Must run after desugaring (sees $_ as Fn nodes, not VarRef("_")).
//! - Must run before typechecking and evaluation (both consumers of resolved coords).
//!
//! See doc/whatif/arena-patterns.md §Variable Resolution Pass Design for the full specification.

use crate::ast::{
    Pattern, Spanned, SurfaceDeclaration, SurfaceDocument, SurfaceEntry, SurfaceExpression,
    SurfaceItem, SurfaceNode, SurfaceProgram,
};
use std::sync::{Arc, RwLock};

// ============================================================================
// runtime-v2: SurfaceProgram resolver — writes inline resolution to AST nodes
/// Variable resolution pass for the Surface AST.
///
/// Walks a `SurfaceProgram` and writes de Bruijn `(level, slot)` coordinates
/// directly into the `resolution` field of each `VarRef` and leading-dot
/// `DotAccess` node. The OnceLock enforces write-once semantics.
struct SurfaceResolver {
    scopes: Vec<indexmap::IndexMap<String, u32>>,
    /// Unresolved VarRef / leading-dot references: (name, span).
    /// Collected during walking; surfaced as "undefined-variable" errors by builtin-resolve.
    pub unresolved: Vec<(String, crate::ast::Span)>,
    /// Module slot tables — maps a binding name to its exported names in slot order.
    ///
    /// Populated when the resolver detects `[name: [include %libdir "mod.llt"]]` patterns.
    /// Used to resolve `name.field` DotAccess nodes to positional slot indices, enabling
    /// the lowerer to emit `slot-get` (O(1)) instead of `field-get` (string-keyed).
    ///
    /// Key: binding name (e.g. `"math"`). Value: ordered list of exported names matching
    /// the slot order of the included module's last top-level dict.
    module_slots: std::collections::HashMap<String, Vec<String>>,
}

impl SurfaceResolver {
    fn new() -> Self {
        Self {
            scopes: Vec::new(),
            unresolved: Vec::new(),
            module_slots: std::collections::HashMap::new(),
        }
    }

    /// Build a resolver pre-seeded from a runtime environment chain.
    ///
    /// Walks the env chain from outermost (root) to innermost (env itself), collecting
    /// (name → slot) maps for each frame. Outermost frame goes at index 0 of scopes
    /// (deepest de Bruijn level). This makes de Bruijn level 0 = innermost (the provided
    /// env itself), matching the evaluator's `get_slot` convention.
    ///
    /// After this constructor, names resolvable from the env chain produce `Var` coordinates
    /// at runtime evaluation. Any name not found in the chain is a genuine compile error.
    fn from_env(env: &Arc<RwLock<crate::value::Environment>>) -> Self {
        let mut frames: Vec<indexmap::IndexMap<String, u32>> = Vec::new();
        let mut current = Some(Arc::clone(env));
        while let Some(frame_arc) = current {
            let frame = frame_arc.read().unwrap();
            let mut scope: indexmap::IndexMap<String, u32> = indexmap::IndexMap::new();
            for (i, key) in frame.slot_names.iter().enumerate() {
                scope.insert(key.clone(), u32::try_from(i).expect("slot overflow"));
            }
            frames.push(scope);
            current = frame.parent.as_ref().map(Arc::clone);
        }
        // frames[0] is innermost (env itself), frames[last] is outermost root.
        // We want frames[0] = deepest ancestor (outermost), frames[last] = env itself,
        // so that when the resolver iterates rev() it finds level 0 = env itself.
        frames.reverse();
        Self {
            scopes: frames,
            unresolved: Vec::new(),
            module_slots: std::collections::HashMap::new(),
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
        self.resolve_name_skipping(name, 0)
    }

    /// Resolve `name` starting from scope level `skip` (0 = innermost, 1 = parent, …).
    /// The returned de Bruijn level is relative to the full scope stack, so `skip=1` and
    /// finding the name immediately gives `level = 1` — correct for evaluator lookup
    /// without any manual offset correction.
    fn resolve_name_skipping(&self, name: &str, skip: usize) -> Option<(u32, u32)> {
        for (offset, scope) in self.scopes.iter().rev().skip(skip).enumerate() {
            if let Some(&slot) = scope.get(name) {
                let level = u32::try_from(skip + offset).expect("scope depth overflow");
                return Some((level, slot));
            }
        }
        None
    }

    fn walk_surface_node(&mut self, arc: &Arc<SurfaceNode>) {
        self.walk_surface_expr(arc, &arc.expr);
    }

    /// Resolve de Bruijn coordinates for all `Pattern::Pin` nodes in a pattern tree.
    ///
    /// Called from `Match` arm processing BEFORE entering the arm's binding scope,
    /// so that Pin names are resolved in the ENCLOSING scope (where pinned values live).
    ///
    /// Writes `resolution.set(Some((level, slot)))` when the name is in scope,
    /// or `resolution.set(None)` when not in scope (wildcard behavior at eval time).
    ///
    /// This enables the evaluator to use `get_slot` instead of `get_by_name` for all
    /// pin lookups in shorthand match arms.
    fn resolve_pins_in_pattern(&self, pat: &crate::ast::Pattern) {
        use crate::ast::Pattern;
        match pat {
            Pattern::Pin(name, resolution) => {
                if let Some(coords) = self.resolve_name(name) {
                    resolution.set(Some(coords));
                } else {
                    // Name not in scope — wildcard behavior; set None to mark resolver ran.
                    resolution.set(None);
                }
            }
            Pattern::Dict { fields, .. } => {
                for (_, field_pat) in fields {
                    self.resolve_pins_in_pattern(&field_pat.node);
                }
            }
            Pattern::Constructor { binding, .. } => {
                if let Some(b) = binding {
                    self.resolve_pins_in_pattern(&b.node);
                }
            }
            Pattern::Or(branches) => {
                for b in branches {
                    self.resolve_pins_in_pattern(&b.node);
                }
            }
            Pattern::TypeAssertPending { inner, .. } | Pattern::TypeAssert { inner, .. } => {
                if let Some(inner_pat) = inner {
                    self.resolve_pins_in_pattern(&inner_pat.node);
                }
            }
            // Leaf patterns with no Pin sub-patterns — nothing to resolve.
            Pattern::Wildcard | Pattern::Literal(_) | Pattern::Predicate(_) => {}
        }
    }

    fn walk_surface_expr(&mut self, arc: &Arc<SurfaceNode>, expr: &SurfaceExpression) {
        match expr {
            SurfaceExpression::VarRef { name, resolution, .. } => {
                if let Some(coords) = self.resolve_name(name) {
                    resolution.set(Some(coords));
                } else {
                    // Mark as unresolvable so the lowerer knows the resolver ran but failed.
                    resolution.set(None);
                    // Collect for structured error reporting by builtin-resolve.
                    self.unresolved.push((name.clone(), arc.span.clone()));
                }
            }

            SurfaceExpression::Dict(entries) => {
                let static_keys = surface_dict_static_keys(entries);

                // Walk key expressions in outer scope
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        self.walk_surface_node(key);
                    }
                }

                // Module slot detection: scan for `[name: [include %libdir "path.llt"]]` entries.
                // When found, load the module's exported names (synchronously) and record them
                // in module_slots so that subsequent `name.field` DotAccess nodes can be resolved
                // to positional slot indices.
                for entry in entries.iter() {
                    if let Some(key_node) = &entry.node.key {
                        let binding_name = match &key_node.expr {
                            SurfaceExpression::Str(s) => Some(s.clone()),
                            SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                            SurfaceExpression::Annotated { name, .. } => Some(name.clone()),
                            _ => None,
                        };
                        if let Some(name) = binding_name {
                            if let Some(exported) = detect_include_module(&entry.node.value.expr) {
                                self.module_slots.insert(name, exported);
                            }
                        }
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

            SurfaceExpression::DotAccess { expr: Some(inner), field, field_slot, resolution } => {
                self.walk_surface_node(inner);
                // Resolve "field-get" to get the root scope level for the lowerer.
                // The lowerer reads this to emit CoreExpr::Call with the correct root_level.
                // slot-get is at the same level (root scope), slot 1 — the lowerer knows this.
                if let Some(coords) = self.resolve_name("field-get") {
                    resolution.set(Some(coords));
                } else {
                    // field-get not in scope — this should never happen in a correctly built env.
                    resolution.set(None);
                }

                // Module slot resolution: if the target is a VarRef to a known module AND
                // the field is a string key, look up the field's slot index in the module's
                // exported slot table and write it into field_slot. This enables the lowerer
                // to emit `slot-get` (O(1) positional access) instead of `field-get` (string lookup).
                if let crate::ast::DotKey::Ident(field_name) = field {
                    if let SurfaceExpression::VarRef { name: module_name, .. } = &inner.expr {
                        if let Some(exported_names) = self.module_slots.get(module_name.as_str()) {
                            if let Some(pos) = exported_names.iter().position(|n| n == field_name) {
                                field_slot.set(u32::try_from(pos).expect("slot overflow"));
                            }
                        }
                    }
                }
            }

            // Leading-dot form: `.name` with no preceding expression.
            // Semantics: skip the innermost scope frame and resolve `name` in the parent scope.
            // This allows `[x: "shadowed"  outer-x: .x]` inside a dict to reference the `x`
            // from the enclosing scope rather than the self-referential sibling key.
            SurfaceExpression::DotAccess {
                expr: None,
                field: crate::ast::DotKey::Ident(name),
                resolution,
                ..
            } => {
                // resolve_name_skipping(name, 1) walks the scope stack starting one level up,
                // returning a de Bruijn level relative to the FULL stack — no manual +1 needed.
                // If not found: mark as unresolvable — lowering emits CoreExpr::Error.
                if let Some(coords) = self.resolve_name_skipping(name, 1) {
                    resolution.set(Some(coords));
                } else {
                    resolution.set(None);
                    self.unresolved.push((format!(".{}", name), arc.span.clone()));
                }
            }

            // Leading-dot with integer key: `.0` — no parent-scope numeric lookup.
            // Treated as Error. The parser already rejects this form at parse time,
            // so this arm is a safety fallback. Mark as unresolvable so the lowerer
            // consistently sees `Some(None)` for any leading-dot node we processed.
            SurfaceExpression::DotAccess {
                expr: None,
                field: crate::ast::DotKey::Int(_),
                resolution,
                ..
            } => {
                resolution.set(None);
            }

            // Pipe: walk both sides (the lowering pass will rewrite pipe to call)
            SurfaceExpression::Pipe { lhs, rhs } => {
                self.walk_surface_node(lhs);
                self.walk_surface_node(rhs);
            }

            SurfaceExpression::TypeAssert { annotation, expr, .. } => {
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

                    // Resolve Pin pattern names BEFORE entering the pattern's binding scope.
                    // Pin patterns compare against names in the ENCLOSING scope (not the arm's
                    // own binding scope). This writes de Bruijn coordinates into the OnceLock
                    // on each Pin so the evaluator can use get_slot instead of get_by_name.
                    self.resolve_pins_in_pattern(&arm.pattern.node);

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

            SurfaceExpression::CaseArm { let_bindings, pattern, body } => {
                // let_bindings declares binding targets (not variable references — do NOT
                // walk it). Extract declared names, bring them into scope, then walk pattern
                // (to resolve pin vars in the outer scope) and body (with declared names in scope).
                let bound_names = extract_surface_let_binding_names(let_bindings);
                let has_bindings = !bound_names.is_empty();
                if has_bindings {
                    self.enter_scope(&bound_names);
                }
                self.walk_surface_node(pattern);
                self.walk_surface_node(body);
                if has_bindings {
                    self.exit_scope();
                }
            }

            SurfaceExpression::Annotated { annotation, .. } => {
                self.walk_surface_annotation(annotation);
            }

            // Terminals with no child expressions
            SurfaceExpression::Int(_)
            | SurfaceExpression::U64(_)
            | SurfaceExpression::Float(_)
            | SurfaceExpression::Str(_)
            | SurfaceExpression::Rest(..)
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
            SurfaceDeclaration::MacroDecl { .. } => {
                // Macro bodies are AST templates resolved at expansion-time call sites,
                // not at definition time. Walking params or body here produces false E002s
                // for names that are valid when the macro is called but not yet in scope
                // at definition time. Resolution happens in eval_surface_fn via
                // resolve_surface_node_with_env when the macro is expanded.
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

    fn finish(self) -> Vec<(String, crate::ast::Span)> {
        self.unresolved
    }
}

/// Resolve all VarRef nodes in a SurfaceProgram, writing de Bruijn coordinates inline.
///
/// This is the runtime-v2 entry point for variable resolution. Walks the program and
/// writes `(level, slot)` coordinates directly into the `resolution` field of each
/// `VarRef` and leading-dot `DotAccess` node. Returns only unresolved name errors.
///
/// The resolver models the same scope-chain semantics as the evaluator: each
/// intermediate dict expression's static keys become scope bindings for subsequent
/// expressions within the same document.
pub fn resolve_surface_program(program: &SurfaceProgram) -> Vec<(String, crate::ast::Span)> {
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

/// Resolve all VarRef nodes in a SurfaceProgram seeded from a runtime environment chain.
///
/// This is the primary entry point for `builtin-resolve` when called with an `env:` argument.
/// The resolver is pre-seeded from the env chain so that names from prelude, stdlib, and
/// any other ambient bindings are given proper de Bruijn coordinates rather than producing
/// resolution errors.
///
/// Names still unresolvable after searching the env chain will have no table entry;
/// the lowering pass emits `CoreExpr::Error` for them (genuine compile errors).
/// Resolve a single surface node (and its entire subtree) against a runtime env.
///
/// Used by `eval_surface_fn` to resolve macro transformer bodies at expansion time,
/// before the main per-file resolution pass runs. This gives VarRefs in macro bodies
/// proper de Bruijn coordinates so evaluation succeeds.
pub fn resolve_surface_node_with_env(
    node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<crate::value::Environment>>,
) -> Vec<(String, crate::ast::Span)> {
    let mut resolver = SurfaceResolver::from_env(env);
    resolver.walk_surface_node(node);
    resolver.finish()
}

pub fn resolve_surface_program_with_env(
    program: &SurfaceProgram,
    env: &Arc<RwLock<crate::value::Environment>>,
) -> Vec<(String, crate::ast::Span)> {
    let mut resolver = SurfaceResolver::from_env(env);
    let mut named_sections: Vec<String> = Vec::new();

    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;

        if doc.stage == Some(crate::ast::Stage::Type) {
            continue;
        }

        // Push a `%` scope to match the doc_env frame that eval_surface_file_with_input
        // creates at runtime. This is the eval_surface_file execution path.
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

/// Resolve a program seeded from a runtime env, WITHOUT adding a synthetic `%` scope.
///
/// Used by `builtin-resolve` (called from tinct code). In the `builtin-eval` execution
/// path, `%` is available via the env chain (not as a separate doc_env frame), so no
/// extra scope is added. Adding one would shift all prelude binding levels by 1.
pub fn resolve_surface_program_for_builtin_eval(
    program: &SurfaceProgram,
    env: &Arc<RwLock<crate::value::Environment>>,
) -> Vec<(String, crate::ast::Span)> {
    let mut resolver = SurfaceResolver::from_env(env);

    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;
        if doc.stage == Some(crate::ast::Stage::Type) {
            continue;
        }
        resolver.walk_surface_document(doc);
    }

    resolver.finish()
}

/// Resolve a single document seeded from a runtime env, with proper scope accumulation
/// for intermediate dict expressions.
///
/// This mirrors what `eval_document_exprs_with_env` does at runtime: each intermediate
/// dict expression (all but the last) pushes its static keys as a scope frame before
/// the resolver walks the next expression. This ensures de Bruijn coordinates produced
/// here match what the evaluator expects at runtime.
///
/// Used by `builtin-resolve` when called with a `Value::Document` argument.
pub fn resolve_surface_document_with_scope_accumulation(
    doc: &SurfaceDocument,
    env: &std::sync::Arc<std::sync::RwLock<crate::value::Environment>>,
) -> Vec<(String, crate::ast::Span)> {
    let mut resolver = SurfaceResolver::from_env(env);

    // Collect expression items only (skip Decl items, which have no runtime presence).
    let expr_items: Vec<&std::sync::Arc<SurfaceNode>> = doc
        .items
        .iter()
        .filter_map(|item| {
            if let SurfaceItem::Expr(node) = item {
                Some(node)
            } else {
                None
            }
        })
        .collect();

    let last_idx = expr_items.len().saturating_sub(1);
    let mut injected = 0usize;

    for (i, node) in expr_items.iter().enumerate() {
        resolver.walk_surface_node(node);
        // After each non-last expression, push scope for its static keys — mirrors
        // what eval_document_exprs_with_env does at runtime when it creates env frames
        // for intermediate dict bindings.
        if i < last_idx {
            if let Some(keys) = surface_node_static_keys(node) {
                if !keys.is_empty() {
                    resolver.enter_scope(&keys);
                    injected += 1;
                }
            }
        }
    }

    // Pop the injected intermediate scopes (order doesn't matter for scope stack cleanup).
    for _ in 0..injected {
        resolver.exit_scope();
    }

    resolver.finish()
}

/// Extract static string-keyed names from a SurfaceExpression::Dict's entries.
/// Bare identifier keys are normalized to `Str` by the parser's `push_value` before this
/// function is reached, so only `Str` and `Annotated` arms are needed here.
///
/// Anonymous InstanceDecl entries (no outer key) are excluded: lower.rs flattens their
/// instance binding entries directly into the outer dict at lower time, after resolution.
/// The flattened binding names (e.g., `ɪɴꜱᴛᴀɴᴄᴇ⧼Equatable∷=⟨Int⟩⧽`) are synthetic
/// and not visible in the surface AST, so they are not pre-registered as letrec slots.
/// Their binding names are synthetic, inserted by lower.rs at lower time.
///
/// Named InstanceDecl entries (with outer key like `EquatableInt: [instance ...]`) ARE
/// included: the outer key is a real letrec slot that binds the instance dict as a value.
fn surface_dict_static_keys(entries: &[Spanned<SurfaceEntry>]) -> Vec<String> {
    // Match exactly what the evaluator's lower.rs + eval_dict_core consider "static":
    // - Entries whose key is Str/VarRef/Annotated (static string key)
    // - Excluding non-runtime Decl forms that lowering skips:
    //   ClassDecl, MacroDecl, SyntaxClass, anonymous InstanceDecl (no outer key)
    //   Named InstanceDecl and TypeAlias entries ARE included (lowering produces runtime values).
    entries
        .iter()
        .filter(|entry| {
            // Skip entries whose VALUE is a Decl that lower.rs discards:
            // - ClassDecl, MacroDecl, SyntaxClass → `_ => { continue; }` in lower.rs Dict arm
            // - Anonymous InstanceDecl (key=None) → skipped after recent change
            if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                match decl.as_ref() {
                    SurfaceDeclaration::ClassDecl { .. }
                    | SurfaceDeclaration::MacroDecl { .. }
                    | SurfaceDeclaration::SyntaxClass { .. } => return false,
                    SurfaceDeclaration::InstanceDecl { .. } => {
                        // Anonymous (no outer key): skipped by lowering.
                        // Named (has outer key): included.
                        return entry.node.key.is_some();
                    }
                    _ => {} // TypeAlias, Splice: included
                }
            }
            true
        })
        .filter_map(|entry| {
            entry
                .node
                .key
                .as_ref()
                .and_then(|key_node| match &key_node.expr {
                    SurfaceExpression::Str(s) => Some(s.clone()),
                    SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
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

/// Extract the declared binding names from a `[let n1 n2 ...]` surface node.
///
/// Used by the `CaseArm` resolver to determine which names the arm introduces into
/// scope. The `let_bindings` node is a `SurfaceExpression::LetDecl` whose `bindings`
/// vector holds one node per declared name. Each binding node may be:
/// - `SurfaceExpression::VarRef { name }` — a plain name
/// - `SurfaceExpression::Annotated { name, .. }` — an annotated name (`n@Type`)
/// - `SurfaceExpression::Rest(Some(name), _)` — variadic (`...name`)
///
/// Wildcard (`_`) and unnamed rest (`...`) are skipped — they bind nothing.
fn extract_surface_let_binding_names(lb: &Arc<SurfaceNode>) -> Vec<String> {
    match &lb.expr {
        SurfaceExpression::LetDecl { bindings } => bindings
            .iter()
            .filter_map(|b| match &b.expr {
                SurfaceExpression::VarRef { name, .. } if name != "_" => Some(name.clone()),
                SurfaceExpression::Annotated { name, .. } if name != "_" => Some(name.clone()),
                SurfaceExpression::Rest(Some(name), _) if name != "_" => Some(name.clone()),
                _ => None,
            })
            .collect(),
        // Not a LetDecl node — return empty (legacy 2-arg form or malformed input)
        _ => Vec::new(),
    }
}

/// Extract all variable names bound by a pattern.
/// This is used to create scope bindings for match arm bodies.
///
/// Bare lowercase names are Pin patterns and do NOT introduce bindings — they compare
/// or act as wildcards. This function returns [] for all leaf patterns.
///
/// Examples:
/// - `_` (Wildcard) → []
/// - `x` (Pin, unresolved) → [] (wildcard, no binding)
/// - `$x` (Pin, escaped) → [] (compare, no binding)
/// - `[Some v]` (Constructor with Pin binding) → [] (Pin doesn't bind)
/// - `[Dict {x, y: z}]` → [] (Pin sub-patterns don't bind)
/// - `[seq h t]` → [] (Pin sub-patterns don't bind)
/// - `x | y` (Or with Pin branches) → []
fn extract_pattern_bindings(pattern: &Spanned<Pattern>) -> Vec<String> {
    let mut bindings = Vec::new();
    collect_pattern_bindings(&pattern.node, &mut bindings);
    bindings
}

/// Recursively collect all variable bindings from a pattern.
#[allow(clippy::only_used_in_recursion)]
fn collect_pattern_bindings(pattern: &Pattern, out: &mut Vec<String>) {
    match pattern {
        Pattern::Wildcard => {
            // Wildcard matches anything but binds no variables
        }
        Pattern::Literal(_) => {
            // Literal patterns bind no variables
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
        Pattern::Pin(..) => {
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
        Pattern::Predicate(_) => {
            // T-1140: Predicate patterns introduce no variable bindings.
        }
    }
}

/// Detect whether a surface expression is a `[include %libdir "path.llt"]` call pattern
/// and, if so, return the exported slot names from that file.
///
/// ## Pattern matched
///
/// `Call { func: VarRef("include"), args: [VarRef("%libdir"), Str("path.llt")] }` or
/// `Call { func: VarRef("include"), args: [Str("path.llt")] }` (cwd-relative include).
///
/// Returns `Some(names)` when the file exists, can be parsed, and its last top-level dict
/// has static string keys. Returns `None` on any failure (missing file, parse error, etc.)
/// to allow graceful degradation — callers continue with `field-get` (key-based lookup).
///
/// The result is cached in `module_slots` by the caller to avoid re-loading on every
/// dot-access reference.
fn detect_include_module(expr: &SurfaceExpression) -> Option<Vec<String>> {
    // Match the Call expression pattern for `[include %libdir "path.llt"]`
    let path_str = extract_include_path(expr)?;

    // Locate the stdlib directory using the same heuristic as lib.rs::find_libdir_path().
    // %cwd-relative includes are not currently supported (no cwd available at resolve time).
    let libdir = crate::find_libdir_path()?;
    let file_path = libdir.join(&path_str);

    // Read and parse the file synchronously.
    let source = std::fs::read_to_string(&file_path).ok()?;
    let parsed = crate::parser::parse(&source).ok()?;
    let mut program = parsed.program;
    crate::desugar::desugar_surface_program(&mut program);

    // Extract the exported names from the last top-level dict in the program.
    // The two-dict convention: the second (last) dict is the public API.
    // We collect static keys from the last document's last expression.
    extract_module_exported_names(&program)
}

/// Extract the string path argument from an `[include ... "path"]` call expression.
///
/// Matches:
/// - `[include %libdir "path.llt"]` — %libdir-relative (the common stdlib pattern)
/// - `[include %cwd "path.llt"]` — cwd-relative (not supported for slot resolution)
///
/// Returns `Some(path)` for %libdir-relative includes; `None` otherwise.
fn extract_include_path(expr: &SurfaceExpression) -> Option<String> {
    match expr {
        SurfaceExpression::Call { func, args, .. } => {
            // Check that the function is `include` or `builtin-include`
            let is_include = match &func.expr {
                SurfaceExpression::VarRef { name, .. } => {
                    name == "include" || name == "builtin-include"
                }
                _ => false,
            };
            if !is_include {
                return None;
            }
            // Pattern: [include %libdir "path.llt"] — 2 args, first is %libdir VarRef
            if args.len() == 2 {
                let is_libdir = matches!(&args[0].expr,
                    SurfaceExpression::VarRef { name, .. } if name == "%libdir");
                if is_libdir {
                    if let SurfaceExpression::Str(path) = &args[1].expr {
                        return Some(path.clone());
                    }
                }
            }
            // Pattern: [include "path.llt"] — single string arg (cwd-relative; not supported)
            None
        }
        _ => None,
    }
}

/// Extract the exported (public) slot names from a parsed module program.
///
/// Follows the two-dict library convention: the last expression in the last document is
/// the public API dict. Returns its static keys in source order (slot order).
fn extract_module_exported_names(program: &crate::ast::SurfaceProgram) -> Option<Vec<String>> {
    // Find the last non-type-stage document
    let last_doc = program
        .documents
        .iter()
        .rev()
        .find(|d| d.node.stage != Some(crate::ast::Stage::Type))?;

    // Find the last top-level Expr item in that document
    let last_expr = last_doc
        .node
        .items
        .iter()
        .rev()
        .find_map(|item| match item {
            crate::ast::SurfaceItem::Expr(node) => Some(node),
            _ => None,
        })?;

    // It must be a Dict with static keys
    match &last_expr.expr {
        SurfaceExpression::Dict(entries) => {
            let keys = surface_dict_static_keys(entries);
            if keys.is_empty() {
                None
            } else {
                Some(keys)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::SurfaceExpression;

    /// Parse `src`, desugar, and resolve. Returns the resolved program.
    /// Resolution is written inline to AST nodes; errors are discarded for these tests.
    fn parse_and_resolve(src: &str) -> crate::ast::SurfaceProgram {
        let output = crate::parser::parse(src).expect("parse failed");
        let mut program = output.program;
        crate::desugar::desugar_surface_program(&mut program);
        let _resolve_errors = resolve_surface_program(&program);
        program
    }

    /// Collect all Arc<SurfaceNode> whose expr is VarRef with the given name.
    fn find_varref_nodes(
        program: &crate::ast::SurfaceProgram,
        name: &str,
    ) -> Vec<Arc<SurfaceNode>> {
        let mut results = Vec::new();
        for doc_spanned in &program.documents {
            collect_varrefs_in_doc(&doc_spanned.node, name, &mut results);
        }
        results
    }

    fn collect_varrefs_in_doc(
        doc: &crate::ast::SurfaceDocument,
        name: &str,
        out: &mut Vec<Arc<SurfaceNode>>,
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
        out: &mut Vec<Arc<SurfaceNode>>,
    ) {
        match &arc.expr {
            SurfaceExpression::VarRef { name: n, .. } if n == name => {
                out.push(Arc::clone(arc));
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
            SurfaceExpression::DotAccess {
                expr: Some(inner), ..
            } => collect_varrefs_in_node(inner, name, out),
            SurfaceExpression::DotAccess { expr: None, .. } => {}
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
            SurfaceExpression::CaseArm {
                let_bindings,
                pattern,
                body,
            } => {
                collect_varrefs_in_node(let_bindings, name, out);
                collect_varrefs_in_node(pattern, name, out);
                collect_varrefs_in_node(body, name, out);
            }
            _ => {}
        }
    }

    /// Read the inline resolution from a VarRef node.
    fn varref_resolution(node: &Arc<SurfaceNode>) -> Option<Option<(u32, u32)>> {
        match &node.expr {
            SurfaceExpression::VarRef { resolution, .. } => resolution.get(),
            _ => None,
        }
    }

    // --- Tests ---

    /// A free VarRef (not bound in any enclosing scope) should have `Some(None)` resolution
    /// (resolver ran but found no binding).
    #[test]
    fn varref_not_found_is_free() {
        let program = parse_and_resolve("$undefined_name");
        let refs = find_varref_nodes(&program, "undefined_name");
        assert!(!refs.is_empty(), "expected at least one VarRef node");
        for node in &refs {
            // Resolver ran and found no binding → Some(None)
            assert_eq!(
                varref_resolution(node),
                Some(None),
                "free VarRef should have Some(None) resolution (unresolvable)"
            );
        }
    }

    /// A Dict's values can see sibling keys: `[x: 1  y: $x]` — the VarRef `$x` in `y`'s
    /// value should resolve to (level=0, slot=0) since `x` is the first key in scope.
    #[test]
    fn dict_sibling_key_scoping() {
        let program = parse_and_resolve("[x: 1  y: $x]");
        let refs = find_varref_nodes(&program, "x");
        assert!(!refs.is_empty(), "expected at least one VarRef for $x");
        let coords = varref_resolution(&refs[0])
            .expect("resolver should have run")
            .expect("$x should be resolved (it's a sibling key)");
        assert_eq!(coords.1, 0, "x should be slot 0 (first key in dict scope)");
    }

    /// In a Fn body, VarRef to the param resolves to (level=0, slot=0).
    #[test]
    fn fn_param_scoping_in_body() {
        let program = parse_and_resolve("[fn [let myarg] $myarg]");
        let refs = find_varref_nodes(&program, "myarg");
        assert!(!refs.is_empty(), "expected at least one VarRef for $myarg");
        let coords = varref_resolution(&refs[0])
            .expect("resolver should have run")
            .expect("$myarg should be resolved to fn param scope");
        assert_eq!(coords.0, 0, "fn param should be at level 0");
        assert_eq!(coords.1, 0, "first fn param should be at slot 0");
    }

    /// A multi-param fn resolves each param to its correct slot.
    #[test]
    fn fn_multi_param_slots() {
        let program = parse_and_resolve("[fn [let a b c] $b]");
        let refs = find_varref_nodes(&program, "b");
        assert!(!refs.is_empty(), "expected VarRef for $b");
        let coords = varref_resolution(&refs[0])
            .expect("resolver should have run")
            .expect("$b should be resolved");
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
        let program = parse_and_resolve("[outer: 42  inner: [fn [let] $outer]]");
        let refs = find_varref_nodes(&program, "outer");
        assert!(
            !refs.is_empty(),
            "expected VarRef for $outer inside fn body"
        );
        let coords = varref_resolution(&refs[0])
            .expect("resolver should have run")
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

    /// Outer let binding resolved inside a match arm body.
    #[test]
    fn match_arm_pattern_binding() {
        let program = parse_and_resolve("[x: 1  result: [match $x 1: $x _: $x]]");
        let refs = find_varref_nodes(&program, "x");
        assert!(
            refs.len() >= 3,
            "expected at least 3 VarRefs for $x, got {}",
            refs.len()
        );
        for node in &refs {
            let coords = varref_resolution(node)
                .expect("resolver should have run")
                .expect("$x should be resolved (dict-level binding visible in arm body)");
            assert_eq!(coords.1, 0, "x is the first key in the dict scope, slot 0");
        }
    }

    /// Function parameter bindings are visible inside match arm bodies.
    #[test]
    fn match_arm_guard_sees_pattern_bindings() {
        let src = "[fn [let x] [match $x 1: $x _: $x]]";
        let program = parse_and_resolve(src);
        let refs = find_varref_nodes(&program, "x");
        assert!(
            refs.len() >= 3,
            "expected at least 3 VarRefs for $x in fn body, got {}",
            refs.len()
        );
        for node in &refs {
            let coords = varref_resolution(node)
                .expect("resolver should have run")
                .expect("$x should be resolved to fn param scope");
            assert_eq!(coords.0, 0, "fn param is at level 0");
            assert_eq!(coords.1, 0, "x is the first (and only) fn param, slot 0");
        }
    }

    /// Match with multiple arms: type patterns (Int, String, etc.)
    #[test]
    fn match_dict_pattern_bindings() {
        let src = "[x: 1  result: [match $x Int: [+ $x 1] _: 0]]";
        let program = parse_and_resolve(src);
        let x_refs = find_varref_nodes(&program, "x");
        assert!(
            x_refs.len() >= 2,
            "expected at least 2 VarRefs for $x, got {}",
            x_refs.len()
        );
        for node in &x_refs {
            let coords = varref_resolution(node)
                .expect("resolver should have run")
                .expect("$x should be resolved (dict binding)");
            assert_eq!(coords.1, 0, "$x is first binding, slot 0");
        }
    }

    /// Match with wildcard pattern: $x in the wildcard arm body is unresolvable.
    #[test]
    fn match_wildcard_pattern_no_bindings() {
        let program = parse_and_resolve("[match val _: $x]");
        let refs = find_varref_nodes(&program, "x");
        assert!(
            !refs.is_empty(),
            "expected at least one VarRef for $x in wildcard arm body"
        );
        for node in &refs {
            // Resolver ran but found no binding → Some(None)
            assert_eq!(
                varref_resolution(node),
                Some(None),
                "wildcard binds nothing; $x in wildcard arm body must be Some(None)"
            );
        }
    }

    /// Sequential scope injection.
    #[test]
    fn sequential_scope_injection() {
        let program = parse_and_resolve("[a: 1]\n$a");
        let refs = find_varref_nodes(&program, "a");
        assert!(!refs.is_empty(), "expected VarRef for $a in second expr");
        let coords = varref_resolution(&refs[0])
            .expect("resolver should have run")
            .expect("$a should be resolved (key from prior expr in document)");
        assert_eq!(coords.1, 0, "a is first key from prior expr, slot 0");
    }

    /// B-375: `[case [let v] pattern body]` — the name `v` must be in scope in `body`.
    #[test]
    fn case_arm_let_bindings_in_scope() {
        let src = "[result: [match [Result.Ok 42]
            [case [let v] [Result.Ok v] $v]
            _: 0]]";
        let program = parse_and_resolve(src);
        let refs = find_varref_nodes(&program, "v");
        assert!(
            !refs.is_empty(),
            "expected at least one VarRef for $v in case arm body"
        );
        let resolved: Vec<(u32, u32)> = refs
            .iter()
            .filter_map(|n| varref_resolution(n).and_then(|r| r))
            .collect();
        assert!(
            !resolved.is_empty(),
            "$v in case arm body must be slot-resolved after B-375 fix"
        );
        for coords in &resolved {
            assert_eq!(
                coords.1, 0,
                "v is the only declared name in [let v], must be slot 0"
            );
        }
    }

    /// B-375: Multiple bindings in `[case [let a b] ...]` — each gets the correct slot.
    #[test]
    fn case_arm_let_bindings_multiple_slots() {
        let src = "[fn [let x] [match x
            [case [let a b] [Pair a b] [+ $a $b]]
            _: 0]]";
        let program = parse_and_resolve(src);
        let a_refs = find_varref_nodes(&program, "a");
        let b_refs = find_varref_nodes(&program, "b");
        let a_resolved: Vec<(u32, u32)> = a_refs
            .iter()
            .filter_map(|n| varref_resolution(n).and_then(|r| r))
            .collect();
        assert!(
            !a_resolved.is_empty(),
            "$a in case arm body must be slot-resolved"
        );
        for coords in &a_resolved {
            assert_eq!(coords.1, 0, "a is first name in [let a b], must be slot 0");
        }
        let b_resolved: Vec<(u32, u32)> = b_refs
            .iter()
            .filter_map(|n| varref_resolution(n).and_then(|r| r))
            .collect();
        assert!(
            !b_resolved.is_empty(),
            "$b in case arm body must be slot-resolved"
        );
        for coords in &b_resolved {
            assert_eq!(coords.1, 1, "b is second name in [let a b], must be slot 1");
        }
    }

    /// Collect all Arc<SurfaceNode> whose expr is DotAccess { expr: None, field: Ident(name) }.
    fn find_leading_dot_nodes(
        program: &crate::ast::SurfaceProgram,
        name: &str,
    ) -> Vec<Arc<SurfaceNode>> {
        let mut results = Vec::new();
        for doc_spanned in &program.documents {
            collect_leading_dots_in_doc(&doc_spanned.node, name, &mut results);
        }
        results
    }

    fn collect_leading_dots_in_doc(
        doc: &crate::ast::SurfaceDocument,
        name: &str,
        out: &mut Vec<Arc<SurfaceNode>>,
    ) {
        for item in &doc.items {
            if let crate::ast::SurfaceItem::Expr(node) = item {
                collect_leading_dots_in_node(node, name, out);
            }
        }
    }

    fn collect_leading_dots_in_node(
        arc: &Arc<SurfaceNode>,
        name: &str,
        out: &mut Vec<Arc<SurfaceNode>>,
    ) {
        match &arc.expr {
            SurfaceExpression::DotAccess {
                expr: None,
                field: crate::ast::DotKey::Ident(n),
                ..
            } if n == name => {
                out.push(Arc::clone(arc));
            }
            SurfaceExpression::Dict(entries) => {
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        collect_leading_dots_in_node(key, name, out);
                    }
                    collect_leading_dots_in_node(&entry.node.value, name, out);
                }
            }
            SurfaceExpression::DotAccess {
                expr: Some(inner), ..
            } => {
                collect_leading_dots_in_node(inner, name, out);
            }
            SurfaceExpression::Fn { body, .. } => {
                collect_leading_dots_in_node(body, name, out);
            }
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                collect_leading_dots_in_node(func, name, out);
                for arg in args {
                    collect_leading_dots_in_node(arg, name, out);
                }
                for na in named_args {
                    collect_leading_dots_in_node(&na.node.value, name, out);
                }
            }
            _ => {}
        }
    }

    /// Read the inline resolution from a leading-dot DotAccess node.
    fn dot_resolution(node: &Arc<SurfaceNode>) -> Option<Option<(u32, u32)>> {
        match &node.expr {
            SurfaceExpression::DotAccess {
                expr: None,
                resolution,
                ..
            } => resolution.get(),
            _ => None,
        }
    }

    /// Leading-dot resolves to the parent scope, skipping the innermost dict scope.
    #[test]
    fn leading_dot_parent_scope() {
        let src = "[x: 42  inner: [x: \"shadowed\"  outer-x: .x]]";
        let program = parse_and_resolve(src);
        let leading_dot_refs = find_leading_dot_nodes(&program, "x");
        assert!(
            !leading_dot_refs.is_empty(),
            "expected at least one leading-dot .x node"
        );
        for node in &leading_dot_refs {
            let coords = dot_resolution(node)
                .expect("resolver should have run")
                .expect("leading-dot .x should be resolved (parent scope)");
            assert_eq!(
                coords.1, 0,
                "x is the first key in the outer dict scope, must be slot 0"
            );
            assert_eq!(
                coords.0, 1,
                "outer dict is one level up from the inner dict (level 1, not 0)"
            );
        }
    }

    /// B-375: Names declared in `[let ...]` must NOT resolve in the outer scope.
    #[test]
    fn case_arm_let_bindings_not_resolved_as_varrefs() {
        let src = "[result: [match 42
            [case [let v] v $v]
            _: 0]]";
        let program = parse_and_resolve(src);
        let refs = find_varref_nodes(&program, "v");
        let resolved: Vec<(u32, u32)> = refs
            .iter()
            .filter_map(|n| varref_resolution(n).and_then(|r| r))
            .collect();
        assert!(
            !resolved.is_empty(),
            "$v in body must resolve after B-375 fix"
        );
    }
}
