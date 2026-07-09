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
    /// Each scope frame is an IndexMap<String, ()> where the slot index of a name
    /// is its position in the map: `scope.get_index_of(name) -> slot`.
    /// The explicit u32 value is dropped — position IS the slot.
    scopes: Vec<indexmap::IndexMap<String, ()>>,
    table: ResolutionTable,
    /// Unresolved VarRefs in expression position: (name, span).
    /// Populated only when suppress_depth == 0.  Positions that are NOT runtime
    /// variable references (annotations, static dict keys, LetDecl binding names,
    /// instance/class method-name keys, instance patterns) increment suppress_depth
    /// so they never contribute false positives.
    unresolved: Vec<(String, crate::ast::Span)>,
    /// > 0 when inside a context where unresolved VarRefs are not errors
    /// (annotation, static key, declaration position, etc.).
    suppress_depth: usize,
}

impl SurfaceResolver {
    fn new() -> Self {
        Self {
            scopes: Vec::new(),
            table: ResolutionTable::new(),
            unresolved: Vec::new(),
            suppress_depth: 0,
        }
    }

    fn enter_scope(&mut self, keys: &[String]) {
        let mut scope: indexmap::IndexMap<String, ()> =
            indexmap::IndexMap::with_capacity(keys.len());
        for key in keys {
            scope.insert(key.clone(), ());
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
            if let Some(slot) = scope.get_index_of(name) {
                let level = u32::try_from(offset).expect("scope depth overflow");
                let slot = u32::try_from(slot).expect("slot index overflow");
                return Some((level, slot));
            }
        }
        None
    }

    /// Resolve a class method name to ANY matching instance binding in scope.
    ///
    /// When `resolve_name` fails (method name not directly in scope), this searches
    /// all scope entries for instance bindings whose method component matches `name`.
    /// The instance binding name format is `ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷{method}⟨{args}⟩⧽`.
    ///
    /// Returns coordinates of the first matching binding. The type checker overrides
    /// via `call_dispatch` when it can determine the specific instance; this is the
    /// resolver's best-effort fallback so the OnceLock is set and the lowerer doesn't
    /// emit "undefined variable".
    fn method_to_instance(&self, name: &str) -> Option<(u32, u32)> {
        // Instance binding format: "ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷{method}⟨{args}⟩⧽" (with args)
        //                      or: "ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷{method}⧽"            (no args)
        // Match on "∷{name}⟨" or "∷{name}⧽" appearing in the binding name.
        let needle_with_args = format!("∷{}⟨", name);
        let needle_no_args = format!("∷{}⧽", name);
        for (offset, scope) in self.scopes.iter().rev().enumerate() {
            for (binding, _) in scope {
                if binding.starts_with('ɪ')
                    && (binding.contains(&needle_with_args) || binding.contains(&needle_no_args))
                {
                    if let Some(slot) = scope.get_index_of(binding) {
                        let level = u32::try_from(offset).expect("scope depth overflow");
                        let slot = u32::try_from(slot).expect("slot index overflow");
                        return Some((level, slot));
                    }
                }
            }
        }
        None
    }

    fn walk_surface_node(&mut self, arc: &Arc<SurfaceNode>) {
        self.walk_surface_expr(arc, &arc.expr);
    }

