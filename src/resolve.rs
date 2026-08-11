//! Variable resolution pass: assigns VarAddr addresses to VarRef nodes.
//!
//! The resolver walks the AST and assigns VarAddr values to static variable references
//! before evaluation begins:
//!
//! - `Dispatch(depth, slot)` — reference to `accumulated_group[slot]`. Slot is an
//!   ABSOLUTE cumulative index: root-scope entries occupy slots 0..N-1 (from
//!   `enter_scope_from_frame`), document dict entries follow at cumulative offsets assigned
//!   by `walk_surface_document_with_offset`, and function/match-arm parameters are appended
//!   after definition_scope entries. Cross-dict references within a document use Dispatch with
//!   the absolute slot — no runtime frame traversal needed.
//! - `EffectPerform { slot }` — typeclass method call; resolved at runtime via slot lookup.
//!
//! **Invariants:**
//! - Must run exactly once per AST (write-once OnceLock).
//! - Must run after desugaring (sees $_ as Fn nodes, not VarRef("_")).
//! - Must run before typechecking and evaluation (both consumers of resolved coords).
//!
//! See doc/whatif/arena-patterns.md §Variable Resolution Pass Design for the full specification.

use crate::ast::{
    node_id, DotKey, ResolutionTable, Spanned, SurfaceDeclaration, SurfaceDocument, SurfaceEntry,
    SurfaceExpression, SurfaceItem, SurfaceNode, SurfaceProgram, VarAddr,
};
use crate::error::Diagnostic;
use std::sync::Arc;

// ============================================================================
// runtime-v2: SurfaceProgram resolver — produces ResolutionTable

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
    /// Each scope frame maps name → VarAddr.
    scopes: Vec<indexmap::IndexMap<String, VarAddr>>,
    table: ResolutionTable,
    diagnostics: Vec<Diagnostic>,
    /// Names from the env-dict (name-set) that were referenced during this document's walk.
    /// Used to compute unreferenced env names returned by resolve_surface_document_with_env_dict.
    referenced_env_names: std::collections::HashSet<String>,
    /// Depth of the env-dict scope in the scope stack (set by resolve_surface_document_with_env_dict).
    /// When set, only names resolved at this depth are recorded in referenced_env_names.
    env_scope_depth: Option<usize>,
    /// Tracks the cumulative base slot for the next `SurfaceExpression::Dict` letrec scope.
    ///
    /// At runtime, `eval_dict_core` builds `extended_group = outer_frame.group ++ letrec_slots`,
    /// placing the dict's entries at indices `outer_frame.group.len()..outer_frame.group.len()+N`.
    /// The resolver must assign LGM(outer_frame.group.len()+i) to entry i — NOT LGM(i) — so that
    /// `LGM(slot)` resolves to `frame.group[slot]` which is the correct thunk.
    ///
    /// This field tracks the value that `outer_frame.group.len()` will have at the eval_dict_core
    /// call site for the next nested dict. Updated rules:
    /// - Document level: set to `cumulative_offset` before walking each document dict node
    ///   (see `walk_surface_document_with_offset`).
    /// - Dict arm (`SurfaceExpression::Dict`): save, use as base, advance by `static_keys.len()`
    ///   before walking values (so nested dicts see the right base), restore on exit.
    /// - Sequential arm (fn intermediate dict bodies): set to `sequential_offset + body_key_count`
    ///   before walking each body's values.
    /// - Fn arm (`SurfaceExpression::Fn`): save, reset to 0 (fn call frame has `group = []`),
    ///   restore on exit.
    accumulated_dict_offset: u32,
}

impl SurfaceResolver {
    fn new() -> Self {
        Self {
            scopes: Vec::new(),
            table: ResolutionTable::new(),
            diagnostics: Vec::new(),
            referenced_env_names: std::collections::HashSet::new(),
            env_scope_depth: None,
            accumulated_dict_offset: 0,
        }
    }

    /// Inject EffectPerform addresses for class methods into the current topmost scope.
    /// Used when the resolver is seeded with class information from previously-resolved
    /// files — the class method names must be findable through the normal scope chain.
    fn inject_class_methods(&mut self, methods: &std::collections::HashMap<String, u64>) {
        if let Some(scope) = self.scopes.last_mut() {
            for (method_name, _class_id) in methods {
                let should_inject = match scope.get(method_name) {
                    None => true,
                    Some(VarAddr::EffectPerform { .. }) => true,
                    _ => false,
                };
                if should_inject {
                    scope.insert(
                        method_name.clone(),
                        VarAddr::EffectPerform {
                            slot: 0, // placeholder — will be resolved to actual slot at lookup time
                        },
                    );
                }
            }
        }
    }

    /// Enter a letrec dict scope with a slot offset.
    /// Each key gets `Dispatch(0, offset + i)` as its VarAddr.
    /// Used by walk_surface_document to assign cumulative Dispatch indices to sequential
    /// dict scopes so that cross-document references don't collide with prior dicts.
    fn enter_scope_with_offset(&mut self, keys: &[String], offset: u32) {
        let mut scope: indexmap::IndexMap<String, VarAddr> =
            indexmap::IndexMap::with_capacity(keys.len());
        for (i, key) in keys.iter().enumerate() {
            // depth=0 is a placeholder; resolve_name overwrites depth with the actual
            // scope-stack traversal distance when the name is looked up.
            scope.insert(key.clone(), VarAddr::Dispatch(0, offset + i as u32));
        }
        self.scopes.push(scope);
    }

