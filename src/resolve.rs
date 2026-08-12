//! Variable resolution pass: assigns VarAddr addresses to VarRef nodes.
//!
//! The resolver walks the AST and assigns VarAddr values to static variable references
//! before evaluation begins:
//!
//! - `Dispatch(depth, slot)` — per-frame relative slot lookup.
//!   `depth` is the distance from the top of the scope stack (0 = innermost frame).
//!   `slot` is the entry index within that frame (0-based per frame).
//! - `EffectPerform { class_decl_id, method_slot }` — typeclass method call;
//!   resolved at runtime via the evidence map on the caller's SymbolTable.
//!
//! **Invariants:**
//! - Must run exactly once per AST (write-once OnceLock).
//! - Must run after desugaring (sees $_ as Fn nodes, not VarRef("_")).
//! - Must run before typechecking and evaluation (both consumers of resolved coords).

use crate::ast::{
    node_id, DotKey, ResolutionTable, Spanned, SurfaceDeclaration, SurfaceDocument, SurfaceEntry,
    SurfaceExpression, SurfaceItem, SurfaceNode, SurfaceProgram, VarAddr,
};
use crate::error::Diagnostic;
use crate::symbol_table::SymbolTable;
use std::sync::Arc;

/// Sentinel slot value stored in SymbolTable entries that represent EffectPerform
/// typeclass method bindings. When `resolve_name` encounters an entry with this
/// slot, it consults `effect_perform_addrs` for the actual VarAddr.
const EFFECT_SENTINEL: u32 = u32::MAX;

/// Result of variable resolution.
pub struct ResolveResult {
    pub table: ResolutionTable,
    pub diagnostics: Vec<Diagnostic>,
    pub unreferenced: Vec<String>,
}