    fn walk_surface_expr(&mut self, arc: &Arc<SurfaceNode>, expr: &SurfaceExpression) {
        match expr {
            SurfaceExpression::VarRef {
                name, resolution, ..
            } => {
                if let Some(coords) = self.resolve_name(name) {
                    resolution.set(Some(coords));
                    self.table.insert(node_id(arc), coords);
                } else if let Some(coords) = self.method_to_instance(name) {
                    // Class method name not in direct scope — resolve to the first
                    // matching instance binding. The type checker overrides via
                    // call_dispatch when it can identify the specific instance.
                    resolution.set(Some(coords));
                    self.table.insert(node_id(arc), coords);
                } else if self.suppress_depth == 0 && name != "_" {
                    // Genuinely unresolved expression VarRef. Record for builtin-resolve
                    // error reporting. suppress_depth > 0 when in annotation, static
                    // dict-key, LetDecl binding, or declaration method-name position —
                    // none of those are runtime variable references.
                    self.unresolved.push((name.clone(), arc.span.clone()));
                }
                // OnceLock left unset (None): the lowerer treats None as
                // "undefined variable" and emits LowerDiagnostic::Error.
            }

            SurfaceExpression::Dict(entries) => {
                let static_keys = surface_dict_static_keys(entries);

                // Walk key expressions in outer scope.
                // Non-escaped VarRef keys (bare identifier keys like `x:`) are static
                // name declarations, not variable references — suppress error recording.
                // Escaped VarRef keys ($x:) ARE runtime lookups — report errors normally.
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        let is_static =
                            matches!(&key.expr, SurfaceExpression::VarRef { escaped: false, .. });
                        if is_static {
                            self.suppress_depth += 1;
                        }
                        self.walk_surface_node(key);
                        if is_static {
                            self.suppress_depth -= 1;
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

            SurfaceExpression::Field {
                expr,
                field,
                resolution,
                ..
            } => {
                if let Some(target) = expr {
                    self.walk_surface_node(target);
                    // Resolve field-get to get its de Bruijn level at the current scope depth.
                    // slot-get lives in the same root env and therefore has the same level.
                    // The lowerer reads this level and uses it with the hardcoded root slot
                    // constants (FIELD_GET_ROOT_SLOT / SLOT_GET_ROOT_SLOT), so only the level
                    // matters here — the slot from resolve_name is intentionally discarded.
                    // If field-get is not in scope (resolver not seeded with env), leave
                    // the OnceLock unset — the lowerer falls back to (MAX, MAX).
                    if let Some(coords) = self.resolve_name("field-get") {
                        let _ = resolution.set(Some(coords));
                    }
                } else if let crate::ast::DotKey::Ident(name) = field {
                    // Leading-dot `.name`: resolve the name in the current scope.
                    if let Some(coords) = self.resolve_name(name) {
                        let _ = resolution.set(Some(coords));
                    } else {
                        // Resolver ran but name not found — emit error node, not MAX/MAX.
                        let _ = resolution.set(None);
                    }
                }
                // Leading-dot with integer key (`.0`) is a parse error; no resolution needed.
            }

            // Pipe: walk both sides (the lowering pass will rewrite pipe to call)
            SurfaceExpression::Pipe { lhs, rhs } => {
                self.walk_surface_node(lhs);
                self.walk_surface_node(rhs);
            }

            SurfaceExpression::TypeAssert {
                annotation, expr, ..
            } => {
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
                    // Wildcard (`_`), literals, TypeTag, and Pin patterns bind nothing;
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
                // Binding names in [let x y] and [pattern x] are declarations, not
                // variable references. The lowerer extracts them as string literals via
                // lower_let_decl_binding(), never reading their OnceLocks as variables.
                self.suppress_depth += 1;
                for b in bindings {
                    self.walk_surface_node(b);
                }
                self.suppress_depth -= 1;
            }

            SurfaceExpression::CaseArm {
                let_bindings,
                pattern,
                body,
            } => {
                self.walk_surface_node(let_bindings);
                // Extract binding variable names from [let name1 name2 ...].
                // Enter scope BEFORE walking the pattern so that binding VarRefs
                // inside the pattern (e.g. `v` in `[Result.Ok v]`) resolve to
                // the case arm's own scope rather than leaving OnceLocks unset.
                let bound_names: Vec<String> = match &let_bindings.expr {
                    SurfaceExpression::LetDecl { bindings } => bindings
                        .iter()
                        .filter_map(|b| {
                            if let SurfaceExpression::VarRef { name, .. } = &b.expr {
                                Some(name.clone())
                            } else {
                                None
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                };
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

            // Declaration embedded as a dict entry value — walk class/instance bodies so
            // that runtime VarRefs inside method implementations (e.g. `result-map`
            // referenced in a named instance) get resolved against the enclosing letrec
            // scope.
            // TypeAlias bodies are type-level and must NOT be walked: names like `Null`,
            // `Int`, `Fn` etc. inside `[type ...]` are type names resolved by the type
            // checker, not runtime variables. Walking them triggers false "undefined
            // variable" diagnostics.
            SurfaceExpression::Decl(decl) => match decl.as_ref() {
                crate::ast::SurfaceDeclaration::ClassDecl { .. }
                | crate::ast::SurfaceDeclaration::InstanceDecl { .. } => {
                    self.walk_surface_declaration(decl);
                }
                _ => {}
            },

            // Terminals with no child expressions
            SurfaceExpression::Int(_)
            | SurfaceExpression::U64(_)
            | SurfaceExpression::Float(_)
            | SurfaceExpression::Str(_)
            | SurfaceExpression::Rest(..)
            | SurfaceExpression::Placeholder
            | SurfaceExpression::Error(_) => {}
        }
    }

    fn walk_surface_annotation(&mut self, ann: &Spanned<crate::ast::Annotation>) {
        // Annotation nodes contain type names (String, Int, type vars like `a`),
        // not runtime variable references. Suppress unresolved-error recording.
        self.suppress_depth += 1;
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
        self.suppress_depth -= 1;
    }

    fn walk_surface_declaration(&mut self, decl: &SurfaceDeclaration) {
        match decl {
            SurfaceDeclaration::TypeAlias { body, .. } => self.walk_surface_node(body),
            SurfaceDeclaration::ClassDecl { .. } => {
                // ClassDecl is entirely type-level: method signatures, determines, and the
                // resolver function name are all resolved by the type checker against the
                // type-stage env. The runtime resolver never touches them.
            }
            SurfaceDeclaration::InstanceDecl { arms, .. } => {
                for (pattern, methods) in arms {
                    // Instance patterns like `[let a@String b@String c]` are type-matching
                    // context, not runtime variable references.
                    self.suppress_depth += 1;
                    self.walk_surface_node(pattern);
                    self.suppress_depth -= 1;
                    for method in methods {
                        if let Some(key) = &method.node.key {
                            // Method implementation names are declarations, not references.
                            self.suppress_depth += 1;
                            self.walk_surface_node(key);
                            self.suppress_depth -= 1;
                        }
                        self.walk_surface_node(&method.node.value);
                    }
                }
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

    fn finish_with_errors(self) -> (ResolutionTable, Vec<(String, crate::ast::Span)>) {
        (self.table, self.unresolved)
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
/// Resolve a single SurfaceDocument in-place, writing de Bruijn coordinates directly to
/// the inline `Resolution` OnceLocks on each VarRef node.
///
/// The env chain is walked to populate resolver scopes from outermost to innermost.
/// Names not found in the env chain have their OnceLock left unset (None) and are
/// returned in the errors vec as `(name, span)` pairs. Only expression-position VarRefs
/// are reported — annotation type names, static dict keys, LetDecl binding names, and
/// class/instance method-name keys are suppressed (they are not runtime variable references).
///
/// The resolver MUST be seeded from the same env the document will be evaluated with.
/// A mismatch produces wrong de Bruijn levels, causing eval-time "undefined variable" errors.
///
/// Returns `(ResolutionTable, errors)`. The ResolutionTable can be discarded by most callers;
/// the errors are what `builtin-resolve` surfaces in its `errors:` result dict.
pub fn resolve_surface_document_inplace(
    doc: &crate::ast::SurfaceDocument,
    outer_env: &std::sync::Arc<std::sync::RwLock<crate::env::Env>>,
) -> (ResolutionTable, Vec<(String, crate::ast::Span)>) {
    let mut resolver = SurfaceResolver::new();

    // Collect env chain levels from outermost to innermost
    let mut env_levels: Vec<Vec<String>> = Vec::new();
    {
        let mut current = Some(std::sync::Arc::clone(outer_env));
        while let Some(env_rc) = current {
            let env = env_rc.read().unwrap();
            env_levels.push(env.slot_names());
            current = env.parent.as_ref().map(std::sync::Arc::clone);
        }
    }

    // Enter scopes from outermost (root) to innermost (doc-level).
    // This ensures level 0 = innermost = doc-level scope, matching eval-time env chain.
    for names in env_levels.iter().rev() {
        resolver.enter_scope(names);
    }

    resolver.walk_surface_document(doc);

    for _ in &env_levels {
        resolver.exit_scope();
    }

    resolver.finish_with_errors()
}

/// Resolve all VarRef nodes in a SurfaceProgram and return a ResolutionTable.
///
/// When `env` is provided, the resolver is pre-seeded from the env chain so that
/// builtin names (e.g. `builtin-mul`, prelude functions) resolve to
/// proper de Bruijn (level, slot) coordinates instead of leaving VarRef nodes
/// unresolved and falling back to name-based lookup at eval time.
///
/// When `env` is `None`, the resolver starts with an empty scope stack. Dict-internal
/// sibling references are still resolved by the resolver's scope-tracking logic; only
/// env-provided names (builtins, caps) remain unresolved.
///
/// Callers that have a runtime `Environment` available MUST pass `Some(&env)`. Callers
/// that genuinely have no env (tests, type-checker bootstrap paths) pass `None`.
pub fn resolve_surface_program(
    program: &SurfaceProgram,
    env: Option<&std::sync::Arc<std::sync::RwLock<crate::env::Env>>>,
) -> ResolutionTable {
    let mut resolver = SurfaceResolver::new();

    if let Some(env_arc) = env {
        // Seed the resolver from the env chain: collect scope frames from outermost to
        // innermost, matching the De Bruijn convention (level 0 = innermost scope).
        let mut env_levels: Vec<Vec<String>> = Vec::new();
        let mut current = Some(std::sync::Arc::clone(env_arc));
        while let Some(env_rc) = current {
            let env_guard = env_rc.read().unwrap();
            env_levels.push(env_guard.slot_names());
            current = env_guard.parent.as_ref().map(std::sync::Arc::clone);
        }
        // Enter scopes from outermost to innermost so that level 0 = innermost at resolve time.
        for names in env_levels.iter().rev() {
            resolver.enter_scope(names);
        }
        for doc_spanned in &program.documents {
            let doc = &doc_spanned.node;
            if doc.stage == Some(crate::ast::Stage::Type) {
                continue;
            }
            resolver.walk_surface_document(doc);
        }
        for _ in &env_levels {
            resolver.exit_scope();
        }
    } else {
        for doc_spanned in &program.documents {
            let doc = &doc_spanned.node;
            if doc.stage == Some(crate::ast::Stage::Type) {
                continue;
            }
            resolver.walk_surface_document(doc);
        }
    }

    resolver.finish()
}

/// Extract static string-keyed names from a SurfaceExpression::Dict's entries.
///
/// Handles two cases that the lowerer also handles, so the resolver's letrec scope matches
/// the evaluator's letrec environment exactly:
///
/// 1. Keyed entries — VarRef or string literal keys become static scope slots.
/// 2. Anonymous InstanceDecl entries (no outer key) — the lowerer flattens these into
///    ɪ-prefixed binding names (`ɪɴꜱᴛᴀɴᴄᴇ⧼Class∷method⟨T⟩⧽`) in the outer dict.
///    We register those same names here so `method_to_instance` can find them within
///    the same letrec scope (e.g., `[< y x]` inside `>` in the same dict as the
///    Comparable instance).
fn surface_dict_static_keys(entries: &[Spanned<SurfaceEntry>]) -> Vec<String> {
    let mut keys = Vec::new();
    for entry in entries {
        if let Some(key_node) = &entry.node.key {
            match &key_node.expr {
                SurfaceExpression::Str(s) => keys.push(s.clone()),
                // Non-escaped VarRef (bare identifier) → static name for letrec scope.
                // Escaped VarRef ($k:) is a computed key — not a static scope binding.
                SurfaceExpression::VarRef {
                    name,
                    escaped: false,
                    ..
                } => keys.push(name.clone()),
                _ => {}
            }
        } else if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
            // Anonymous entry (no outer key): check for InstanceDecl whose method
            // bindings the lowerer will flatten into the enclosing dict.
            if let crate::ast::SurfaceDeclaration::InstanceDecl { class_name, arms } = decl.as_ref()
            {
                for (pattern, method_entries) in arms {
                    let dispatch_tags = crate::lower::extract_dispatch_tags(&pattern.expr);
                    let type_args: Vec<&str> =
                        dispatch_tags.iter().filter_map(|t| t.as_deref()).collect();
                    for me in method_entries {
                        let method_name = match me.node.key.as_ref() {
                            Some(k) => match &k.expr {
                                SurfaceExpression::Str(s) => s.clone(),
                                SurfaceExpression::VarRef { name, .. } => name.clone(),
                                _ => continue,
                            },
                            None => continue,
                        };
                        keys.push(crate::type_def::instance_binding_name(
                            class_name,
                            &method_name,
                            &type_args,
                        ));
                    }
                }
            }
        }
    }
    keys
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
        Pattern::Pin(..) => {
            // Pin patterns (bare names and $name) match against scope values, don't bind
        }
        Pattern::Literal(_) => {
            // Literal patterns bind no variables
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
        // TypeAssertPending/TypeAssert/Predicate patterns bind no variables directly
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
        Pattern::Predicate { .. } => {}
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
        // No runtime env in unit tests — dict-internal and lexical references still
        // resolve via the resolver's scope tracking; env-provided names (builtins) use None.
        let table = resolve_surface_program(&program, None);
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
            SurfaceExpression::Field {
                expr: Some(expr), ..
            } => {
                collect_varrefs_in_node(expr, name, out);
                // (Field with expr: None is a leading-dot form — no sub-expression to recurse into)
            }
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
                // Walk into all three sub-nodes so VarRef searches reach the body.
                // Note: let_bindings VarRefs are declarations (not references), but
                // walking them here is safe — they'll have no entry in the resolution
                // table (OnceLock unset for declaration-position names).
                collect_varrefs_in_node(let_bindings, name, out);
                collect_varrefs_in_node(pattern, name, out);
                collect_varrefs_in_node(body, name, out);
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
    /// Uses [case [let n] _ $n] form: [let n] declares the binding, _ matches anything,
    /// and $n in the body resolves to the case arm's scope (level=0, slot=0).
    #[test]
    fn match_arm_pattern_binding() {
        // T-1154: bare lowercase names in match arm patterns are now Pin (not Variable).
        // To bind a variable in a match arm, use [case [let n] pattern body] form.
        // [case [let n] _  $n] — n is declared by [let n], _ matches anything, $n resolves.
        let (program, table) = parse_and_resolve("[match 42 [case [let n] _ $n]]");
        let refs = find_varref_nodes(&program, "n");
        assert!(!refs.is_empty(), "expected VarRef for $n in case arm body");
        let (id, _) = &refs[0];
        let coords = table
            .get(id)
            .expect("$n should be resolved (case arm binding in arm scope)");
        assert_eq!(coords.0, 0, "pattern binding should be at level 0");
        assert_eq!(coords.1, 0, "n is the first (and only) binding");
    }

    /// Case arm bodies see the bindings declared in [let ...].
    /// T-1154: bare lowercase names in match arm patterns are now Pin (not Variable).
    /// To bind a variable, use [case [let n] pattern body] form.
    #[test]
    fn match_arm_guard_sees_pattern_bindings() {
        // T-1154: `n:` in match arm position creates a Pin pattern, not a variable binding.
        // Pins in pattern position resolve against the outer scope — n is NOT introduced into
        // the arm body scope. Use [case [let n] _ body] to actually bind n.
        let src = "[match 42 [case [let n] _ [+ $n 1]]]";
        let (program, table) = parse_and_resolve(src);
        let refs = find_varref_nodes(&program, "n");
        // $n in the body should resolve (introduced by [let n] in the case arm)
        assert_eq!(
            refs.len(),
            1,
            "expected exactly 1 VarRef for $n (body reference)"
        );
        for (id, _) in &refs {
            let coords = table
                .get(id)
                .expect("$n should be resolved via case arm [let n]");
            // The case arm scope introduces n as slot 0
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