    /// Enter a scope with pre-computed (name, slot) pairs.
    ///
    /// Unlike `enter_scope_with_offset` which assigns contiguous slots from a base offset,
    /// this method accepts explicit slot positions for each name. Used when entries get
    /// uniform sequential slot assignment (all entries, not just static-key ones), so
    /// static key names may have non-contiguous slot positions.
    fn enter_scope_with_slot_pairs(&mut self, pairs: &[(String, u32)]) {
        let mut scope: indexmap::IndexMap<String, VarAddr> =
            indexmap::IndexMap::with_capacity(pairs.len());
        for (name, slot) in pairs {
            scope.insert(name.clone(), VarAddr::Dispatch(0, *slot));
        }
        self.scopes.push(scope);
    }

    /// Enter a function parameter scope: each param gets `Dispatch(0, slot)` as its VarAddr.
    /// Params are assigned slots starting at accumulated_dict_offset (the definition_scope size).
    /// Advances accumulated_dict_offset by params.len() so body dict entries start after params.
    fn enter_param_scope(&mut self, params: &[String]) {
        let mut scope = indexmap::IndexMap::with_capacity(params.len());
        for (i, name) in params.iter().enumerate() {
            let slot = self.accumulated_dict_offset + i as u32;
            scope.insert(name.clone(), VarAddr::Dispatch(0, slot));
        }
        self.scopes.push(scope);
        self.accumulated_dict_offset += params.len() as u32;
    }

    /// Seed scope from an external frame (initial_frames from the loader).
    ///
    /// External frames carry u32 slot indices into the root group (accumulated_group).
    /// Root-scope names (builtins and capabilities) are assigned `Dispatch(0, slot)` —
    /// they occupy the first slots of the accumulated_group at runtime. Document dict entries
    /// follow at cumulative slot offsets above the root-scope slots.
    fn enter_scope_from_frame(&mut self, frame: &indexmap::IndexMap<String, u32>) {
        let converted: indexmap::IndexMap<String, VarAddr> = frame
            .iter()
            // depth=0 placeholder; resolve_name overwrites depth with traversal offset at lookup.
            .map(|(k, &slot)| (k.clone(), VarAddr::Dispatch(0, slot)))
            .collect();
        self.scopes.push(converted);
    }

    fn exit_scope(&mut self) {
        self.scopes.pop().expect("scopes is empty");
    }

    /// Resolve `name` in the current scope stack, returning its `VarAddr`.
    ///
    /// All variable addressing uses exactly two variants:
    /// - `Dispatch(depth, slot)` — absolute cumulative slot in the accumulated_group.
    ///   Root-scope entries occupy slots 0..N-1; document dict entries follow at cumulative
    ///   offsets; function/match-arm parameters are appended after definition_scope entries.
    ///   Cross-dict references (a name from dict_i referenced in dict_j where j>i)
    ///   resolve directly to Dispatch(depth, slot) because walk_surface_document assigns cumulative
    ///   slots via enter_scope_with_offset, making each slot unique across the document.
    /// - `EffectPerform { slot }` — typeclass method call; resolved at runtime via slot lookup.
    ///
    /// If a name crosses a fn boundary, it is added to the current function's capture list
    /// (if not already present) with its original Dispatch/Parameter address. The VarRef
    /// itself still receives the Dispatch address — the evaluator handles cross-fn-boundary
    /// access via the definition-site group carried by function values.
    fn resolve_name(&mut self, name: &str) -> Option<VarAddr> {
        // Search from innermost scope outward.
        let scopes_len = self.scopes.len();
        let found = self
            .scopes
            .iter()
            .rev()
            .enumerate()
            .find_map(|(offset, scope)| {
                scope.get(name).map(|addr| {
                    let frame_abs_idx = scopes_len.saturating_sub(1 + offset);
                    (frame_abs_idx, offset as u32, addr.clone())
                })
            });

        let (match_depth, traversal_offset, addr) = found?;
        // Inject the correct depth into Dispatch entries. The stored depth=0 is a
        // placeholder written at scope-construction time; the actual traversal offset is only
        // known at lookup time (here). The evaluator ignores depth and uses slot directly.
        let addr = match addr {
            VarAddr::Dispatch(_, slot) => VarAddr::Dispatch(traversal_offset, slot),
            other => other,
        };

        if self.env_scope_depth.map_or(false, |d| match_depth == d) {
            self.referenced_env_names.insert(name.to_string());
        }

        Some(addr)
    }

    /// Resolve `name` using leading-dot semantics: find the **second** occurrence of `name`
    /// in the scope chain, skipping the innermost (first) binding.
    ///
    /// `.name` means "the next `name` beyond the nearest one." This is relative to where
    /// `name` is actually bound, not to the structure of Dict scope boundaries:
    ///
    /// - Find the first binding of `name` anywhere in the scope chain (innermost wins).
    /// - Skip it and search outward for the next binding of `name`.
    /// - Return `None` if there is no second binding — caller emits "undefined variable".
    ///
    /// This is strictly more correct than the old "skip nearest Dict scope" heuristic:
    ///   - Works even when `name` is not in the nearest Dict (no over-skipping).
    ///   - No special cases: if there is only one binding, `.name` is an error.
    ///   - Composable: a dispatcher that chains via `.method` naturally reaches the
    ///     prelude's copy regardless of how many scope levels separate them.
    ///
    fn resolve_name_parent(&mut self, name: &str) -> Option<VarAddr> {
        let scopes_len = self.scopes.len();
        let mut found_first = false;
        let found = self
            .scopes
            .iter()
            .rev()
            .enumerate()
            .find_map(|(offset, scope)| {
                if !found_first {
                    if scope.contains_key(name) {
                        found_first = true;
                    }
                    return None;
                }
                scope.get(name).map(|addr| {
                    let frame_abs_idx = scopes_len.saturating_sub(1 + offset);
                    (frame_abs_idx, offset as u32, addr.clone())
                })
            });

        let (match_depth, traversal_offset, addr) = found?;
        let addr = match addr {
            VarAddr::Dispatch(_, slot) => VarAddr::Dispatch(traversal_offset, slot),
            other => other,
        };

        if self.env_scope_depth.map_or(false, |d| match_depth == d) {
            self.referenced_env_names.insert(name.to_string());
        }

        Some(addr)
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

            SurfaceExpression::Dict {
                entries,
                scope_frame: _,
            } => {
                // Walk key expressions in outer scope.
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        let is_static =
                            matches!(&key.expr, SurfaceExpression::VarRef { escaped: false, .. });
                        if is_static {}
                        self.walk_surface_node(key);
                        if is_static {}
                    }
                }