/// Extract binding variable names from a `[let name1 name2 ...]` node.
/// Excludes `_` (wildcard) from the result — `_` is not a binding.
fn extract_case_arm_binding_names(let_bindings: &SurfaceNode) -> Vec<String> {
    match &let_bindings.expr {
        SurfaceExpression::LetDecl { bindings } => bindings
            .iter()
            .filter_map(|b| {
                if let SurfaceExpression::VarRef { name, .. } = &b.expr {
                    if name == "_" {
                        None // wildcard, not a binding
                    } else {
                        Some(name.clone())
                    }
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

struct SurfaceResolver {
    /// Scope stack: index 0 is the outermost scope, last element is the innermost.
    scope_stack: Vec<Arc<SymbolTable>>,
    table: ResolutionTable,
    diagnostics: Vec<Diagnostic>,
    /// Maps method_name -> EffectPerform VarAddr for typeclass method calls.
    /// Entries are inserted when class/instance pairs are discovered within a dict.
    /// The corresponding SymbolTable entry uses EFFECT_SENTINEL as its slot.
    effect_perform_addrs: std::collections::HashMap<String, VarAddr>,
    /// The initial (outermost) scope passed by the caller, used for unreferenced tracking.
    initial_scope: Option<Arc<SymbolTable>>,
    /// Names from the initial scope that were referenced during resolution.
    referenced_initial_names: std::collections::HashSet<String>,
}

impl SurfaceResolver {
    fn new(initial_scope: &Arc<SymbolTable>) -> Self {
        Self {
            scope_stack: vec![Arc::clone(initial_scope)],
            table: ResolutionTable::new(),
            diagnostics: Vec::new(),
            effect_perform_addrs: std::collections::HashMap::new(),
            initial_scope: Some(Arc::clone(initial_scope)),
            referenced_initial_names: std::collections::HashSet::new(),
        }
    }

    /// Enter a scope with contiguous 0-based slots for the given names.
    /// Returns the Arc<SymbolTable> for setting on AST nodes.
    fn enter_scope(&mut self, names: &[String]) -> Arc<SymbolTable> {
        let mut st = SymbolTable::new();
        for (i, name) in names.iter().enumerate() {
            st.insert(name.clone(), i as u32, None);
        }
        let arc = Arc::new(st);
        self.scope_stack.push(Arc::clone(&arc));
        arc
    }

    /// Enter a scope with pre-computed (name, slot) pairs.
    /// Slots must be contiguous 0-based to match IndexMap insertion order.
    fn enter_scope_with_slots(&mut self, pairs: &[(String, u32)]) -> Arc<SymbolTable> {
        let mut st = SymbolTable::new();
        for (name, slot) in pairs {
            st.insert(name.clone(), *slot, None);
        }
        let arc = Arc::new(st);
        self.scope_stack.push(Arc::clone(&arc));
        arc
    }

    fn exit_scope(&mut self) {
        self.scope_stack.pop().expect("scope stack underflow");
    }

    /// Resolve `name` in the scope stack, returning its `VarAddr`.
    ///
    /// All variable addressing uses exactly two variants:
    /// - `Dispatch(depth, slot)` — per-frame relative slot lookup.
    ///   `depth` is the distance from the top of the scope stack (0 = innermost).
    ///   `slot` is the entry index within that frame.
    /// - `EffectPerform { class_decl_id, method_slot }` — typeclass method call;
    ///   resolved at runtime via the evidence map.
    fn resolve_name(&mut self, name: &str) -> Option<VarAddr> {
        if self.scope_stack.is_empty() {
            return None;
        }
        let initial_ptr = self.initial_scope.as_ref().map(|s| Arc::as_ptr(s));
        let mut result: Option<(u32, u32, bool, bool)> = None; // depth, slot, is_effect, is_initial

        for (depth, frame) in self.scope_stack.iter().rev().enumerate() {
            if let Some(entry) = frame.entries().get(name) {
                let is_initial =
                    initial_ptr.map_or(false, |ip| std::ptr::eq(Arc::as_ptr(frame), ip));
                let is_effect = entry.slot == EFFECT_SENTINEL;
                if !is_effect {
                    entry
                        .referenced
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                result = Some((depth as u32, entry.slot, is_effect, is_initial));
                break;
            }
        }

        let (depth, slot, is_effect, is_initial) = result?;
        if is_initial {
            self.referenced_initial_names.insert(name.to_string());
        }
        if is_effect {
            return self.effect_perform_addrs.get(name).cloned();
        }
        Some(VarAddr::Dispatch(depth, slot))
    }

    /// Resolve `name` using leading-dot semantics: find the **second** occurrence of `name`
    /// in the scope stack, skipping the innermost (first) binding.
    ///
    /// `.name` means "the next `name` beyond the nearest one." This is relative to where
    /// `name` is actually bound, not to the structure of Dict scope boundaries:
    ///
    /// - Find the first binding of `name` anywhere in the scope stack (innermost wins).
    /// - Skip it and search outward for the next binding of `name`.
    /// - Return `None` if there is no second binding — caller emits "undefined variable".
    fn resolve_name_parent(&mut self, name: &str) -> Option<VarAddr> {
        if self.scope_stack.is_empty() {
            return None;
        }
        let initial_ptr = self.initial_scope.as_ref().map(|s| Arc::as_ptr(s));
        let mut found_first = false;
        let mut result: Option<(u32, u32, bool, bool)> = None;

        for (depth, frame) in self.scope_stack.iter().rev().enumerate() {
            if frame.entries().contains_key(name) {
                if !found_first {
                    found_first = true;
                } else {
                    let entry = frame.entries().get(name).unwrap();
                    let is_initial =
                        initial_ptr.map_or(false, |ip| std::ptr::eq(Arc::as_ptr(frame), ip));
                    let is_effect = entry.slot == EFFECT_SENTINEL;
                    if !is_effect {
                        entry
                            .referenced
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    result = Some((depth as u32, entry.slot, is_effect, is_initial));
                    break;
                }
            }
        }

        let (depth, slot, is_effect, is_initial) = result?;
        if is_initial {
            self.referenced_initial_names.insert(name.to_string());
        }
        if is_effect {
            return self.effect_perform_addrs.get(name).cloned();
        }
        Some(VarAddr::Dispatch(depth, slot))
    }

    fn walk_surface_node(&mut self, arc: &Arc<SurfaceNode>) {
        self.walk_surface_expr(arc, &arc.expr);
    }

    fn walk_surface_expr(&mut self, arc: &Arc<SurfaceNode>, expr: &SurfaceExpression) {
        match expr {
            SurfaceExpression::VarRef {
                name, resolution, ..
            } => {
                if let Some(addr) = self.resolve_name(name) {
                    resolution.set(Some(addr.clone()));
                    self.table.insert(node_id(arc), addr);
                } else {
                    resolution.set(None);
                    self.diagnostics.push(Diagnostic::error(
                        "resolve-error",
                        format!("undefined variable: {}", name),
                        arc.span.clone(),
                    ));
                }
            }

            SurfaceExpression::Dict { entries, scope: _ } => {
                // Walk key expressions in outer scope.
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        self.walk_surface_node(key);
                    }
                }

                // Phase 1b: assign class_ids to all ClassDecl nodes (including nested ones
                // inside namespace dicts like `File: [Readable: [class ...]]`).
                for entry in entries.iter() {
                    if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                        if let crate::ast::SurfaceDeclaration::ClassDecl {
                            pre_assigned_class_decl_id,
                            ..
                        } = decl.as_ref()
                        {
                            if pre_assigned_class_decl_id.get().is_none() {
                                pre_assigned_class_decl_id
                                    .set(crate::type_class::next_class_decl_id());
                            }
                        }
                    } else if let SurfaceExpression::Dict {
                        entries: inner_entries,
                        ..
                    } = &entry.node.value.expr
                    {
                        for inner_entry in inner_entries.iter() {
                            if let SurfaceExpression::Decl(inner_decl) =
                                &inner_entry.node.value.expr
                            {
                                if let crate::ast::SurfaceDeclaration::ClassDecl {
                                    pre_assigned_class_decl_id,
                                    ..
                                } = inner_decl.as_ref()
                                {
                                    if pre_assigned_class_decl_id.get().is_none() {
                                        pre_assigned_class_decl_id
                                            .set(crate::type_class::next_class_decl_id());
                                    }
                                }
                            }
                        }
                    }
                }

                // Build the SymbolTable for this dict's letrec scope.
                // Slots are per-frame relative (0-based). Only named entries get scope slots.
                let mut st = SymbolTable::new();
                let mut scope_slot = 0u32;
                let mut overall_slot = 0u32;
                for entry in entries.iter() {
                    if let Some(key) = &entry.node.key {
                        match &key.expr {
                            SurfaceExpression::StringLiteral { content, .. } => {
                                st.insert(content.clone(), scope_slot, None);
                                scope_slot += 1;
                            }
                            SurfaceExpression::VarRef {
                                name,
                                escaped: false,
                                ..
                            } => {
                                st.insert(name.clone(), scope_slot, None);
                                scope_slot += 1;
                            }
                            _ => {}
                        }
                    }
                    // First write wins: shared AST nodes may be visited multiple times.
                    let _ = entry.node.assigned_slot.set(overall_slot);
                    overall_slot += 1;
                    // InstanceDecl method slots: advance past method bindings.
                    if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                        if let crate::ast::SurfaceDeclaration::InstanceDecl { arms, .. } =
                            decl.as_ref()
                        {
                            for (_pattern, method_entries) in arms {
                                for me in method_entries {
                                    let method_name =
                                        me.node.key.as_ref().and_then(|k| match &k.expr {
                                            SurfaceExpression::StringLiteral {
                                                content, ..
                                            } => Some(content.clone()),
                                            SurfaceExpression::VarRef { name, .. } => {
                                                Some(name.clone())
                                            }
                                            _ => None,
                                        });
                                    if let Some(name) = method_name {
                                        if !st.entries().contains_key(&name) {
                                            st.insert(name.clone(), scope_slot, None);
                                            scope_slot += 1;
                                        }
                                        let _ = me.node.assigned_slot.set(overall_slot);
                                        overall_slot += 1;
                                    }
                                }
                            }
                        }
                    }
                }

                // Phase 2: inject EffectPerform for typeclass methods.
                // Scan InstanceDecl entries, resolve each instance's class_name to find
                // the ClassDecl, and inject the class's method names as EffectPerform sentinels.
                for entry in entries.iter() {
                    if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                        if let crate::ast::SurfaceDeclaration::InstanceDecl { class_name, .. } =
                            decl.as_ref()
                        {
                            if let Some((class_id, methods)) =
                                find_class_decl_in_entries(class_name, entries)
                            {
                                for (method_slot, method_name) in methods.iter().enumerate() {
                                    let should_inject = st
                                        .entries()
                                        .get(method_name)
                                        .map_or(true, |e| e.slot == EFFECT_SENTINEL);
                                    if should_inject {
                                        st.insert(method_name.clone(), EFFECT_SENTINEL, None);
                                        self.effect_perform_addrs.insert(
                                            method_name.clone(),
                                            VarAddr::EffectPerform {
                                                class_decl_id: class_id,
                                                method_slot: method_slot as u32,
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                // Arc the SymbolTable and push onto scope stack + set on AST node.
                let st_arc = Arc::new(st);
                if let SurfaceExpression::Dict { scope, .. } = &arc.expr {
                    let _ = scope.set(Arc::clone(&st_arc));
                }
                self.scope_stack.push(st_arc);

                // Walk key annotations INSIDE the letrec scope so that annotation
                // values can reference the dict's own entries (forward letrec refs).
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        if let SurfaceExpression::VarRef {
                            annotation: Some(ann),
                            ..
                        } = &key.expr
                        {
                            self.walk_surface_annotation(ann);
                        }
                    }
                }
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
                for param in params {
                    if let Some(ann) = &param.node.annotation {
                        self.walk_surface_annotation(ann);
                    }
                }

                let param_names: Vec<String> = params.iter().map(|p| p.node.name.clone()).collect();
                // Set resolved dispatch slots on param nodes for lowerer use.
                // Slots are per-frame relative (0-based).
                for (i, p) in params.iter().enumerate() {
                    p.node.resolved_dispatch_slot.set(i as u32);
                }
                let scope_arc = self.enter_scope(&param_names);
                // Set SymbolTable on the Fn AST node.
                if let SurfaceExpression::Fn { scope, .. } = &arc.expr {
                    let _ = scope.set(scope_arc);
                }

                self.walk_surface_node(body);

                self.exit_scope();
            }

            SurfaceExpression::Sequential(exprs) => {
                let mut injected = 0usize;
                for (i, e) in exprs.iter().enumerate() {
                    let is_last = i == exprs.len() - 1;

                    if !is_last {
                        // Intermediate body: walk it as a dict node (which creates its own
                        // letrec scope via the Dict arm), then inject a sequential scope
                        // for subsequent bodies.
                        self.walk_surface_node(e);
                        // After walking, read the scope from the Dict AST node (if it was
                        // a dict) and create a sequential scope with the same named entries.
                        if let SurfaceExpression::Dict { scope, .. } = &e.expr {
                            if let Some(dict_st) = scope.get() {
                                let pairs: Vec<(String, u32)> = dict_st
                                    .entries()
                                    .iter()
                                    .filter(|(_, e)| e.slot != EFFECT_SENTINEL)
                                    .map(|(name, entry)| (name.clone(), entry.slot))
                                    .collect();
                                if !pairs.is_empty() {
                                    self.enter_scope_with_slots(&pairs);
                                    injected += 1;
                                }
                            }
                        }
                    } else {
                        // Final expression: walk normally.
                        self.walk_surface_node(e);
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
                } else if let crate::ast::DotKey::Ident(name) = field {
                    // Leading-dot `.name`: look up in the PARENT scope, skipping the
                    // nearest enclosing letrec dict scope. This prevents `[k: .k ...]`
                    // from creating a circular self-reference.
                    // `.a.b.c` chains work correctly: only this innermost `expr: None`
                    // case uses parent lookup; outer `.b` / `.c` lower to CoreExpr::Field
                    // (direct structural access — no resolver involvement needed).
                    if let Some(coords) = self.resolve_name_parent(name) {
                        resolution.set(Some(coords.clone()));
                        self.table.insert(node_id(arc), coords);
                    } else {
                        // Resolver ran but name not found in parent — emit error node.
                        resolution.set(None);
                    }
                }
                // Leading-dot with integer key (`.0`) is a parse error; no resolution needed.
            }

            // Pipe: walk both sides (the lowering pass will rewrite pipe to call)
            SurfaceExpression::Pipe { lhs, rhs, .. } => {
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
            SurfaceExpression::Quote(_inner) => {}

            SurfaceExpression::Unquote(inner) | SurfaceExpression::UnquoteSplice(inner) => {
                self.walk_surface_node(inner);
            }

            SurfaceExpression::Match { scrutinee, arms } => {
                self.walk_surface_node(scrutinee);
                for arm in arms {
                    let let_bindings = &arm.let_bindings;
                    self.walk_surface_node(let_bindings);
                    let bound_names = extract_case_arm_binding_names(let_bindings);
                    let has_bindings = !bound_names.is_empty();

                    if has_bindings {
                        let scope_arc = self.enter_scope(&bound_names);
                        // Set SymbolTable on the MatchArm.
                        let _ = arm.scope.set(scope_arc);
                    }

                    self.walk_surface_node(&arm.pattern);

                    if let Some(guard) = &arm.guard {
                        self.walk_surface_node(guard);
                    }
                    for body_expr in &arm.body {
                        self.walk_surface_node(body_expr);
                    }
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
                for b in bindings {
                    self.walk_surface_node(b);
                }
            }

            // Declaration embedded as a dict entry value — walk class/instance bodies so
            // that runtime VarRefs inside method implementations (e.g. `result-map`
            // referenced in a named instance) get resolved against the enclosing letrec
            // scope. For TypeAlias, walk only constructor annotation values (runtime
            // expressions like [fn [let t] t]) — NOT the type structure itself (constructor
            // names, field types, type parameters), which are type-level, not runtime.
            SurfaceExpression::Decl(decl) => match decl.as_ref() {
                crate::ast::SurfaceDeclaration::ClassDecl { .. }
                | crate::ast::SurfaceDeclaration::InstanceDecl { .. } => {
                    self.walk_surface_declaration(decl);
                }
                crate::ast::SurfaceDeclaration::TypeAlias { body, .. } => {
                    self.walk_type_alias_body(body);
                }
                _ => {}
            },

            SurfaceExpression::UnitVariant { .. } => {} // no VarRefs to resolve
            SurfaceExpression::MakeVariant { payload, .. } => {
                self.walk_surface_node(payload);
            }
            SurfaceExpression::TypeDecl { inner, .. } => {
                self.walk_surface_node(inner);
            }
            SurfaceExpression::ReprDecl { inner, is_pred, .. } => {
                if let Some(is_pred_node) = is_pred {
                    self.walk_surface_node(is_pred_node);
                }
                self.walk_surface_node(inner);
            }

            // Terminals with no child expressions
            SurfaceExpression::Int(_)
            | SurfaceExpression::U64(_)
            | SurfaceExpression::Float(_)
            | SurfaceExpression::StringLiteral { .. }
            | SurfaceExpression::Error(_) => {}

            // Placeholder (...) is a genuine TODO marker — no special handling needed.
            SurfaceExpression::Placeholder(..) => {}
        }
    }

    /// Walk the body of a [type ...] declaration, resolving ONLY the runtime values
    /// stored in constructor PropertyDict annotations (e.g. `@[as-type: [fn [let t] t]]`).
    ///
    /// TypeAlias bodies contain two kinds of sub-expressions:
    ///   - Type structure: constructor name VarRefs (Red, Green), field type expressions
    ///     ([Map String TypeNode]), type parameters (`a` in `value: a`) — ALL type-level.
    ///   - Annotation values: PropertyDict entries on constructor VarRefs/funcs — runtime
    ///     expressions stored via builtin-make-annotated, returned by annotation-of at runtime.
    ///
    /// Walk the body of a TypeAlias declaration, resolving VarRefs inside constructor
    /// annotations. After expansion, TypeAlias declarations are replaced with TypeDecl
    /// nodes, so this function is only needed for pre-expand ASTs (e.g., type-stage
    /// documents or paths where the expander has not yet run).
    fn walk_type_alias_body(&mut self, body: &Arc<SurfaceNode>) {
        match &body.expr {
            SurfaceExpression::Dict { entries, scope: _ } => {
                // Multi-constructor union. Each entry is one constructor:
                //   - Named constructor: `Constructor@[annotation]: [fields]`
                //     The KEY holds the constructor name and annotation; the VALUE is
                //     the field type dict (type-level, not walked at runtime).
                //   - Positional (unit) constructor: `Constructor@[annotation]`
                //     (no key) — the VALUE is the constructor node itself.
                // Walk only the node that carries the constructor name and annotation.
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        // Named constructor: annotation is on the key.
                        self.walk_ctor_node(key);
                    } else {
                        // Positional constructor: annotation is on the value.
                        self.walk_ctor_node(&entry.node.value);
                    }
                }
            }
            _ => {
                // Single constructor body.
                self.walk_ctor_node(body);
            }
        }
    }

    /// Walk the PropertyDict annotation values of a single constructor node.
    fn walk_ctor_node(&mut self, node: &Arc<SurfaceNode>) {
        match &node.expr {
            // Annotated VarRef: `Red@[as-type: [fn [let t] t]]` (bare unit constructor)
            SurfaceExpression::VarRef {
                name,
                annotation: Some(ann),
                ..
            } if crate::eval::is_constructor_name(name) => {
                self.walk_surface_annotation(ann);
            }
            SurfaceExpression::VarRef {
                name,
                annotation: None,
                ..
            } if crate::eval::is_constructor_name(name) => {}
            // Bracket form: `[Dict@[as-type: ...] fields: ...]` or `[Int@[...]]`
            // The parser wraps annotated constructors in a bracket (a single-entry Dict).
            // Recurse through it to find the actual VarRef or Call inside.
            SurfaceExpression::Dict { entries, scope: _ } => {
                for entry in entries {
                    self.walk_ctor_node(&entry.node.value);
                }
            }
            // Call with annotated func: `Call { func: VarRef("Dict")@[...], named_args: [...] }`
            SurfaceExpression::Call { func, .. } => {
                if let SurfaceExpression::VarRef {
                    name, annotation, ..
                } = &func.expr
                {
                    if crate::eval::is_constructor_name(name) {
                        if let Some(ann) = annotation {
                            self.walk_surface_annotation(ann);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn walk_surface_annotation(&mut self, ann: &Spanned<crate::ast::Annotation>) {
        match &ann.node {
            crate::ast::Annotation::Simple(_) => {}
            crate::ast::Annotation::Quote => {}
            crate::ast::Annotation::PropertyDict(entries) => {
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        self.walk_surface_node(key);
                    }
                    self.walk_surface_node(&entry.node.value);
                }
            }
            crate::ast::Annotation::Annotated(outer, inner) => {
                let outer_spanned = Spanned::new(outer.as_ref().clone(), ann.span.clone());
                self.walk_surface_annotation(&outer_spanned);
                let inner_spanned = Spanned::new(inner.as_ref().clone(), ann.span.clone());
                self.walk_surface_annotation(&inner_spanned);
            }
        }
    }

    fn walk_surface_declaration(&mut self, decl: &SurfaceDeclaration) {
        match decl {
            // After expansion, TypeAlias declarations are replaced with TypeDecl nodes.
            // This arm is only reached for pre-expand ASTs (type-stage documents or
            // paths where the expander has not yet run).
            SurfaceDeclaration::TypeAlias { body, .. } => self.walk_type_alias_body(body),
            SurfaceDeclaration::ClassDecl { .. } => {
                // ClassDecl is entirely type-level: method signatures, determines, and the
                // resolver function name are all resolved by the type checker against the
                // type-stage env. The runtime resolver never touches them.
            }
            SurfaceDeclaration::InstanceDecl { arms, .. } => {
                for (pattern, methods) in arms {
                    // Instance patterns like `[let a@String b@String c]` are type-matching
                    // context, not runtime variable references.
                    self.walk_surface_node(pattern);
                    for method in methods {
                        if let Some(key) = &method.node.key {
                            // Method implementation names are declarations, not references.
                            self.walk_surface_node(key);
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

    /// Walk a document, pushing per-frame 0-based scopes onto the scope stack for each dict.
    ///
    /// Each intermediate dict expression's named entries become scope bindings
    /// for subsequent expressions within the same document (sequential scope injection).
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
                        // After walking, read the scope from the Dict AST node and
                        // create a sequential scope with the same named entries.
                        if let SurfaceExpression::Dict { scope, .. } = &node.expr {
                            if let Some(dict_st) = scope.get() {
                                let pairs: Vec<(String, u32)> = dict_st
                                    .entries()
                                    .iter()
                                    .filter(|(_, e)| e.slot != EFFECT_SENTINEL)
                                    .map(|(name, entry)| (name.clone(), entry.slot))
                                    .collect();
                                if !pairs.is_empty() {
                                    self.enter_scope_with_slots(&pairs);
                                    injected += 1;
                                }
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

        // Exit the injected scopes.
        for _ in 0..injected {
            self.exit_scope();
        }
    }

    fn finish(self) -> ResolveResult {
        let initial_scope = self.initial_scope.as_ref();
        let unreferenced: Vec<String> = match initial_scope {
            Some(scope) => scope
                .entries()
                .keys()
                .filter(|name| !self.referenced_initial_names.contains(*name))
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        ResolveResult {
            table: self.table,
            diagnostics: self.diagnostics,
            unreferenced,
        }
    }
}

/// Resolve all VarRef nodes in a SurfaceDocument.
///
/// `scope` is the initial scope containing all pre-existing bindings (builtins,
/// capabilities, env-dict names, etc.). The resolver creates new SymbolTable frames
/// on top of this scope for each dict, fn, and match arm encountered during the walk.
///
/// Returns a `ResolveResult` containing:
/// - `table`: resolution table mapping node IDs to VarAddr values.
/// - `diagnostics`: resolve errors (undefined variables).
/// - `unreferenced`: names from `scope` that were never referenced.
pub fn resolve_document(doc: &SurfaceDocument, scope: &Arc<SymbolTable>) -> ResolveResult {
    let mut resolver = SurfaceResolver::new(scope);
    resolver.walk_surface_document(doc);
    resolver.finish()
}

/// Resolve all VarRef nodes in a SurfaceProgram.
///
/// `scope` is the initial scope containing all pre-existing bindings.
/// Each document in the program is walked sequentially.
///
/// Returns a `ResolveResult` containing:
/// - `table`: resolution table mapping node IDs to VarAddr values.
/// - `diagnostics`: resolve errors (undefined variables).
/// - `unreferenced`: names from `scope` that were never referenced.
pub fn resolve_program(program: &SurfaceProgram, scope: &Arc<SymbolTable>) -> ResolveResult {
    let mut resolver = SurfaceResolver::new(scope);
    for doc_spanned in &program.documents {
        resolver.walk_surface_document(&doc_spanned.node);
    }
    resolver.finish()
}

/// Extract the static produced keys of a document's final expression.
///
/// For a document whose last expression is a Dict (`[k1: v1  k2: v2]`), returns
/// the list of static string-keyed names that this stage "produces" (makes available
/// to the next pipeline document via `%`). Returns an empty Vec if the final expression
/// is not a Dict or has no static keys.
///
/// Also returns the span of the final expression for use in pipeline lint warnings.
pub fn collect_document_produced_keys(
    doc: &crate::ast::SurfaceDocument,
    doc_span: &crate::ast::Span,
) -> (Vec<String>, crate::ast::Span) {
    // Find the last Expr item.
    let last_expr = doc.items.iter().rev().find_map(|item| match item {
        SurfaceItem::Expr(node) => Some(node),
        SurfaceItem::Decl(_) => None,
    });
    let Some(expr_node) = last_expr else {
        return (Vec::new(), doc_span.clone());
    };
    let span = expr_node.span.clone();
    match &expr_node.expr {
        SurfaceExpression::Dict { entries, scope: _ } => (surface_dict_static_keys(entries), span),
        _ => (Vec::new(), span),
    }
}

/// Cross-document pipeline lint.
///
/// After resolving all documents in a pipeline, checks whether keys produced by
/// non-final stages are consumed by the subsequent document. Keys produced but
/// not consumed generate `abandoned-output` warnings.
///
/// If the subsequent document uses dynamic access to the pipeline variable (e.g. `[get key var]`
/// with a variable key), the warning is suppressed for that stage because the
/// accessed keys cannot be statically determined.
///
/// `stages` is a slice of `(produced_keys, var_field_accesses, uses_dynamic_var)`:
/// - `produced_keys`: static key names from the stage's final expression.
/// - `var_field_accesses`: static `var.key` field access names from the stage's document.
/// - `uses_dynamic_var`: whether the stage uses the pipeline variable in a non-field context.
///
/// Returns a vec of `Diagnostic` warnings for abandoned outputs.
pub fn lint_pipeline_stages(
    stages: &[(
        Vec<String>,      // produced keys
        Vec<String>,      // field accesses on the pipeline variable from next doc
        bool,             // next doc uses pipeline variable in non-field (dynamic) context
        crate::ast::Span, // span of the producing document's final expression
    )],
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (produced_keys, consumed_keys, uses_dynamic, span) in stages {
        if *uses_dynamic {
            // Dynamic access to % — cannot statically determine consumed keys; suppress.
            continue;
        }
        let consumed_set: std::collections::HashSet<&str> =
            consumed_keys.iter().map(|s| s.as_str()).collect();
        for key in produced_keys {
            if !consumed_set.contains(key.as_str()) {
                diagnostics.push(Diagnostic::warn(
                    "abandoned-output",
                    format!(
                        "key '{}' is produced but never consumed by the next pipeline stage",
                        key
                    ),
                    span.clone(),
                ));
            }
        }
    }
    diagnostics
}

/// Collect all static `var.key` field accesses on the named variable from a document.
///
/// Walks the document's AST and returns the set of string field names accessed
/// on the named variable (e.g. the pipeline input). Also returns whether the
/// variable is used in a non-field context (e.g., passed to a function, used as
/// a match scrutinee), which indicates dynamic access that prevents static key
/// analysis.
pub fn collect_var_accesses(
    doc: &crate::ast::SurfaceDocument,
    var_name: &str,
) -> (Vec<String>, bool) {
    let mut field_accesses = Vec::new();
    let mut dynamic_use = false;
    for item in &doc.items {
        if let SurfaceItem::Expr(node) = item {
            collect_var_accesses_node(node, var_name, &mut field_accesses, &mut dynamic_use);
        }
    }
    (field_accesses, dynamic_use)
}

/// Recursive helper: collect var.key accesses and detect dynamic usage of the named variable.
fn collect_var_accesses_node(
    node: &Arc<SurfaceNode>,
    var_name: &str,
    accesses: &mut Vec<String>,
    dynamic_use: &mut bool,
) {
    match &node.expr {
        SurfaceExpression::Field {
            expr: Some(target),
            field,
            ..
        } => {
            // Check if target is VarRef for the named variable
            if matches!(&target.expr, SurfaceExpression::VarRef { name, .. } if name == var_name) {
                // var.key — target is VarRef(var_name), no need to recurse into it.
                // For Ident keys: record the key name as a consumed pipeline key.
                // For Int keys: variable used as an indexed sequence — no named key to record.
                if let crate::ast::DotKey::Ident(key) = field {
                    accesses.push(key.clone());
                }
                // DotKey::Int: variable used as an indexed sequence — treat as dynamic access
                // because the pipeline lint operates on named keys only.
                else {
                    *dynamic_use = true;
                }
            } else {
                collect_var_accesses_node(target, var_name, accesses, dynamic_use);
            }
        }
        SurfaceExpression::VarRef { name, .. } if name == var_name => {
            // Variable used in a non-field context — dynamic access
            *dynamic_use = true;
        }
        // Recurse into child expressions
        SurfaceExpression::Dict { entries, scope: _ } => {
            for entry in entries {
                if let Some(key) = &entry.node.key {
                    collect_var_accesses_node(key, var_name, accesses, dynamic_use);
                }
                collect_var_accesses_node(&entry.node.value, var_name, accesses, dynamic_use);
            }
        }
        SurfaceExpression::Fn { body, .. } => {
            collect_var_accesses_node(body, var_name, accesses, dynamic_use);
        }
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } => {
            collect_var_accesses_node(func, var_name, accesses, dynamic_use);
            for arg in args {
                collect_var_accesses_node(arg, var_name, accesses, dynamic_use);
            }
            for na in named_args {
                collect_var_accesses_node(&na.node.value, var_name, accesses, dynamic_use);
            }
        }
        SurfaceExpression::Sequential(exprs) => {
            for e in exprs {
                collect_var_accesses_node(e, var_name, accesses, dynamic_use);
            }
        }
        SurfaceExpression::Pipe { lhs, rhs, .. } => {
            collect_var_accesses_node(lhs, var_name, accesses, dynamic_use);
            collect_var_accesses_node(rhs, var_name, accesses, dynamic_use);
        }
        SurfaceExpression::TypeAssert { expr, .. } => {
            collect_var_accesses_node(expr, var_name, accesses, dynamic_use);
        }
        SurfaceExpression::Match { scrutinee, arms } => {
            collect_var_accesses_node(scrutinee, var_name, accesses, dynamic_use);
            for arm in arms {
                collect_var_accesses_node(&arm.pattern, var_name, accesses, dynamic_use);
                collect_var_accesses_node(&arm.let_bindings, var_name, accesses, dynamic_use);
                if let Some(guard) = &arm.guard {
                    collect_var_accesses_node(guard, var_name, accesses, dynamic_use);
                }
                for body_expr in &arm.body {
                    collect_var_accesses_node(body_expr, var_name, accesses, dynamic_use);
                }
            }
        }
        SurfaceExpression::Unquote(inner) | SurfaceExpression::UnquoteSplice(inner) => {
            collect_var_accesses_node(inner, var_name, accesses, dynamic_use);
        }
        _ => {}
    }
}

/// Find a ClassDecl in a dict's entries by walking an InstanceDecl's class_name AST node.
///
/// Handles two cases:
/// - `VarRef("Addable")` → scan entries for key "Addable" with ClassDecl value
/// - `Field(VarRef("File"), "Readable")` → scan entries for key "File", enter its Dict
///   value, scan inner entries for key "Readable" with ClassDecl value
///
/// Returns `(class_id, method_names)` if found.
fn find_class_decl_in_entries(
    class_name: &Arc<SurfaceNode>,
    entries: &[Spanned<SurfaceEntry>],
) -> Option<(u64, Vec<String>)> {
    match &class_name.expr {
        SurfaceExpression::VarRef { name, .. } => find_class_decl_by_key(name, entries),
        SurfaceExpression::Field {
            expr: Some(inner),
            field: DotKey::Ident(field_name),
            ..
        } => {
            // Resolve the base (e.g. "File") in entries, then look inside for the field.
            let base_name = match &inner.expr {
                SurfaceExpression::VarRef { name, .. } => name,
                _ => return None,
            };
            for entry in entries {
                if entry_has_key(entry, base_name) {
                    if let SurfaceExpression::Dict {
                        entries: inner_entries,
                        ..
                    } = &entry.node.value.expr
                    {
                        return find_class_decl_by_key(field_name, inner_entries);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Scan entries for a keyed ClassDecl with the given name.
/// Returns `(class_id, method_names)` if found.
fn find_class_decl_by_key(
    name: &str,
    entries: &[Spanned<SurfaceEntry>],
) -> Option<(u64, Vec<String>)> {
    for entry in entries {
        if entry_has_key(entry, name) {
            if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                if let crate::ast::SurfaceDeclaration::ClassDecl {
                    methods,
                    pre_assigned_class_decl_id,
                    ..
                } = decl.as_ref()
                {
                    let class_id = pre_assigned_class_decl_id.get()?;
                    let method_names: Vec<String> = methods
                        .iter()
                        .filter_map(|me| {
                            me.node.key.as_ref().and_then(|k| match &k.expr {
                                SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                                SurfaceExpression::StringLiteral { content, .. } => {
                                    Some(content.clone())
                                }
                                _ => None,
                            })
                        })
                        .collect();
                    return Some((class_id, method_names));
                }
            }
        }
    }
    None
}

/// Check if an entry's key matches a given name (VarRef or StringLiteral).
fn entry_has_key(entry: &Spanned<SurfaceEntry>, name: &str) -> bool {
    match &entry.node.key {
        Some(key_node) => match &key_node.expr {
            SurfaceExpression::VarRef {
                name: key_name,
                escaped: false,
                ..
            } => key_name == name,
            SurfaceExpression::StringLiteral { content, .. } => content == name,
            _ => false,
        },
        None => false,
    }
}

/// Extract static string-keyed names from a SurfaceExpression::Dict's entries.
///
/// Handles two cases:
///
/// 1. Keyed entries — VarRef or string literal keys become static scope slots.
/// 2. InstanceDecl entries — emit plain method names (deduplicated) so the resolver's
///    letrec scope has slots for each method. Multiple instance arms defining the same
///    method (e.g., "+" for Int and "+" for String) produce only one slot.
pub(crate) fn surface_dict_static_keys(entries: &[Spanned<SurfaceEntry>]) -> Vec<String> {
    // Build the key list.
    // ClassDecl is type-level only — push the outer class name, no method injection.
    // InstanceDecl emits plain method names (deduplicated across arms).
    let mut keys = Vec::new();
    for entry in entries {
        // Push outer key name if present (for all keyed entries including named instances).
        if let Some(key_node) = &entry.node.key {
            match &key_node.expr {
                SurfaceExpression::StringLiteral { content, .. } => keys.push(content.clone()),
                SurfaceExpression::VarRef {
                    name,
                    escaped: false,
                    ..
                } => keys.push(name.clone()),
                _ => {}
            }
        }
        // For any InstanceDecl (named or anonymous), inject plain method names.
        if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
            if let crate::ast::SurfaceDeclaration::InstanceDecl { arms, .. } = decl.as_ref() {
                for (_pattern, method_entries) in arms {
                    for me in method_entries {
                        let method_name = match me.node.key.as_ref() {
                            Some(k) => match &k.expr {
                                SurfaceExpression::StringLiteral { content, .. } => content.clone(),
                                SurfaceExpression::VarRef { name, .. } => name.clone(),
                                _ => continue,
                            },
                            None => continue,
                        };
                        if !keys.contains(&method_name) {
                            keys.push(method_name);
                        }
                    }
                }
            }
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    // Tests use resolve_program() with the SurfaceProgram/SurfaceNode AST directly.
    use super::*;
    use crate::ast::{node_id, NodeId, SurfaceExpression};

    fn test_file(_src: &str) -> Arc<str> {
        Arc::from(file!())
    }

    /// Parse `src`, desugar, and resolve. Returns (program, table).
    fn parse_and_resolve(src: &str) -> (crate::ast::SurfaceProgram, ResolutionTable) {
        let output = crate::parser::parse(src, test_file(src)).expect("parse failed");
        let program = crate::desugar::desugar_surface_program(&output.program);
        // No runtime env in unit tests — dict-internal and lexical references still
        // resolve via the resolver's scope tracking; env-provided names (builtins) use None.
        let scope = SymbolTable::empty();
        let result = resolve_program(&program, &scope);
        (program, result.table)
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
            SurfaceExpression::Dict { entries, scope: _ } => {
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
            SurfaceExpression::Pipe { lhs, rhs, .. } => {
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
                    collect_varrefs_in_node(&arm.pattern, name, out);
                    collect_varrefs_in_node(&arm.let_bindings, name, out);
                    if let Some(guard) = &arm.guard {
                        collect_varrefs_in_node(guard, name, out);
                    }
                    for body_expr in &arm.body {
                        collect_varrefs_in_node(body_expr, name, out);
                    }
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
                !table.contains_key(id),
                "free VarRef should have no entry in the resolution table"
            );
        }
    }

    /// A Dict's values can see sibling keys: `[x: 1  y: $x]` — the VarRef `$x` in `y`'s
    /// value should resolve to Dispatch(0, 0) since `x` is the first key in scope.
    #[test]
    fn dict_sibling_key_scoping() {
        let (program, table) = parse_and_resolve("[x: 1  y: $x]");
        let refs = find_varref_nodes(&program, "x");
        assert!(!refs.is_empty(), "expected at least one VarRef for $x");
        // $x inside the dict value for y resolves to Dispatch(0, 0) — first key
        let (id, _) = &refs[0];
        let addr = table
            .get(id)
            .expect("$x should be resolved (it's a sibling key)");
        assert_eq!(
            addr,
            &VarAddr::Dispatch(0, 0),
            "x should be Dispatch(0, 0) (first key in same dict scope)"
        );
    }

    /// In a Fn body, VarRef to the param resolves to Dispatch(0, 0).
    #[test]
    fn fn_param_scoping_in_body() {
        let (program, table) = parse_and_resolve("[fn [let myarg] $myarg]");
        let refs = find_varref_nodes(&program, "myarg");
        assert!(!refs.is_empty(), "expected at least one VarRef for $myarg");
        let (id, _) = &refs[0];
        let addr = table
            .get(id)
            .expect("$myarg should be resolved to fn param scope");
        assert_eq!(
            addr,
            &VarAddr::Dispatch(0, 0),
            "first fn param should be Dispatch(0, 0)"
        );
    }

    /// A multi-param fn resolves each param to its correct slot.
    #[test]
    fn fn_multi_param_slots() {
        let (program, table) = parse_and_resolve("[fn [let a b c] $b]");
        let refs = find_varref_nodes(&program, "b");
        assert!(!refs.is_empty(), "expected VarRef for $b");
        let (id, _) = &refs[0];
        let addr = table.get(id).expect("$b should be resolved");
        assert_eq!(
            addr,
            &VarAddr::Dispatch(0, 1),
            "b is the second param (slot 1), should be Dispatch(0, 1)"
        );
    }

    /// A VarRef inside a fn body that refers to an outer dict key resolves to
    /// Dispatch — the resolver does not do closure conversion.
    /// The evaluator handles cross-fn-boundary access mechanics.
    #[test]
    fn fn_body_captures_outer_dict_key() {
        // outer: 42  inner: [fn [] $outer]
        // $outer is a free variable in the fn body — resolves to Dispatch(depth, slot)
        let (program, table) = parse_and_resolve("[outer: 42  inner: [fn [let] $outer]]");
        let refs = find_varref_nodes(&program, "outer");
        assert!(
            !refs.is_empty(),
            "expected VarRef for $outer inside fn body"
        );
        let (id, _) = &refs[0];
        let addr = table
            .get(id)
            .expect("$outer should be resolved (captured from dict scope)");
        assert!(
            matches!(addr, VarAddr::Dispatch(_, _)),
            "$outer should resolve to Dispatch (no closure conversion), got {:?}",
            addr
        );
    }

    /// Match arm pattern bindings should be resolved in the arm body.
    /// Uses [case [let n] _ $n] form: [let n] declares the binding, _ matches anything,
    /// and $n in the body resolves to the case arm's scope as Dispatch(0, 0).
    ///
    /// Case arm bindings get per-frame 0-based slots in their own SymbolTable frame.
    #[test]
    fn match_arm_pattern_binding() {
        // Bare lowercase names in match arm patterns are now Pin (not Variable).
        // To bind a variable in a match arm, use [case [let n] pattern body] form.
        // [case [let n] _  $n] — n is declared by [let n], _ matches anything, $n resolves.
        let (program, table) = parse_and_resolve("[match 42 [case [let n] _ $n]]");
        let refs = find_varref_nodes(&program, "n");
        assert!(!refs.is_empty(), "expected VarRef for $n in case arm body");
        let (id, _) = &refs[0];
        let addr = table
            .get(id)
            .expect("$n should be resolved (case arm binding in arm scope)");
        assert_eq!(
            addr,
            &VarAddr::Dispatch(0, 0),
            "n is the first (and only) binding — Dispatch(0, 0) in its SymbolTable frame"
        );
    }

    /// Case arm bodies see the bindings declared in [let ...].
    /// Bare lowercase names in match arm patterns are now Pin (not Variable).
    /// To bind a variable, use [case [let n] pattern body] form.
    ///
    /// Case arm bindings resolve to Dispatch(0, slot) where slot is per-frame 0-based.
    #[test]
    fn match_arm_guard_sees_pattern_bindings() {
        // `n:` in match arm position creates a Pin pattern, not a variable binding.
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
            let addr = table
                .get(id)
                .expect("$n should be resolved via case arm [let n]");
            // The case arm scope introduces n as Dispatch(0, 0) in its SymbolTable frame
            assert_eq!(
                addr,
                &VarAddr::Dispatch(0, 0),
                "n is Dispatch(0, 0) in its SymbolTable frame"
            );
        }
    }

    /// Match with multiple arms: type patterns (Int, String, etc.)
    /// The body of each arm can reference outer variables.
    #[test]
    fn match_dict_pattern_bindings() {
        // Match with two type arms; both arm bodies reference outer $x.
        let src = "[x: 1  result: [match $x Int: [+ $x 1] ...: 0]]";
        let (program, table) = parse_and_resolve(src);

        // Check $x resolves (should appear at least twice: match scrutinee + Int arm body)
        let x_refs = find_varref_nodes(&program, "x");
        assert!(
            x_refs.len() >= 2,
            "expected at least 2 VarRefs for $x, got {}",
            x_refs.len()
        );
        // The resolver does not do closure conversion.
        // All $x references resolve to Dispatch — either Dispatch(0, 0) for the scrutinee
        // or Dispatch(depth, 0) for references inside arm bodies.
        for (id, _) in &x_refs {
            let addr = table.get(id).expect("$x should be resolved (dict binding)");
            assert!(
                matches!(addr, VarAddr::Dispatch(_, 0)),
                "$x should be Dispatch(_, 0), got {:?}",
                addr
            );
        }
    }

    /// Match with wildcard pattern should introduce no bindings.
    /// A VarRef inside a wildcard arm body must NOT be slot-resolved — the
    /// wildcard creates no scope entries, so `$x` stays a FreeVar.
    #[test]
    fn match_wildcard_pattern_no_bindings() {
        // `$x` in the wildcard arm body refers to nothing bound by `_`.
        // The resolver must not produce a table entry for this VarRef.
        let (program, table) = parse_and_resolve("[match val ...: $x]");
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
                !table.contains_key(id),
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
        let addr = table
            .get(id)
            .expect("$a should be resolved (key from prior expr in document)");
        // The first dict creates a scope with `a` as Dispatch(0, 0)
        assert_eq!(
            addr,
            &VarAddr::Dispatch(0, 0),
            "a is first key from prior expr, Dispatch(0, 0)"
        );
    }

    // --- B-600: Unreferenced env-name tracking ---

    /// Helper: parse, desugar, and resolve via the env-dict protocol,
    /// returning the unreferenced env names.
    fn resolve_with_env_dict(src: &str, env_names: &[String]) -> Vec<String> {
        let output = crate::parser::parse(src, test_file(src)).expect("parse failed");
        let program = crate::desugar::desugar_program_full(&output.program);
        let program = crate::expand::expand_program(&program);
        let doc = &program.documents[0].node;
        // Build scope from env_names with 0-based contiguous slots.
        let mut st = SymbolTable::new();
        for (i, name) in env_names.iter().enumerate() {
            st.insert(name.clone(), i as u32, None);
        }
        let scope = Arc::new(st);
        let result = resolve_document(doc, &scope);
        result.unreferenced
    }

    /// When some env names are unused, they appear in the unreferenced list.
    #[test]
    fn unreferenced_includes_unused_env_name() {
        // source: "[result: %]" — references % but not x
        let env_names = vec!["%".to_string(), "x".to_string()];
        let unreferenced = resolve_with_env_dict("[result: $%]", &env_names);
        assert!(
            unreferenced.contains(&"x".to_string()),
            "expected 'x' in unreferenced, got: {:?}",
            unreferenced
        );
        assert!(
            !unreferenced.contains(&"%".to_string()),
            "expected '%' NOT in unreferenced, got: {:?}",
            unreferenced
        );
    }

    /// When all env names are referenced, unreferenced is empty.
    #[test]
    fn unreferenced_empty_when_all_used() {
        // source: "[result: %]" — references %
        let env_names = vec!["%".to_string()];
        let unreferenced = resolve_with_env_dict("[result: $%]", &env_names);
        assert!(
            unreferenced.is_empty(),
            "expected empty unreferenced when all env names used, got: {:?}",
            unreferenced
        );
    }

    /// When no env names are referenced, all appear in unreferenced.
    #[test]
    fn unreferenced_lists_all_when_none_used() {
        // source: "[result: 42]" — references neither % nor x
        let env_names = vec!["%".to_string(), "x".to_string()];
        let unreferenced = resolve_with_env_dict("[result: 42]", &env_names);
        assert!(
            unreferenced.contains(&"%".to_string()),
            "expected '%' in unreferenced, got: {:?}",
            unreferenced
        );
        assert!(
            unreferenced.contains(&"x".to_string()),
            "expected 'x' in unreferenced, got: {:?}",
            unreferenced
        );
    }

    /// Shadowed env names must not be falsely marked as referenced.
    #[test]
    fn unreferenced_shadowed_env_name_not_falsely_marked() {
        // Document declares local "x" that shadows env-dict "x".
        // The local x is used, but the env-dict x is NOT — it should be unreferenced.
        let env_names = vec!["x".to_string(), "%".to_string()];
        let unreferenced = resolve_with_env_dict("[x: 42  result: $x]", &env_names);
        assert!(
            unreferenced.contains(&"x".to_string()),
            "env-dict 'x' must be unreferenced when shadowed by local: {:?}",
            unreferenced
        );
        assert!(
            unreferenced.contains(&"%".to_string()),
            "% must be unreferenced (never used): {:?}",
            unreferenced
        );
    }

    // --- T-1742: Pipeline stage lint unit tests ---

    /// All keys produced by stage are consumed — no warnings.
    #[test]
    fn lint_pipeline_stages_all_consumed_no_warn() {
        let dummy_span = crate::rust_span!();
        let stages = vec![(
            vec!["x".to_string(), "y".to_string()],
            vec!["x".to_string(), "y".to_string()],
            false,
            dummy_span,
        )];
        let warnings = lint_pipeline_stages(&stages);
        assert!(
            warnings.is_empty(),
            "expected no warnings when all keys consumed, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
    }

    /// Stage produces a key that the next stage does not consume — warning fires.
    #[test]
    fn lint_pipeline_stages_ignored_key_warns() {
        let dummy_span = crate::rust_span!();
        let stages = vec![(
            vec!["x".to_string(), "y".to_string()],
            vec!["x".to_string()], // y not consumed
            false,
            dummy_span,
        )];
        let warnings = lint_pipeline_stages(&stages);
        let abandoned: Vec<&str> = warnings
            .iter()
            .filter(|w| w.kind == "abandoned-output")
            .map(|w| w.message.as_str())
            .collect();
        assert_eq!(
            abandoned.len(),
            1,
            "expected 1 abandoned-output warning, got: {:?}",
            abandoned
        );
        assert!(
            abandoned[0].contains("'y'"),
            "expected warning to mention key 'y', got: {:?}",
            abandoned[0]
        );
    }

    /// Dynamic % access (uses_dynamic = true) suppresses all warnings.
    #[test]
    fn lint_pipeline_stages_dynamic_access_no_warn() {
        let dummy_span = crate::rust_span!();
        let stages = vec![(
            vec!["x".to_string(), "y".to_string()],
            vec![], // no static accesses — dynamic only
            true,   // uses_dynamic = true
            dummy_span,
        )];
        let warnings = lint_pipeline_stages(&stages);
        assert!(
            warnings.is_empty(),
            "expected no warnings when dynamic % access, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
    }

    /// Pass-through pattern — next stage consumes a superset of produced keys.
    #[test]
    fn lint_pipeline_stages_passthrough_no_warn() {
        let dummy_span = crate::rust_span!();
        // Next stage accesses x, y, z — all three, stage only produces x and y.
        let stages = vec![(
            vec!["x".to_string(), "y".to_string()],
            vec!["x".to_string(), "y".to_string(), "z".to_string()],
            false,
            dummy_span,
        )];
        let warnings = lint_pipeline_stages(&stages);
        assert!(
            warnings.is_empty(),
            "expected no warnings when all produced keys are consumed, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
    }

    // --- B-586: Nested dict per-frame slot alignment ---

    /// A nested dict literal (dict as value of an outer dict entry) uses
    /// per-frame 0-based slots in its own SymbolTable frame.
    ///
    /// With no initial scope: `[x: 1  inner: [ref: $x]]`.
    ///   Outer dict scope: x→slot 0, inner→slot 1.
    ///   Inner dict scope: ref→slot 0 (own frame).
    ///   $x from inner dict → Dispatch(1, 0) — depth 1, slot 0.
    #[test]
    fn nested_dict_lgm_offset_zero_base() {
        // With no initial frames, base offset = 0.
        let (program, table) = parse_and_resolve("[x: 1  inner: [ref: $x]]");

        // `x` resolves as sibling in outer dict: Dispatch(1, 0)
        let x_refs = find_varref_nodes(&program, "x");
        assert!(!x_refs.is_empty(), "expected VarRef for $x");
        let (x_id, _) = &x_refs[0];
        let x_addr = table.get(x_id).expect("$x should be resolved");
        assert_eq!(
            x_addr,
            &VarAddr::Dispatch(1, 0),
            "$x should be Dispatch(1, 0) — first entry of outer dict, seen from inner dict"
        );

        // Find the VarRef that is the VALUE of `ref` in the inner dict.
        // There is exactly one $x VarRef (in the inner dict's value).
        // It must resolve to Dispatch(_, 0), not something else.
        // (Already verified above — the same VarRef node is found.)
    }

    /// Nested dict with a non-zero initial scope.
    ///
    /// Simulates the case where root entries exist (e.g., builtins) before the document
    /// dict. With per-frame 0-based slots, the outer dict's entries get slots 0, 1 in
    /// their own frame, and the root scope is a separate parent frame.
    ///   Outer dict: x→slot 0, inner→slot 1 (per-frame).
    ///   Inner dict: ref→slot 0 (per-frame).
    ///   $x from inner dict → Dispatch(1, 0) — depth 1 to outer dict frame, slot 0.
    #[test]
    fn nested_dict_lgm_offset_nonzero_base() {
        // Seed with one root-scope entry to simulate builtins.
        let output =
            crate::parser::parse("[x: 1  inner: [ref: $x]]", test_file("")).expect("parse");
        let program = crate::desugar::desugar_surface_program(&output.program);
        let doc = &program.documents[0].node;

        // Build root scope with one entry.
        let mut root_st = SymbolTable::new();
        root_st.insert("builtin-dict-get".to_string(), 0, None);
        let scope = Arc::new(root_st);
        let result = resolve_document(doc, &scope);

        // With per-frame 0-based slots:
        // Outer dict scope: x→0, inner→1
        // Inner dict scope: ref→0
        // $x from inner dict: depth 1 (inner→outer), slot 0
        let x_refs = find_varref_nodes(&program, "x");
        assert!(!x_refs.is_empty(), "expected VarRef for $x");
        let (x_id, _) = &x_refs[0];
        let x_addr = result.table.get(x_id).expect("$x should be resolved");
        assert_eq!(
            x_addr,
            &VarAddr::Dispatch(1, 0),
            "$x must be Dispatch(1, 0) — x is slot 0 in the outer dict frame, one scope up"
        );
    }

    /// Nested dict inside a fn body intermediate dict value.
    ///
    /// `[fn [let x] [a: $x  nested: [c: $x]] ...]`
    /// With per-frame 0-based slots:
    ///   Fn param scope: x→slot 0.
    ///   Body dict scope: a→slot 0, nested→slot 1 (separate frame).
    ///   Inner dict scope: c→slot 0 (separate frame).
    /// All $x refs have slot=0 (the fn param's slot) but varying depth.
    #[test]
    fn nested_dict_inside_fn_body_dict() {
        let (program, table) = parse_and_resolve("[fn [let x] [a: $x  nested: [c: $x]] a]");

        let x_refs = find_varref_nodes(&program, "x");
        assert!(
            x_refs.len() >= 2,
            "expected at least 2 VarRefs for $x (in a's value and c's value)"
        );
        for (id, _) in &x_refs {
            let addr = table.get(id).expect("$x should be resolved to fn param");
            match addr {
                VarAddr::Dispatch(_, slot) => {
                    assert_eq!(*slot, 0, "$x must be at slot 0 (param slot)");
                }
                other => panic!("expected Dispatch, got {:?}", other),
            }
        }
    }

    // --- B-577: nested fn referencing outer fn's parameter ---

    /// When fn_B is nested inside fn_A and references fn_A's parameter,
    /// the reference resolves to Parameter (the outer fn's param index).
    /// With closure conversion removed, the resolver preserves the original
    /// address; the evaluator handles cross-fn-boundary access via definition_scope.
    #[test]
    fn nested_fn_references_outer_fn_param() {
        let (_program, table) = parse_and_resolve("[outer: [fn [let x] [fn [let y] $x]]]");

        // $x inside inner fn should resolve — the resolver assigns an address.
        let x_refs = find_varref_nodes(&_program, "x");
        assert!(
            !x_refs.is_empty(),
            "expected VarRef for $x in inner fn body"
        );
        let (id, _) = &x_refs[0];
        let addr = table.get(id).expect("$x should be resolved");
        // x is the outer fn's parameter — resolver produces Dispatch.
        assert!(
            matches!(addr, VarAddr::Dispatch(_, _)),
            "$x in inner fn body should be Dispatch, got {:?}",
            addr
        );
    }

    /// Triple nesting: fn_A -> fn_B -> fn_C, where fn_C references fn_A's parameter.
    /// With closure conversion removed, the resolver preserves the original address.
    #[test]
    fn triple_nested_fn_references_outermost_param() {
        let (_program, table) = parse_and_resolve("[f: [fn [let x] [fn [let y] [fn [let z] $x]]]]");

        let x_refs = find_varref_nodes(&_program, "x");
        assert!(!x_refs.is_empty());
        let (id, _) = &x_refs[0];
        let addr = table.get(id).expect("$x should be resolved");
        // x is the outermost fn's parameter — resolver produces Dispatch.
        assert!(
            matches!(addr, VarAddr::Dispatch(_, _)),
            "$x in fn_C should be Dispatch, got {:?}",
            addr
        );
    }

    #[test]
    fn collect_var_accesses_works_with_non_percent_var_name() {
        // Verifies collect_var_accesses is truly generic: it works with any var_name,
        // not just "%". Source accesses "input" field on the named variable "src".
        let output = crate::parser::parse("[result: src.input]", test_file("[result: src.input]"))
            .expect("parse failed");
        let program = crate::desugar::desugar_program_full(&output.program);
        let program = crate::expand::expand_program(&program);
        let doc = &program.documents[0].node;
        let (accesses, dynamic) = super::collect_var_accesses(doc, "src");
        assert!(
            accesses.contains(&"input".to_string()),
            "expected 'input' access on 'src'"
        );
        assert!(!dynamic, "field access should not be dynamic");
        // Verify % is not mistakenly tracked when it's not in the source.
        let (pct_accesses, _) = super::collect_var_accesses(doc, "%");
        assert!(
            pct_accesses.is_empty(),
            "% should have no accesses in this doc"
        );
    }

    #[test]
    fn collect_document_produced_keys_empty_doc_returns_doc_span() {
        // Build a SurfaceDocument with no Expr items — only an empty header.
        let doc = crate::ast::SurfaceDocument {
            header: indexmap::IndexMap::new(),
            items: Vec::new(),
        };
        let doc_span = crate::ast::Span::new(10, 5, 20, 15, std::sync::Arc::from("test-file.llt"));
        let (keys, span) = super::collect_document_produced_keys(&doc, &doc_span);
        assert!(keys.is_empty(), "empty doc should produce no keys");
        assert_eq!(
            span, doc_span,
            "returned span must be the passed doc_span, not a Rust source location"
        );
        // Verify the span points to test-file.llt, not a Rust source file.
        assert_eq!(&*span.file, "test-file.llt");
        assert_eq!(span.start_line, 10);
        assert_eq!(span.start_col, 5);
    }

    /// B-724: Case arm cross-fn reference consistency (historical).
    ///
    /// When a match has both a [case [let bindings] ...] arm and a keyed arm (...:),
    /// and both arms contain nested functions that reference the same outer name,
    /// both fns must resolve the reference consistently. With closure conversion
    /// removed, all references resolve to Dispatch.
    #[test]
    fn case_arm_sibling_keyed_arm_fn_dispatch_consistent() {
        let src = r#"
[
  exports: 42
  result: [match 0
    [case [let] 0 [fn [] $exports]]
    ...:          [fn [] $exports]]
]
"#;
        let (program, table) = parse_and_resolve(src);

        // Find all VarRefs for 'exports'.
        let exports_refs = find_varref_nodes(&program, "exports");
        assert_eq!(
            exports_refs.len(),
            2,
            "expected 2 VarRefs for exports (one in case arm fn, one in keyed arm fn)"
        );

        // Both should resolve to Dispatch (no closure conversion).
        for (id, _) in &exports_refs {
            let addr = table.get(id).expect("exports should be resolved");
            assert!(
                matches!(addr, VarAddr::Dispatch(_, _)),
                "exports should be Dispatch, got {:?}",
                addr
            );
        }
    }

    /// B-732 (historical): Deeply nested sibling fn reference — verifies that both
    /// sibling fns inside an outer fn resolve root-name consistently.
    ///
    /// With closure conversion removed, the resolver always produces Dispatch
    /// addresses. The evaluator handles cross-fn-boundary access mechanics
    /// via the definition-site group carried by functions.
    #[test]
    fn sibling_fn_dispatch_addr_consistent() {
        let src = r#"
[
  root-name: 42
  outer: [fn []
    [match 0
      [case [let] 0 [fn [] $root-name]]
      ...:          [fn [] $root-name]]]
]
"#;
        let (program, table) = parse_and_resolve(src);

        // Find all VarRefs for 'root-name'.
        let root_refs = find_varref_nodes(&program, "root-name");
        assert_eq!(
            root_refs.len(),
            2,
            "expected 2 VarRefs for root-name (one in case arm fn, one in sibling fn)"
        );

        // Both should resolve to Dispatch (no closure conversion).
        for (id, _) in &root_refs {
            let addr = table.get(id).expect("root-name should be resolved");
            assert!(
                matches!(addr, VarAddr::Dispatch(_, _)),
                "root-name should be Dispatch, got {:?}",
                addr
            );
        }
    }
}