                let base_offset = self.accumulated_dict_offset;

                // Uniform slot assignment: every entry gets a real sequential slot.
                // Assign slots BEFORE entering scope so we can collect the actual
                // (name, slot) pairs for static-key entries. Static keys may have
                // non-contiguous slot positions when non-static entries are interspersed.
                let mut scope_pairs: Vec<(String, u32)> = Vec::new();
                {
                    let mut slot_idx: u32 = 0;
                    for entry in entries.iter() {
                        // Collect static key name → actual slot for scope entry.
                        if let Some(key) = &entry.node.key {
                            match &key.expr {
                                SurfaceExpression::StringLiteral { content, .. } => {
                                    scope_pairs.push((content.clone(), base_offset + slot_idx));
                                }
                                SurfaceExpression::VarRef {
                                    name,
                                    escaped: false,
                                    ..
                                } => {
                                    scope_pairs.push((name.clone(), base_offset + slot_idx));
                                }
                                _ => {}
                            }
                        }
                        // First write wins: shared AST nodes may be visited multiple times.
                        let _ = entry.node.assigned_slot.set(base_offset + slot_idx);
                        slot_idx += 1;
                        // InstanceDecl method slots: advance slot_idx past method bindings
                        // so subsequent entries get the correct slot.
                        if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                            if let crate::ast::SurfaceDeclaration::InstanceDecl { arms, .. } =
                                decl.as_ref()
                            {
                                for (_pattern, method_entries) in arms {
                                    for me in method_entries {
                                        let method_name =
                                            me.node.key.as_ref().and_then(|k| match &k.expr {
                                                SurfaceExpression::StringLiteral {
                                                    content,
                                                    ..
                                                } => Some(content.clone()),
                                                SurfaceExpression::VarRef { name, .. } => {
                                                    Some(name.clone())
                                                }
                                                _ => None,
                                            });
                                        if let Some(name) = method_name {
                                            if !scope_pairs.iter().any(|(n, _)| n == &name) {
                                                scope_pairs.push((name, base_offset + slot_idx));
                                            }
                                            let _ =
                                                me.node.assigned_slot.set(base_offset + slot_idx);
                                            slot_idx += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Enter the dict's letrec scope with actual slot positions.
                    //
                    // At runtime, eval_dict_core builds extended_group = outer_frame.group
                    // ++ letrec_slots, placing ALL entries (not just static-key ones) at
                    // sequential indices. The resolver assigns Dispatch(_, slot) using the
                    // actual slot position of each static-key entry so that LGM(slot) →
                    // frame.group[slot] resolves to the correct thunk.
                    self.enter_scope_with_slot_pairs(&scope_pairs);
                    // Set ScopeFrame on the Dict AST node after entering scope.
                    if let SurfaceExpression::Dict { scope_frame, .. } = &arc.expr {
                        let _ = scope_frame.set(std::sync::Arc::new(crate::ast::ScopeFrame::new(scope_pairs.len())));
                    }
                    // Advance accumulated_dict_offset by the total slot count (all entries
                    // + instance method slots) so nested dicts use the correct base.
                    self.accumulated_dict_offset = base_offset + slot_idx;
                }

                // Phase 1b: two-pass scan for typeclass method injection.
                //
                // Pass 1: assign class_ids to all ClassDecl nodes (including nested ones
                // inside namespace dicts like `File: [Readable: [class ...]]`).
                //
                // Pass 2: scan for InstanceDecl entries (named or anonymous), resolve
                // each instance's class_name to find the ClassDecl, and inject the
                // class's method names into the current scope as EffectPerform addresses.
                //
                // Method injection is INSTANCE-driven: the instance provides evidence
                // that the methods are callable from this scope. The class only needs to
                // be visible from the instance's scope — the caller sees the instance.

                // Pass 1: assign class_ids to ClassDecl entries.
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

                // Pass 2: scan for InstanceDecl entries and inject method names.
                for entry in entries.iter() {
                    if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                        if let crate::ast::SurfaceDeclaration::InstanceDecl { class_name, .. } =
                            decl.as_ref()
                        {
                            if let Some((_class_id, methods)) =
                                find_class_decl_in_entries(class_name, entries)
                            {
                                if let Some(scope) = self.scopes.last_mut() {
                                    for method_name in methods {
                                        let should_inject = match scope.get(&method_name) {
                                            None => true,
                                            Some(VarAddr::EffectPerform { .. }) => true,
                                            _ => false,
                                        };
                                        if should_inject {
                                            scope.insert(
                                                method_name.clone(),
                                                VarAddr::EffectPerform {
                                                    slot: 0, // placeholder — actual slot assigned by enter_scope_with_offset
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Walk key annotations INSIDE the letrec scope so that annotation
                // values can reference the dict's own entries (forward letrec refs).
                // E.g., `Int@[as-type: [fn [let t] t]  supertype: TypeNode.Bytes]: []`
                // — the annotation lambda `[fn [let t] t]` needs `t` resolved, and
                // `TypeNode.Bytes` needs `TypeNode` which is in this dict's scope.
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
                // Restore accumulated_dict_offset after exiting this dict's scope.
                // Sibling dicts (same level as this one) use the same base_offset.
                self.accumulated_dict_offset = base_offset;
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
                // Must happen BEFORE enter_param_scope so accumulated_dict_offset is correct.
                for (i, p) in params.iter().enumerate() {
                    p.node
                        .resolved_dispatch_slot
                        .set(self.accumulated_dict_offset + i as u32);
                }
                self.enter_param_scope(&param_names);
                // Set ScopeFrame on the Fn AST node after entering param scope.
                if let SurfaceExpression::Fn { scope_frame, .. } = &arc.expr {
                    let _ = scope_frame.set(std::sync::Arc::new(crate::ast::ScopeFrame::new(param_names.len())));
                }

                self.walk_surface_node(body);

                self.exit_scope();
            }

            SurfaceExpression::Sequential(exprs) => {
                let mut injected = 0usize;
                // Cumulative LGM slot offset for sequential scope injection within this
                // Sequential expression. Mirrors the same mechanism in walk_surface_document:
                // each body's sequential scope uses LGM slots starting at this offset so
                // that LetrecChainStep's accumulated group has non-overlapping indices.
                //
                // Body 0 starts at offset 0; body 1 starts at body-0's key count; etc.
                let mut sequential_offset: u32 = 0;
                for (i, e) in exprs.iter().enumerate() {
                    let is_last = i == exprs.len() - 1;

                    if !is_last {
                        if let SurfaceExpression::Dict {
                            entries,
                            scope_frame: _,
                        } = &e.expr
                        {
                            for entry in entries {
                                if let Some(key) = &entry.node.key {
                                    let is_static = matches!(
                                        &key.expr,
                                        SurfaceExpression::VarRef { escaped: false, .. }
                                    );
                                    if is_static {}
                                    self.walk_surface_node(key);
                                    if is_static {}
                                }
                            }

                            // Uniform slot assignment: assign sequential slots to ALL
                            // entries and collect (name, slot) pairs for static-key
                            // entries (same approach as the Dict arm).
                            let mut scope_pairs: Vec<(String, u32)> = Vec::new();
                            let total_slots;
                            {
                                let mut slot_idx: u32 = 0;
                                for entry in entries.iter() {
                                    if let Some(key) = &entry.node.key {
                                        match &key.expr {
                                            SurfaceExpression::StringLiteral {
                                                content, ..
                                            } => {
                                                scope_pairs.push((
                                                    content.clone(),
                                                    sequential_offset + slot_idx,
                                                ));
                                            }
                                            SurfaceExpression::VarRef {
                                                name,
                                                escaped: false,
                                                ..
                                            } => {
                                                scope_pairs.push((
                                                    name.clone(),
                                                    sequential_offset + slot_idx,
                                                ));
                                            }
                                            _ => {}
                                        }
                                    }
                                    let _ =
                                        entry.node.assigned_slot.set(sequential_offset + slot_idx);
                                    slot_idx += 1;
                                    if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                                        if let crate::ast::SurfaceDeclaration::InstanceDecl {
                                            arms,
                                            ..
                                        } = decl.as_ref()
                                        {
                                            for (_pattern, method_entries) in arms {
                                                for me in method_entries {
                                                    let method_name =
                                                        me.node.key.as_ref().and_then(|k| match &k
                                                            .expr
                                                        {
                                                            SurfaceExpression::StringLiteral {
                                                                content,
                                                                ..
                                                            } => Some(content.clone()),
                                                            SurfaceExpression::VarRef {
                                                                name,
                                                                ..
                                                            } => Some(name.clone()),
                                                            _ => None,
                                                        });
                                                    if let Some(name) = method_name {
                                                        if !scope_pairs
                                                            .iter()
                                                            .any(|(n, _)| n == &name)
                                                        {
                                                            scope_pairs.push((
                                                                name,
                                                                sequential_offset + slot_idx,
                                                            ));
                                                        }
                                                        let _ = me
                                                            .node
                                                            .assigned_slot
                                                            .set(sequential_offset + slot_idx);
                                                        slot_idx += 1;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                total_slots = slot_idx;
                            }

                            if scope_pairs.is_empty() {
                                // No static keys: no scope injection needed. Walk values normally.
                                for entry in entries {
                                    self.walk_surface_node(&entry.node.value);
                                }
                                // Still advance sequential_offset by total slots so nested
                                // dicts in subsequent bodies use the correct base.
                                sequential_offset += total_slots;
                            } else {
                                // Enter the per-body letrec scope with actual slot positions.
                                self.enter_scope_with_slot_pairs(&scope_pairs);

                                // Walk key annotations inside the letrec scope
                                // (same as the Dict arm — annotations can reference siblings).
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

                                // Walk each entry's VALUE.
                                // Set accumulated_dict_offset to sequential_offset + total_slots
                                // so that any nested dict literals inside this body's values see
                                // the correct base offset for their own letrec scopes.
                                let saved_seq_dict_offset = self.accumulated_dict_offset;
                                self.accumulated_dict_offset = sequential_offset + total_slots;

                                for entry in entries {
                                    self.walk_surface_node(&entry.node.value);
                                }

                                // Exit the per-body letrec scope and restore offset.
                                self.exit_scope();
                                self.accumulated_dict_offset = saved_seq_dict_offset;

                                // Enter the Sequential scope (injected) so subsequent
                                // bodies can reference this body's bindings.
                                // Use the same slot pairs so name→slot mapping is consistent.
                                self.enter_scope_with_slot_pairs(&scope_pairs);
                                sequential_offset += total_slots;
                                injected += 1;
                            }
                        } else if let Some((pairs, total)) = surface_node_slot_pairs(e) {
                            // Non-dict node that nonetheless produces static keys
                            // (currently none, but keep the original fallback so the
                            // scope injection path stays correct for future cases).
                            self.walk_surface_node(e);
                            if !pairs.is_empty() {
                                self.enter_scope_with_slot_pairs(&pairs);
                                sequential_offset += total;
                                injected += 1;
                            } else {
                                sequential_offset += total;
                            }
                        } else {
                            // Non-dict, non-key-producing intermediate body: walk normally.
                            self.walk_surface_node(e);
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
                        self.enter_param_scope(&bound_names);
                        // Set ScopeFrame on the MatchArm after entering param scope for bindings.
                        let _ = arm
                            .scope_frame
                            .set(std::sync::Arc::new(crate::ast::ScopeFrame::new(bound_names.len())));
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
            SurfaceExpression::Dict {
                entries,
                scope_frame: _,
            } => {
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
            SurfaceExpression::Dict {
                entries,
                scope_frame: _,
            } => {
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

    /// Walk a document, assigning cumulative LGM slots to each dict's entries.
    ///
    /// Each intermediate dict's entries are assigned absolute cumulative LGM slots so that
    /// cross-dict references within the document resolve directly to LGM(absolute_slot) without
    /// any runtime frame traversal. The cumulative offset starts at `initial_offset` (the number
    /// of root-scope entries already assigned by the caller) and increases by each dict's key count.
    ///
    fn walk_surface_document_with_offset(&mut self, doc: &SurfaceDocument, initial_offset: u32) {
        let mut injected = 0usize;
        let items: Vec<&SurfaceItem> = doc.items.iter().collect();
        let expr_count = items
            .iter()
            .filter(|i| matches!(i, SurfaceItem::Expr(_)))
            .count();
        let mut expr_idx = 0usize;
        // Cumulative LGM slot offset: starts at initial_offset (root-scope entries already
        // assigned), increases by each dict's key count. Ensures absolute slot uniqueness
        // across the document's accumulated_group.
        let mut cumulative_offset: u32 = initial_offset;

        for item in &items {
            match item {
                SurfaceItem::Expr(node) => {
                    let is_last_expr = expr_idx == expr_count - 1;
                    // Sync accumulated_dict_offset to the current cumulative offset so that
                    // when the Dict arm of walk_surface_expr processes this document dict, it
                    // uses cumulative_offset as the base for LGM slot assignment. At runtime,
                    // eval_dict_core is called with outer_frame.group containing exactly
                    // cumulative_offset entries (root entries + prior document dict thunks), so
                    // the dict's entries land at indices cumulative_offset..cumulative_offset+N.
                    self.accumulated_dict_offset = cumulative_offset;
                    self.walk_surface_node(node);
                    // accumulated_dict_offset is restored to cumulative_offset by the Dict arm's
                    // save/restore logic after it exits the dict's letrec scope.
                    if !is_last_expr {
                        // Use slot pairs from the Dict arm's walk (assigned_slot already
                        // set) so that scope name→slot mapping matches the actual slot
                        // positions, and cumulative_offset advances by total slots (all
                        // entries, not just static keys).
                        if let Some((pairs, total)) = surface_node_slot_pairs(node) {
                            if !pairs.is_empty() {
                                // Document-level sequential intermediate dict scope.
                                self.enter_scope_with_slot_pairs(&pairs);
                                cumulative_offset += total;
                                injected += 1;
                            } else {
                                // No static keys but entries exist: advance offset anyway.
                                cumulative_offset += total;
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

    fn finish(self) -> ResolutionTable {
        self.table
    }

    fn finish_with_errors(self) -> (ResolutionTable, Vec<Diagnostic>) {
        (self.table, self.diagnostics)
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
/// Accepts `initial_frames` from prior resolver runs as input to establish outer scopes.
/// Names not found in the scope stack have their OnceLock left unset (None) and are
/// returned as `Diagnostic` entries with `kind = "resolve-error"`. Only expression-position
/// VarRefs are reported — annotation type names, static dict keys, LetDecl binding names,
/// and class/instance method-name keys are suppressed (not runtime variable references).
///
/// Returns `(ResolutionTable, diagnostics)` where:
/// - `diagnostics`: unified bag of resolve diagnostics (errors only).
///   - `kind = "resolve-error"`, `level = Err`: undefined-variable VarRefs in expression position.
pub fn resolve_surface_document_inplace(
    doc: &crate::ast::SurfaceDocument,
    initial_frames: &[indexmap::IndexMap<String, u32>],
) -> (ResolutionTable, Vec<Diagnostic>) {
    let mut resolver = SurfaceResolver::new();

    // Seed from initial_frames (outermost first).
    // The frames contain LGM slot indices into the accumulated_group; their slots are the
    // root-scope entries (builtins and capabilities). Compute initial_offset = total root
    // slots so document dict entries start at the right cumulative index.
    let initial_offset: u32 = initial_frames
        .iter()
        .flat_map(|f| f.values())
        .copied()
        .max()
        .map(|m| m + 1)
        .unwrap_or(0);
    for frame in initial_frames {
        resolver.enter_scope_from_frame(frame);
    }

    resolver.walk_surface_document_with_offset(doc, initial_offset);

    // Exit seeded scopes
    for _ in initial_frames {
        resolver.exit_scope();
    }

    resolver.finish_with_errors()
}

/// Resolve a SurfaceDocument seeded with an env-dict name list.
///
/// This is the resolver entry point for the env-dict protocol (`builtin-eval` / T-1775).
/// Both this function and `resolve_surface_document_inplace` use `Dispatch(depth, slot)`
/// for all names. Env-dict names occupy the first `env_names.len()` Dispatch slots; document
/// dict entries follow at cumulative slots starting at `env_names.len()`.
///
/// At runtime, `eval_core_document_exprs` starts accumulated_group with the env-dict thunks
/// (slots 0..env_names.len()-1), then extends with each dict's entries at cumulative slots.
/// `Dispatch(depth, slot)` resolves to `group[slot]` directly — no frame traversal.
///
/// `env_names`: ordered list of in-scope names from the env-dict (insertion order from
/// the name-set passed to `builtin-resolve`).  The i-th name gets `Dispatch(0, root_group_len + i)`.
///
/// Returns `(ResolutionTable, diagnostics, unreferenced)`.  New frames (produced by the
/// document's own declarations) are discarded — callers accumulate bindings via the exports
/// dict returned by `builtin-eval`, not by querying resolver frames.
///
/// The third return value (`unreferenced`) lists env-dict names that were never referenced
/// by any VarRef during the document's resolution walk. Callers (e.g., `loader.llt`) use
/// this to emit domain-specific warnings (e.g., abandoned pipeline input).
///
/// `root_group_len`: the number of root-scope entries (builtins + capabilities) in
/// the runtime accumulated_group. Env-dict entries start at this offset so their
/// Dispatch(0, root_group_len + i) slots match the runtime group ordering.
pub fn resolve_surface_document_with_env_dict(
    doc: &crate::ast::SurfaceDocument,
    env_names: &[String],
    root_group_len: u32,
) -> (ResolutionTable, Vec<Diagnostic>, Vec<String>) {
    let mut resolver = SurfaceResolver::new();

    // Seed with Dispatch(0, root_group_len + i) for the i-th env-dict name.
    // This matches the runtime ordering: accumulated_group = [root_group, env_dict_thunks, ...]
    // so env-dict name i is at position root_group_len + i.
    //
    // Leading-dot (`.name`) uses "second occurrence" semantics — it finds the first
    // binding of `name` and skips it, returning the next one. This is independent of
    // scope kind and works correctly for env-dict names.
    //
    // Enter env-dict scope with offset starting at root_group_len.
    // Env-dict name i gets LGM(root_group_len + i) to match the runtime accumulated_group
    // where env-dict thunks follow root_group entries.
    resolver.enter_scope_with_offset(env_names, root_group_len);
    resolver.env_scope_depth = Some(resolver.scopes.len() - 1);

    // Walk the document with cumulative offset starting after root_group + env-dict slots.
    // Document dict entries start at LGM(root_group_len + env_names.len()) so they don't
    // collide with root_group slots or env-dict slots in the accumulated_group.
    let initial_offset = root_group_len + env_names.len() as u32;
    // walk_surface_document_with_offset is called for its side effects on
    // resolver.referenced_env_names; the returned scope frames are not needed here.
    resolver.walk_surface_document_with_offset(doc, initial_offset);

    // Compute unreferenced env names: names from env_names that were never resolved
    // by any VarRef during the walk. Callers use this for domain-specific warnings.
    let unreferenced: Vec<String> = env_names
        .iter()
        .filter(|n| !resolver.referenced_env_names.contains(*n))
        .cloned()
        .collect();

    // Exit the seeded scope.
    resolver.exit_scope();

    let (table, diagnostics) = resolver.finish_with_errors();
    (table, diagnostics, unreferenced)
}

/// Like `resolve_surface_document_with_env_dict`, but seeds the resolver with additional
/// scope frames before the env-dict scope. Each seed frame maps names to their actual
/// slot positions in the runtime accumulated group (root_group primitives, capabilities,
/// or other pre-env entries). The names in the seed frames are provided by the caller
/// and have no intrinsic meaning to the resolver — the resolver treats them as ordinary
/// scope entries at their given slots.
///
/// Used by `builtin-resolve` to include root_group entries in the resolver's scope at
/// their correct runtime slots, while keeping env-dict names at their own offset
/// starting at `root_group_len`.
pub fn resolve_surface_document_with_seed_frames(
    doc: &crate::ast::SurfaceDocument,
    seed_frames: &[indexmap::IndexMap<String, u32>],
    env_names: &[String],
    root_group_len: u32,
) -> (ResolutionTable, Vec<Diagnostic>, Vec<String>) {
    let mut resolver = SurfaceResolver::new();

    for frame in seed_frames {
        resolver.enter_scope_from_frame(frame);
    }

    resolver.enter_scope_with_offset(env_names, root_group_len);
    resolver.env_scope_depth = Some(resolver.scopes.len() - 1);

    let initial_offset = root_group_len + env_names.len() as u32;
    resolver.walk_surface_document_with_offset(doc, initial_offset);

    let unreferenced: Vec<String> = env_names
        .iter()
        .filter(|n| !resolver.referenced_env_names.contains(*n))
        .cloned()
        .collect();

    for _ in seed_frames {
        resolver.exit_scope();
    }
    resolver.exit_scope();

    let (table, diagnostics) = resolver.finish_with_errors();
    (table, diagnostics, unreferenced)
}

/// Variant of `resolve_surface_document_with_seed_frames` that injects
/// `VarAddr::EffectPerform` addresses for typeclass method names into the scope chain.
///
/// `class_methods` maps `method_name → class_decl_id` — derived from `Env.classes` after the
/// type checker has processed all `ClassDecl` entries in scope. These are injected into the
/// env-names scope so `resolve_name` finds them through the normal scope chain search.
///
/// Callers that do not have class information should use
/// `resolve_surface_document_with_seed_frames` instead.
pub fn resolve_surface_document_with_seed_frames_and_classes(
    doc: &crate::ast::SurfaceDocument,
    seed_frames: &[indexmap::IndexMap<String, u32>],
    env_names: &[String],
    root_group_len: u32,
    class_methods: std::collections::HashMap<String, u64>,
) -> (ResolutionTable, Vec<crate::error::Diagnostic>, Vec<String>) {
    let mut resolver = SurfaceResolver::new();

    for frame in seed_frames {
        resolver.enter_scope_from_frame(frame);
    }

    resolver.enter_scope_with_offset(env_names, root_group_len);
    resolver.inject_class_methods(&class_methods);
    resolver.env_scope_depth = Some(resolver.scopes.len() - 1);

    let initial_offset = root_group_len + env_names.len() as u32;
    resolver.walk_surface_document_with_offset(doc, initial_offset);

    let unreferenced: Vec<String> = env_names
        .iter()
        .filter(|n| !resolver.referenced_env_names.contains(*n))
        .cloned()
        .collect();

    for _ in seed_frames {
        resolver.exit_scope();
    }
    resolver.exit_scope();

    let (table, diagnostics) = resolver.finish_with_errors();
    (table, diagnostics, unreferenced)
}

/// Resolve all VarRef nodes in a SurfaceProgram and return a ResolutionTable.
///
/// Accepts `initial_frames` from prior resolver runs as input to establish outer scopes.
/// Pass `&[]` when there is no outer scope (e.g., standalone eval, tests).
///
pub fn resolve_surface_program(
    program: &SurfaceProgram,
    initial_frames: &[indexmap::IndexMap<String, u32>],
) -> ResolutionTable {
    let mut resolver = SurfaceResolver::new();

    // Compute initial_offset = total root-scope slots (builtins + capabilities).
    // Document dict entries start at this offset in the accumulated_group.
    let initial_offset: u32 = initial_frames
        .iter()
        .flat_map(|f| f.values())
        .copied()
        .max()
        .map(|m| m + 1)
        .unwrap_or(0);

    // Seed from initial_frames (outermost first)
    for frame in initial_frames {
        resolver.enter_scope_from_frame(frame);
    }

    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;
        resolver.walk_surface_document_with_offset(doc, initial_offset);
    }

    // Exit seeded scopes
    for _ in initial_frames {
        resolver.exit_scope();
    }

    resolver.finish()
}

/// Variant of `resolve_surface_program` that injects `VarAddr::EffectPerform` addresses
/// for typeclass method names into the scope chain.
///
/// `class_methods` maps `method_name → class_decl_id` — derived from `Env.classes` after
/// the type checker has processed all `ClassDecl` entries in scope. These are injected
/// directly into the topmost seed scope so that `resolve_name` finds them through the
/// normal scope chain search.
///
/// Callers that do not have class information should use `resolve_surface_program` instead.
pub fn resolve_surface_program_with_classes(
    program: &SurfaceProgram,
    initial_frames: &[indexmap::IndexMap<String, u32>],
    class_methods: std::collections::HashMap<String, u64>,
) -> ResolutionTable {
    let mut resolver = SurfaceResolver::new();

    let initial_offset: u32 = initial_frames
        .iter()
        .flat_map(|f| f.values())
        .copied()
        .max()
        .map(|m| m + 1)
        .unwrap_or(0);

    for frame in initial_frames {
        resolver.enter_scope_from_frame(frame);
    }
    resolver.inject_class_methods(&class_methods);

    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;
        resolver.walk_surface_document_with_offset(doc, initial_offset);
    }

    for _ in initial_frames {
        resolver.exit_scope();
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
        SurfaceExpression::Dict {
            entries,
            scope_frame: _,
        } => (surface_dict_static_keys(entries), span),
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
        SurfaceExpression::Dict {
            entries,
            scope_frame: _,
        } => {
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

/// Count the total number of slots consumed by entries in a dict.
///
/// Every entry gets one slot, plus InstanceDecl method keys each get an additional slot.
/// This is the total group-vec growth for uniform slot assignment.
fn surface_dict_total_slots(entries: &[Spanned<SurfaceEntry>]) -> u32 {
    let mut count: u32 = 0;
    for entry in entries {
        count += 1;
        if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
            if let crate::ast::SurfaceDeclaration::InstanceDecl { arms, .. } = decl.as_ref() {
                for (_pattern, method_entries) in arms {
                    for me in method_entries {
                        let has_key = me.node.key.as_ref().map_or(false, |k| {
                            matches!(
                                &k.expr,
                                SurfaceExpression::StringLiteral { .. }
                                    | SurfaceExpression::VarRef { .. }
                            )
                        });
                        if has_key {
                            count += 1;
                        }
                    }
                }
            }
        }
    }
    count
}

/// Extract static string-keyed names from an Arc<SurfaceNode> if it is a Dict.
/// Compute (name, assigned_slot) pairs and total slot count from a Dict node.
///
/// Returns `None` for non-Dict nodes. The pairs contain only static-key entries
/// (entries with StringLiteral or unescaped VarRef keys) mapped to their actual
/// assigned_slot values. The total is the full slot count (all entries + instance methods).
///
/// Requires that assigned_slot has already been set on entries (i.e., the Dict arm's
/// walk has already run).
fn surface_node_slot_pairs(node: &Arc<SurfaceNode>) -> Option<(Vec<(String, u32)>, u32)> {
    if let SurfaceExpression::Dict {
        entries,
        scope_frame: _,
    } = &node.expr
    {
        let total = surface_dict_total_slots(entries);
        let mut pairs = Vec::new();
        for entry in entries {
            if let Some(&slot) = entry.node.assigned_slot.get() {
                if let Some(key) = &entry.node.key {
                    match &key.expr {
                        SurfaceExpression::StringLiteral { content, .. } => {
                            pairs.push((content.clone(), slot));
                        }
                        SurfaceExpression::VarRef {
                            name,
                            escaped: false,
                            ..
                        } => {
                            pairs.push((name.clone(), slot));
                        }
                        _ => {}
                    }
                }
            }
            // InstanceDecl method slots
            if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                if let crate::ast::SurfaceDeclaration::InstanceDecl { arms, .. } = decl.as_ref() {
                    for (_pattern, method_entries) in arms {
                        for me in method_entries {
                            if let Some(&slot) = me.node.assigned_slot.get() {
                                let method_name =
                                    me.node.key.as_ref().and_then(|k| match &k.expr {
                                        SurfaceExpression::StringLiteral { content, .. } => {
                                            Some(content.clone())
                                        }
                                        SurfaceExpression::VarRef { name, .. } => {
                                            Some(name.clone())
                                        }
                                        _ => None,
                                    });
                                if let Some(name) = method_name {
                                    if !pairs.iter().any(|(n, _)| n == &name) {
                                        pairs.push((name, slot));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Some((pairs, total))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    // All prior tests used resolve_file(), Expr, File, and ast_convert::surface_program_to_file()
    // which were deleted in the runtime-v2 migration. Tests now use resolve_surface_program()
    // with the SurfaceProgram/SurfaceNode AST directly.
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
        let table = resolve_surface_program(&program, &[]);
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
            SurfaceExpression::Dict {
                entries,
                scope_frame: _,
            } => {
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
    /// Case arm bindings use enter_param_scope so they resolve to Dispatch(0, slot),
    /// where slot = accumulated_dict_offset + i. At runtime, arm bindings are appended
    /// to the group after definition_scope entries.
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
            "n is the first (and only) binding — Dispatch(0, 0) via enter_param_scope"
        );
    }

    /// Case arm bodies see the bindings declared in [let ...].
    /// Bare lowercase names in match arm patterns are now Pin (not Variable).
    /// To bind a variable, use [case [let n] pattern body] form.
    ///
    /// Case arm bindings resolve to Dispatch(0, accumulated_dict_offset + i).
    /// The arm body accesses `n` via the group which holds the bound thunk from pattern matching.
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
            // The case arm scope introduces n as Dispatch(0, 0) via enter_param_scope
            assert_eq!(
                addr,
                &VarAddr::Dispatch(0, 0),
                "n is Dispatch(0, 0) via enter_param_scope"
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
    /// returning the unreferenced env names (third return value).
    fn resolve_with_env_dict(src: &str, env_names: &[String]) -> Vec<String> {
        let output = crate::parser::parse(src, test_file(src)).expect("parse failed");
        let program = crate::desugar::desugar_program_full(&output.program);
        let program = crate::expand::expand_program(&program);
        let doc = &program.documents[0].node;
        let (_table, _diagnostics, unreferenced) =
            resolve_surface_document_with_env_dict(doc, env_names, 0);
        unreferenced
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

    // --- B-586: Nested dict LGM slot alignment ---

    /// A nested dict literal (dict as value of an outer dict entry) must use
    /// `accumulated_dict_offset` as its LGM base, not 0.
    ///
    /// With no initial frame (offset=0): `[x: 1  inner: [ref: $x]]`.
    ///   Outer dict: x→LGM(0), inner→LGM(1). accumulated_dict_offset advances to 2.
    ///   Inner dict: ref→LGM(2). $x in ref's value → LGM(0) (outer dict scope).
    ///
    /// At runtime with empty root group (as in unit tests, R=0):
    ///   outer eval_dict_core: group.len()=0, extended=[x_t, inner_t]. x at 0 ✓.
    ///   inner eval_dict_core: group.len()=2, extended=[x_t, inner_t, ref_t]. ref at 2 ✓.
    ///   $x → LGM(0) → group[0] = x_t ✓.
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

    /// Nested dict with a non-zero initial frame offset.
    ///
    /// Simulates the case where R root entries exist (e.g., builtins) before the document
    /// dict. With initial frame [{builtin-dict-get: 0}] (R=1): outer dict has offset 1.
    ///   Outer dict: x→Dispatch(0,1), inner→Dispatch(0,2). accumulated_dict_offset advances to 3.
    ///   Inner dict: ref→Dispatch(0,3). $x → Dispatch(1, 1).
    ///
    /// Before the fix, the Dict arm used enter_scope(&keys, Dict) with offset=0, giving
    ///   x→Dispatch(0,0), inner→Dispatch(0,1), ref→Dispatch(0,0). Dispatch(0,0) → group[0] = builtin-dict-get (wrong!).
    #[test]
    fn nested_dict_lgm_offset_nonzero_base() {
        // Seed with one initial frame entry to simulate R=1 root entries.
        let output =
            crate::parser::parse("[x: 1  inner: [ref: $x]]", test_file("")).expect("parse");
        let program = crate::desugar::desugar_surface_program(&output.program);
        let doc = &program.documents[0].node;

        // Simulate R=1: one root-scope entry named "builtin-dict-get" at slot 0.
        let mut root_frame: indexmap::IndexMap<String, u32> = indexmap::IndexMap::new();
        root_frame.insert("builtin-dict-get".to_string(), 0u32);
        let (table, _diags) = resolve_surface_document_inplace(doc, &[root_frame]);

        // Outer dict: x at Dispatch(_, 1), inner at Dispatch(_, 2) (offset=1 from root frame).
        let x_refs = find_varref_nodes(&program, "x");
        assert!(!x_refs.is_empty(), "expected VarRef for $x");
        let (x_id, _) = &x_refs[0];
        let x_addr = table.get(x_id).expect("$x should be resolved");
        assert_eq!(
            x_addr,
            &VarAddr::Dispatch(1, 1),
            "$x must be Dispatch(1, 1) — x is in the outer dict (one scope up from the reference site)"
        );
    }

    /// Nested dict inside a fn body intermediate dict value.
    ///
    /// `[fn [let x] [a: $x  nested: [c: $x]] ...]`
    /// Param x gets slot 0 (accumulated_dict_offset=0 at fn entry).
    /// Body dict entries start after params: a→Dispatch(0,1), nested→Dispatch(0,2).
    /// Inner dict: c→Dispatch(0,3). All $x refs have slot=0 but varying depth.
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
