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
    node_id, ResolutionTable, Spanned, SurfaceDeclaration, SurfaceDocument, SurfaceEntry,
    SurfaceExpression, SurfaceItem, SurfaceNode, SurfaceProgram,
};
use crate::error::{DiagnosticLevel, TypeDiagnostic};
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
/// Whether a scope frame is a letrec dict scope or not.
///
/// Used by `resolve_name_parent` to implement leading-dot (`.name`) parent-scope lookup:
/// the nearest `Dict` scope and all non-`Dict` scopes above it are skipped, and the search
/// starts from the scope immediately outside the skipped `Dict` scope.
#[derive(Clone, Copy, PartialEq)]
enum ScopeKind {
    /// A letrec dict scope — the scope created when entering a `[k1: v1  k2: v2 ...]` dict.
    /// Leading-dot `.name` skips to above the nearest Dict scope.
    Dict,
    /// Any other scope: fn params, let/case arm bindings, initial (root) frames.
    /// Leading-dot does not skip these when searching for the Dict boundary.
    Other,
}

/// Tracks one intermediate dict body inside a `SurfaceExpression::Sequential`
/// (or one function's parameter list) for lost-binding detection.
struct IntermediateBodyInfo {
    /// Index of the body that introduced these bindings (0-based within the Sequential).
    /// Only used when `is_param = false`: a binding is consumed only if referenced
    /// from a body with index strictly greater than `body_index`.
    body_index: usize,
    /// Absolute forward index of the scope frame in `self.scopes` that holds these
    /// bindings.  Computed as `self.scopes.len() - 1` immediately after `enter_scope`.
    /// Invariant: this index never changes once recorded — scopes pushed later go on top.
    scope_depth: usize,
    /// `(name, definition_span, consumed, referenced_by_final, references)` for each
    /// static binding introduced by this body.
    /// - `consumed`: true if referenced from ANY later body or the final expression.
    /// - `referenced_by_final`: true if referenced directly from the final expression
    ///   (current_body_index == usize::MAX at the time of reference).
    /// - `references`: `(body_index, name)` pairs of earlier-body bindings referenced
    ///   by THIS specific binding's own definition expression. Using `(body_index, name)`
    ///   rather than just `name` avoids shadowing bugs: when two bodies both define a
    ///   binding named `x`, BFS expansion uses the correct `x`'s refs, not both.
    bindings: Vec<(String, crate::ast::Span, bool, bool, Vec<(usize, String)>)>,
    /// True when this info tracks function parameters rather than intermediate dict bindings.
    /// For params, any reference from within the function body counts as consumption,
    /// regardless of `current_body_index` (since params are introduced "before" all bodies).
    is_param: bool,
}

struct SurfaceResolver {
    /// Each scope frame is an (IndexMap<String, u32>, ScopeKind) pair.
    /// The IndexMap maps name → actual slot index; ScopeKind marks whether this is
    /// a letrec dict scope (for leading-dot parent-scope resolution).
    scopes: Vec<(indexmap::IndexMap<String, u32>, ScopeKind)>,
    table: ResolutionTable,
    /// Diagnostics accumulated during the walk (errors and warnings unified).
    /// - `kind = "resolve-error"`, `level = Err`: undefined variable in expression position.
    ///   Populated only when suppress_depth == 0 (annotation, static key, declaration, and
    ///   method-name positions are suppressed — they are not runtime variable references).
    /// - `kind = "lost-binding"`, `level = Warn`: lost intermediate binding or unused param.
    /// - `kind = "abandoned-input"`, `level = Warn`: document never references pipeline input %.
    diagnostics: Vec<TypeDiagnostic>,
    /// > 0 when inside a context where unresolved VarRefs are not errors
    /// (annotation, static key, declaration position, etc.).
    suppress_depth: usize,
    /// Stack of in-progress intermediate bodies for lost-binding detection.
    /// Each entry tracks one intermediate body dict's bindings and whether each
    /// binding was consumed (referenced) from any later body.
    intermediate_bodies: Vec<IntermediateBodyInfo>,
    /// Index of the body currently being walked within the innermost Sequential.
    /// `usize::MAX` means "final expression" — references during the final expression
    /// count as consumption for any enclosing intermediate body.
    current_body_index: usize,
    /// Whether the pipeline input `%` was referenced during this document's walk.
    /// Used for abandoned-input detection: if `%` was in the env but never resolved
    /// by any VarRef, the document ignores its pipeline input.
    percent_referenced: bool,
    /// T-1743: Index into `self.intermediate_bodies` of the IntermediateBodyInfo for
    /// the body currently being walked, paired with the name of the specific binding
    /// whose value expression is being walked. Set by the Sequential handler before
    /// walking each dict entry's value; cleared (None) between entries.
    /// When set, cross-body references are recorded into THAT binding's per-binding
    /// reference list rather than a shared body-level accumulator.
    /// None when not inside a per-binding value walk.
    current_binding_context: Option<(usize, String)>,
}

impl SurfaceResolver {
    fn new() -> Self {
        Self {
            scopes: Vec::new(),
            table: ResolutionTable::new(),
            diagnostics: Vec::new(),
            suppress_depth: 0,
            intermediate_bodies: Vec::new(),
            current_body_index: usize::MAX,
            percent_referenced: false,
            current_binding_context: None,
        }
    }

    fn enter_scope(&mut self, keys: &[String], kind: ScopeKind) {
        let mut scope: indexmap::IndexMap<String, u32> =
            indexmap::IndexMap::with_capacity(keys.len());
        for (slot, key) in keys.iter().enumerate() {
            scope.insert(key.clone(), slot as u32);
        }
        self.scopes.push((scope, kind));
    }

    fn enter_scope_from_frame(&mut self, frame: &indexmap::IndexMap<String, u32>) {
        // Initial frames (root builtins, capabilities, external frames) are Other —
        // they are not letrec dict scopes and leading-dot does not skip them.
        self.scopes.push((frame.clone(), ScopeKind::Other));
    }

    fn exit_scope(&mut self) {
        self.scopes
            .pop()
            .expect("exit_scope called with empty stack");
    }

    fn resolve_name(&mut self, name: &str) -> Option<(u32, u32)> {
        for (offset, (scope, _)) in self.scopes.iter().rev().enumerate() {
            if let Some(&slot) = scope.get(name) {
                let level = u32::try_from(offset).expect("scope depth overflow");
                // Track pipeline input % reference for abandoned-input detection.
                if name == "%" {
                    self.percent_referenced = true;
                }
                // Lost-binding detection: if this name resolves to an intermediate body's
                // scope AND we are currently walking a later body (or the final expression),
                // mark that binding as consumed.
                //
                // `match_depth` is the absolute forward index of the matched scope frame
                // in `self.scopes`.  It equals `self.scopes.len() - 1 - offset` and is
                // invariant even as more scopes are pushed on top later.
                let match_depth = self.scopes.len().saturating_sub(1 + offset);
                for info in &mut self.intermediate_bodies {
                    if info.scope_depth == match_depth {
                        // The match is in this intermediate body's scope.
                        // For parameter scopes (is_param=true): any reference from within
                        // the function body counts, so mark consumed unconditionally.
                        // For intermediate dict scopes (is_param=false): mark consumed only
                        // when the current walk position is AFTER the introducing body.
                        let should_consume =
                            info.is_param || self.current_body_index > info.body_index;
                        if should_consume {
                            let is_final = self.current_body_index == usize::MAX;
                            for (bname, _, consumed, ref_by_final, _) in &mut info.bindings {
                                if bname == name {
                                    *consumed = true;
                                    if is_final {
                                        *ref_by_final = true;
                                    }
                                }
                            }
                        }
                    }
                }
                // T-1743: Track cross-body references at binding granularity.
                // If we are currently walking a non-final body AND walking a specific
                // binding's value (current_binding_context is set), record the referenced
                // name into THAT binding's per-binding reference list. This ensures
                // only the binding that actually uses an earlier-body name is linked,
                // not every binding in the same body.
                if self.current_body_index != usize::MAX {
                    if let Some((info_idx, cur_binding)) = self.current_binding_context.clone() {
                        // Find the earlier-body binding's (body_index, name) pair so the
                        // BFS key is precise. Two bodies may both define a binding named `x`;
                        // using (body_index, name) avoids expanding refs from the wrong `x`.
                        let ref_key: Option<(usize, String)> = self
                            .intermediate_bodies
                            .iter()
                            .find(|info| {
                                info.scope_depth == match_depth
                                    && !info.is_param
                                    && info.body_index < self.current_body_index
                                    && info
                                        .bindings
                                        .iter()
                                        .any(|(bname, _, _, _, _)| bname == name)
                            })
                            .map(|info| (info.body_index, name.to_string()));
                        if let Some(key) = ref_key {
                            // Use info_idx directly (no search by body_index) to avoid
                            // ambiguity with nested Sequentials at the same body index.
                            if let Some(info) = self.intermediate_bodies.get_mut(info_idx) {
                                for (bname, _, _, _, refs) in &mut info.bindings {
                                    if bname == &cur_binding {
                                        refs.push(key);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                return Some((level, slot));
            }
        }
        None
    }

    /// Resolve `name` starting from the parent scope — skipping the nearest Dict scope
    /// and all non-Dict scopes above it (between the current position and the Dict scope).
    ///
    /// This implements leading-dot `.name` semantics: the current letrec dict's own scope
    /// is bypassed so that `.name` refers to the binding in the enclosing scope, not the
    /// dict entry being defined.
    ///
    /// Examples (scopes listed innermost-first):
    /// - `[dict_scope, fn_params]`: skip dict_scope → search fn_params ✓
    /// - `[fn_scope, dict_scope, outer]`: skip fn_scope (Other), skip dict_scope (Dict) → search outer ✓
    /// - `[inner_dict, outer_dict, fn_params]`: skip inner_dict (Dict) → search outer_dict ✓
    fn resolve_name_parent(&mut self, name: &str) -> Option<(u32, u32)> {
        let mut passed_dict = false;
        for (offset, (scope, kind)) in self.scopes.iter().rev().enumerate() {
            if !passed_dict {
                if *kind == ScopeKind::Dict {
                    passed_dict = true;
                }
                continue; // skip everything up to and including the nearest Dict scope
            }
            if let Some(&slot) = scope.get(name) {
                let level = u32::try_from(offset).expect("scope depth overflow");
                // Track pipeline input % reference for abandoned-input detection.
                if name == "%" {
                    self.percent_referenced = true;
                }
                // Also track consumption for leading-dot resolved names.
                let match_depth = self.scopes.len().saturating_sub(1 + offset);
                for info in &mut self.intermediate_bodies {
                    if info.scope_depth == match_depth {
                        let should_consume =
                            info.is_param || self.current_body_index > info.body_index;
                        if should_consume {
                            let is_final = self.current_body_index == usize::MAX;
                            for (bname, _, consumed, ref_by_final, _) in &mut info.bindings {
                                if bname == name {
                                    *consumed = true;
                                    if is_final {
                                        *ref_by_final = true;
                                    }
                                }
                            }
                        }
                    }
                }
                // T-1743: Track cross-body references at binding granularity (leading-dot path).
                if self.current_body_index != usize::MAX {
                    if let Some((info_idx, cur_binding)) = self.current_binding_context.clone() {
                        let ref_key: Option<(usize, String)> = self
                            .intermediate_bodies
                            .iter()
                            .find(|info| {
                                info.scope_depth == match_depth
                                    && !info.is_param
                                    && info.body_index < self.current_body_index
                                    && info
                                        .bindings
                                        .iter()
                                        .any(|(bname, _, _, _, _)| bname == name)
                            })
                            .map(|info| (info.body_index, name.to_string()));
                        if let Some(key) = ref_key {
                            if let Some(info) = self.intermediate_bodies.get_mut(info_idx) {
                                for (bname, _, _, _, refs) in &mut info.bindings {
                                    if bname == &cur_binding {
                                        refs.push(key);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
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
            SurfaceExpression::VarRef {
                name, resolution, ..
            } => {
                if let Some(coords) = self.resolve_name(name) {
                    resolution.set(Some(coords));
                    self.table.insert(node_id(arc), coords);
                } else {
                    // Name not in scope. Always set Some(None) so consumers can
                    // distinguish "not found" from "resolver never ran" (None = bug).
                    resolution.set(None);
                    if self.suppress_depth == 0 {
                        // Genuinely unresolved expression VarRef. suppress_depth > 0 when
                        // in annotation, static dict-key, LetDecl binding, or declaration
                        // method-name position — none of those are runtime variable references.
                        self.diagnostics.push(TypeDiagnostic::error(
                            "resolve-error",
                            format!("undefined variable: {}", name),
                            arc.span.clone(),
                        ));
                    }
                }
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

                self.enter_scope(&static_keys, ScopeKind::Dict);
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
                            self.suppress_depth += 1;
                            self.walk_surface_annotation(ann);
                            self.suppress_depth -= 1;
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
                // Walk param annotations in outer scope
                for param in params {
                    if let Some(ann) = &param.node.annotation {
                        self.walk_surface_annotation(ann);
                    }
                }
                let param_names: Vec<String> = params.iter().map(|p| p.node.name.clone()).collect();
                self.enter_scope(&param_names, ScopeKind::Other);

                // Lost-binding detection: track each parameter as a binding that must be
                // consumed somewhere in the function body.  The scope_depth is recorded
                // AFTER enter_scope so it is the absolute forward index of the param scope.
                let param_scope_depth = self.scopes.len() - 1;
                let param_bindings: Vec<(
                    String,
                    crate::ast::Span,
                    bool,
                    bool,
                    Vec<(usize, String)>,
                )> = params
                    .iter()
                    .map(|p| {
                        // Use the SurfaceNode span of the param node (the [let x] binding).
                        // `p` is a `Spanned<SurfaceParam>`; p.span is the param span.
                        // Parameters don't reference earlier intermediate bodies — the
                        // per-binding references Vec is always empty for params.
                        (
                            p.node.name.clone(),
                            p.span.clone(),
                            false,
                            false,
                            Vec::new(),
                        )
                    })
                    .collect();
                let prev_body_index = self.current_body_index;
                self.current_body_index = usize::MAX; // body is "after" all param bindings
                let param_info_idx = self.intermediate_bodies.len();
                self.intermediate_bodies.push(IntermediateBodyInfo {
                    body_index: 0,
                    scope_depth: param_scope_depth,
                    bindings: param_bindings,
                    is_param: true,
                });

                self.walk_surface_node(body);

                // Pop the param entry we pushed and emit warnings for unused params.
                // The pop is balanced: walk_surface_node(body) restores all nested
                // pushes/pops before returning.
                debug_assert_eq!(
                    self.intermediate_bodies.len(),
                    param_info_idx + 1,
                    "intermediate_bodies stack unbalanced after fn body walk"
                );
                let info = self
                    .intermediate_bodies
                    .pop()
                    .expect("param info must still be on stack");
                for (name, span, consumed, _, _) in info.bindings {
                    if !consumed {
                        self.diagnostics.push(TypeDiagnostic {
                            level: DiagnosticLevel::Warn,
                            kind: "lost-binding",
                            message: format!(
                                "parameter '{}' is never referenced in the function body",
                                name
                            ),
                            spans: vec![(span, String::new())],
                            notes: vec![],
                        });
                    }
                }
                self.current_body_index = prev_body_index;

                self.exit_scope();
            }

            SurfaceExpression::Sequential(exprs) => {
                let prev_body_index = self.current_body_index;
                let prev_binding_context = self.current_binding_context.take();
                let intermediate_bodies_base = self.intermediate_bodies.len();
                let mut injected = 0usize;
                for (i, e) in exprs.iter().enumerate() {
                    let is_last = i == exprs.len() - 1;
                    // Set current_body_index so resolve_name knows which body we are in.
                    self.current_body_index = if is_last { usize::MAX } else { i };

                    if !is_last {
                        if let SurfaceExpression::Dict(entries) = &e.expr {
                            // T-1743: Walk intermediate dict bodies entry-by-entry so that
                            // cross-body references are attributed to the specific binding
                            // whose value expression uses the earlier-body name, not to
                            // every binding in the body (the body-level bug).
                            //
                            // The key invariant: IntermediateBodyInfo for body i must be
                            // pushed into self.intermediate_bodies BEFORE walking body i's
                            // values so that resolve_name (called during the value walk) can
                            // find it by body_index and store per-binding references into it.

                            // Step 1: Walk key expressions in outer scope (suppressed for
                            // non-escaped static keys — same as the Dict arm logic).
                            for entry in entries {
                                if let Some(key) = &entry.node.key {
                                    let is_static = matches!(
                                        &key.expr,
                                        SurfaceExpression::VarRef { escaped: false, .. }
                                    );
                                    if is_static {
                                        self.suppress_depth += 1;
                                    }
                                    self.walk_surface_node(key);
                                    if is_static {
                                        self.suppress_depth -= 1;
                                    }
                                }
                            }

                            // Use the full surface_dict_static_keys (including ClassDecl method
                            // injection) for scope entry — same as the Dict arm — so that
                            // the scope frame matches what the evaluator sees.
                            let all_keys = surface_dict_static_keys(entries);
                            if all_keys.is_empty() {
                                // No static keys: no scope injection needed. Walk values normally
                                // (no per-binding tracking since no bindings are introduced).
                                // Still need to walk the key annotations inside the (empty) scope.
                                for entry in entries {
                                    self.walk_surface_node(&entry.node.value);
                                }
                            } else {
                                // Step 2: Enter the Dict letrec scope (for value walks to
                                // reference sibling entries — same as the Dict arm behavior).
                                self.enter_scope(&all_keys, ScopeKind::Dict);

                                // Step 3: Build binding_list for lost-binding tracking.
                                let named_entries: Vec<(String, crate::ast::Span)> =
                                    surface_dict_keys_with_spans(entries);
                                let binding_list: Vec<(
                                    String,
                                    crate::ast::Span,
                                    bool,
                                    bool,
                                    Vec<(usize, String)>,
                                )> = named_entries
                                    .iter()
                                    .map(|(name, span)| {
                                        (name.clone(), span.clone(), false, false, Vec::new())
                                    })
                                    .collect();
                                let has_tracked_bindings = !binding_list.is_empty();
                                // Push IntermediateBodyInfo BEFORE walking values so that
                                // resolve_name can find it by body_index and store per-binding
                                // refs into it during the value walk.
                                // Use a placeholder scope_depth (the Dict letrec scope index).
                                // We will update it to the Sequential scope index after step 7.
                                let info_idx = self.intermediate_bodies.len();
                                if has_tracked_bindings {
                                    self.intermediate_bodies.push(IntermediateBodyInfo {
                                        body_index: i,
                                        // Placeholder: will be updated to Sequential scope depth
                                        // after entering the Sequential scope in step 7.
                                        scope_depth: usize::MAX,
                                        bindings: binding_list,
                                        is_param: false,
                                    });
                                }

                                // Step 4: Walk key annotations inside the letrec scope
                                // (same as the Dict arm — annotations can reference siblings).
                                for entry in entries {
                                    if let Some(key) = &entry.node.key {
                                        if let SurfaceExpression::VarRef {
                                            annotation: Some(ann),
                                            ..
                                        } = &key.expr
                                        {
                                            self.suppress_depth += 1;
                                            self.walk_surface_annotation(ann);
                                            self.suppress_depth -= 1;
                                        }
                                    }
                                }

                                // Step 5: Walk each entry's VALUE.
                                // For tracked bindings, set current_binding_context so
                                // resolve_name can record per-binding cross-body references.
                                // For untracked bindings (positional entries, class/instance
                                // decl entries), walk normally with context = None.
                                for entry in entries {
                                    let binding_name: Option<String> =
                                        entry.node.key.as_ref().and_then(|key| match &key.expr {
                                            SurfaceExpression::VarRef {
                                                name,
                                                escaped: false,
                                                ..
                                            } if has_tracked_bindings => {
                                                // Only set if this name is in the tracked binding list.
                                                self.intermediate_bodies
                                                    .get(info_idx)
                                                    .filter(|info| {
                                                        info.bindings
                                                            .iter()
                                                            .any(|(bn, _, _, _, _)| bn == name)
                                                    })
                                                    .map(|_| name.clone())
                                            }
                                            SurfaceExpression::StringLiteral {
                                                content, ..
                                            } if has_tracked_bindings => self
                                                .intermediate_bodies
                                                .get(info_idx)
                                                .filter(|info| {
                                                    info.bindings
                                                        .iter()
                                                        .any(|(bn, _, _, _, _)| bn == content)
                                                })
                                                .map(|_| content.clone()),
                                            _ => None,
                                        });
                                    // Set per-binding context (None for untracked entries).
                                    self.current_binding_context =
                                        binding_name.map(|name| (info_idx, name));
                                    self.walk_surface_node(&entry.node.value);
                                }
                                // Clear binding context after walking all entries.
                                self.current_binding_context = None;

                                // Step 6: Exit the Dict letrec scope.
                                self.exit_scope();

                                // Step 7: Enter the Sequential scope (injected) so subsequent
                                // bodies can reference this body's bindings. Record scope_depth
                                // from this scope — this is what resolve_name uses to identify
                                // which body a referenced name belongs to.
                                self.enter_scope(&all_keys, ScopeKind::Dict);
                                let sequential_scope_depth = self.scopes.len() - 1;
                                if has_tracked_bindings {
                                    // Update the scope_depth placeholder now that we have
                                    // the correct Sequential scope index.
                                    self.intermediate_bodies[info_idx].scope_depth =
                                        sequential_scope_depth;
                                }
                                injected += 1;
                            }
                        } else if let Some(keys) = surface_node_static_keys(e) {
                            // Non-dict node that nonetheless produces static keys
                            // (currently none, but keep the original fallback so the
                            // scope injection path stays correct for future cases).
                            self.walk_surface_node(e);
                            if !keys.is_empty() {
                                self.enter_scope(&keys, ScopeKind::Dict);
                                injected += 1;
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
                // T-1743: Transitive reachability analysis for lost-binding detection.
                // This subsumes the direct T-1740 check: a binding is reachable if and
                // only if the final expression (or a chain of bindings starting from
                // the final expression) references it.
                //
                // 1. Seed the reachable set with bindings directly referenced by the final
                //    expression (referenced_by_final == true).
                // 2. BFS: for each reachable binding X, add only X's own per-binding references.
                //    (Not all references of X's body — that was the body-level bug.)
                // 3. Any binding NOT in the reachable set gets a lost-binding warning.
                let drained: Vec<IntermediateBodyInfo> = self
                    .intermediate_bodies
                    .drain(intermediate_bodies_base..)
                    .collect();

                // Collect all binding names and their per-binding info for reachability analysis.
                // Each entry: ((body_index, name), span, per-binding-references).
                // Using (body_index, name) as the unique key prevents the shadowing bug:
                // when two bodies both define a binding named `x`, BFS expansion uses the
                // correct `x`'s per-binding refs rather than merging refs from all `x`s.
                let mut all_bindings: Vec<(
                    (usize, String),
                    crate::ast::Span,
                    Vec<(usize, String)>,
                )> = Vec::new();
                let mut reachable: std::collections::HashSet<(usize, String)> =
                    std::collections::HashSet::new();

                for info in &drained {
                    for (name, span, _, ref_by_final, per_binding_refs) in &info.bindings {
                        let key = (info.body_index, name.clone());
                        // Per-binding references: only what THIS binding's value uses.
                        all_bindings.push((key.clone(), span.clone(), per_binding_refs.clone()));
                        if *ref_by_final {
                            reachable.insert(key);
                        }
                    }
                }

                // BFS: for each reachable (body_index, name) pair, expand using ONLY its own
                // per-binding refs. Because each binding is keyed by (body_index, name), two
                // bindings with the same name in different bodies are treated independently.
                let mut queue: std::collections::VecDeque<(usize, String)> =
                    reachable.iter().cloned().collect();
                while let Some(key) = queue.pop_front() {
                    for (bkey, _, refs) in &all_bindings {
                        if bkey == &key {
                            for referenced in refs {
                                if reachable.insert(referenced.clone()) {
                                    queue.push_back(referenced.clone());
                                }
                            }
                            // Each (body_index, name) pair is unique — no need to continue.
                            break;
                        }
                    }
                }

                // Emit warnings for bindings not in the reachable set.
                for (key, span, _) in &all_bindings {
                    if !reachable.contains(key) {
                        self.diagnostics.push(TypeDiagnostic {
                            level: DiagnosticLevel::Warn,
                            kind: "lost-binding",
                            message: format!(
                                "intermediate binding '{}' is defined but never referenced from a later body — its value is lost in tinct's lazy evaluation",
                                key.1
                            ),
                            spans: vec![(span.clone(), String::new())],
                            notes: vec![],
                        });
                    }
                }

                self.current_binding_context = prev_binding_context;
                self.current_body_index = prev_body_index;
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
                    // Leading-dot `.name`: look up in the PARENT scope, skipping the
                    // nearest enclosing letrec dict scope. This prevents `[k: .k ...]`
                    // from creating a circular self-reference.
                    // `.a.b.c` chains work correctly: only this innermost `expr: None`
                    // case uses parent lookup; outer `.b` / `.c` use normal field-get.
                    if let Some(coords) = self.resolve_name_parent(name) {
                        let _ = resolution.set(Some(coords));
                    } else {
                        // Resolver ran but name not found in parent — emit error node.
                        let _ = resolution.set(None);
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
            SurfaceExpression::Quote(_) => {}

            SurfaceExpression::Unquote(inner) | SurfaceExpression::UnquoteSplice(inner) => {
                self.walk_surface_node(inner);
            }

            SurfaceExpression::Match { scrutinee, arms } => {
                self.walk_surface_node(scrutinee);
                for arm in arms {
                    // Walk the pattern as a SurfaceNode with suppress_depth incremented.
                    // This prevents undefined-variable errors for VarRefs in pattern position —
                    // unresolved VarRefs get Some(None), which eval treats as "arm does not match".
                    // CaseArm patterns are handled separately below (they have their own scope).
                    self.suppress_depth += 1;
                    self.walk_surface_node(&arm.pattern);
                    self.suppress_depth -= 1;
                    // Match arm patterns introduce NO new bindings to arm scope.
                    // Only [case [let names] ...] form introduces bindings.
                    if let Some(guard) = &arm.guard {
                        self.walk_surface_node(guard);
                    }
                    for body_expr in &arm.body {
                        self.walk_surface_node(body_expr);
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
                                // `_` is a wildcard, not a binding — exclude it so the pattern
                                // position VarRef for `_` remains unresolved (Some(None)), which
                                // eval treats as wildcard rather than a pin.
                                if name == "_" {
                                    None
                                } else {
                                    Some(name.clone())
                                }
                            } else {
                                None
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let has_bindings = !bound_names.is_empty();
                if has_bindings {
                    self.enter_scope(&bound_names, ScopeKind::Other);
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

            // Terminals with no child expressions
            SurfaceExpression::Int(_)
            | SurfaceExpression::U64(_)
            | SurfaceExpression::Float(_)
            | SurfaceExpression::StringLiteral { .. }
            | SurfaceExpression::Placeholder(..)
            | SurfaceExpression::Error(_) => {}
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
    /// Walking the type structure would generate false "undefined variable" errors for
    /// constructor names and type parameters. We surgically walk ONLY annotation values,
    /// at the current suppress_depth (no blanket suppression), so genuine errors in
    /// annotation bodies ARE reported while the type structure is left untouched.
    fn walk_type_alias_body(&mut self, body: &Arc<SurfaceNode>) {
        match &body.expr {
            SurfaceExpression::Dict(entries) => {
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
                name, annotation, ..
            } if crate::eval::is_constructor_name(name) => {
                if let Some(ann) = annotation {
                    self.walk_ctor_annotation_values(ann);
                }
            }
            // Bracket form: `[Dict@[as-type: ...] fields: ...]` or `[Int@[...]]`
            // The parser wraps annotated constructors in a bracket (a single-entry Dict).
            // Recurse through it to find the actual VarRef or Call inside.
            SurfaceExpression::Dict(entries) => {
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
                            self.walk_ctor_annotation_values(ann);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Walk PropertyDict annotation VALUES as runtime expressions, without adding
    /// suppress_depth. This is the key difference from walk_surface_annotation: annotation
    /// values on constructors ARE runtime closures (stored via builtin-make-annotated),
    /// so undefined names in them must produce errors, not be silently swallowed.
    fn walk_ctor_annotation_values(&mut self, ann: &Spanned<crate::ast::Annotation>) {
        if let crate::ast::Annotation::PropertyDict(entries) = &ann.node {
            for entry in entries {
                // Keys are static strings — no resolution needed.
                // Values are runtime expressions: walk them at current suppress_depth
                // so the Fn handler can enter parameter scopes and resolve body VarRefs.
                self.walk_surface_node(&entry.node.value);
            }
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

    fn walk_surface_document(
        &mut self,
        doc: &SurfaceDocument,
    ) -> Vec<indexmap::IndexMap<String, u32>> {
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
                                self.enter_scope(&keys, ScopeKind::Dict);
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

        // Capture injected frames (strip ScopeKind — callers only need name→slot maps)
        let start = self.scopes.len() - injected;
        let frames: Vec<_> = self.scopes[start..]
            .iter()
            .map(|(m, _)| m.clone())
            .collect();

        for _ in 0..injected {
            self.exit_scope();
        }

        frames
    }

    fn finish(self) -> ResolutionTable {
        self.table
    }

    fn finish_with_errors(self) -> (ResolutionTable, Vec<TypeDiagnostic>) {
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
/// returned as `TypeDiagnostic` entries with `kind = "resolve-error"`. Only expression-position
/// VarRefs are reported — annotation type names, static dict keys, LetDecl binding names,
/// and class/instance method-name keys are suppressed (not runtime variable references).
///
/// Returns `(ResolutionTable, diagnostics, new_frames)` where:
/// - `diagnostics`: unified bag of resolve diagnostics (errors and warnings).
///   - `kind = "resolve-error"`, `level = Err`: undefined-variable VarRefs in expression position.
///   - `kind = "lost-binding"`, `level = Warn`: lost intermediate bindings and unused parameters.
///   - `kind = "abandoned-input"`, `level = Warn`: document never references pipeline input %.
/// - `new_frames`: scope frames ADDED by this document (not including `initial_frames`).
pub fn resolve_surface_document_inplace(
    doc: &crate::ast::SurfaceDocument,
    initial_frames: &[indexmap::IndexMap<String, u32>],
) -> (
    ResolutionTable,
    Vec<TypeDiagnostic>,
    Vec<indexmap::IndexMap<String, u32>>,
) {
    let mut resolver = SurfaceResolver::new();

    // Seed from initial_frames (outermost first)
    for frame in initial_frames {
        resolver.enter_scope_from_frame(frame);
    }

    let new_frames = resolver.walk_surface_document(doc);

    // T-1741: Abandoned pipeline input % detection.
    // If `%` was provided in the initial_frames (meaning this is not the first
    // document in the pipeline) but was never referenced by any VarRef during
    // resolution, the document ignores its pipeline input — emit a warning.
    if !resolver.percent_referenced {
        let percent_in_env = initial_frames.iter().any(|frame| frame.contains_key("%"));
        if percent_in_env {
            // Use the first item's span for the warning, or a synthetic span.
            let span = doc
                .items
                .first()
                .map(|item| item.span())
                .unwrap_or_else(|| crate::rust_span!());
            resolver.diagnostics.push(TypeDiagnostic {
                level: DiagnosticLevel::Warn,
                kind: "abandoned-input",
                message: "document does not reference pipeline input %".to_string(),
                spans: vec![(span, String::new())],
                notes: vec![],
            });
        }
    }

    // Exit seeded scopes
    for _ in initial_frames {
        resolver.exit_scope();
    }

    let (table, diagnostics) = resolver.finish_with_errors();
    (table, diagnostics, new_frames)
}

/// Resolve all VarRef nodes in a SurfaceProgram and return a ResolutionTable.
///
/// Accepts `initial_frames` from prior resolver runs as input to establish outer scopes.
/// Pass `&[]` when there is no outer scope (e.g., standalone eval, tests).
///
/// Returns `(ResolutionTable, new_frames)` where `new_frames` are the scope frames
/// ADDED by this program (not including `initial_frames`).
pub fn resolve_surface_program(
    program: &SurfaceProgram,
    initial_frames: &[indexmap::IndexMap<String, u32>],
) -> (ResolutionTable, Vec<indexmap::IndexMap<String, u32>>) {
    let mut resolver = SurfaceResolver::new();

    // Seed from initial_frames (outermost first)
    for frame in initial_frames {
        resolver.enter_scope_from_frame(frame);
    }

    let mut new_frames = Vec::new();
    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;
        let doc_frames = resolver.walk_surface_document(doc);
        new_frames.extend(doc_frames);
    }

    // Exit seeded scopes
    for _ in initial_frames {
        resolver.exit_scope();
    }

    let table = resolver.finish();
    (table, new_frames)
}

/// T-1742: Extract the static produced keys of a document's final expression.
///
/// For a document whose last expression is a Dict (`[k1: v1  k2: v2]`), returns
/// the list of static string-keyed names that this stage "produces" (makes available
/// to the next pipeline document via `%`). Returns an empty Vec if the final expression
/// is not a Dict or has no static keys.
///
/// Also returns the span of the final expression for use in pipeline lint warnings.
pub fn collect_document_produced_keys(
    doc: &crate::ast::SurfaceDocument,
) -> (Vec<String>, crate::ast::Span) {
    // Find the last Expr item.
    let last_expr = doc.items.iter().rev().find_map(|item| match item {
        SurfaceItem::Expr(node) => Some(node),
        SurfaceItem::Decl(_) => None,
    });
    let Some(expr_node) = last_expr else {
        return (Vec::new(), crate::rust_span!());
    };
    let span = expr_node.span.clone();
    match &expr_node.expr {
        SurfaceExpression::Dict(entries) => (surface_dict_static_keys(entries), span),
        _ => (Vec::new(), span),
    }
}

/// T-1742: Cross-document pipeline lint.
///
/// After resolving all documents in a pipeline, checks whether keys produced by
/// non-final stages are consumed by the subsequent document. Keys produced but
/// not consumed generate `abandoned-output` warnings.
///
/// If the subsequent document uses dynamic access to `%` (e.g. `[get key %]`
/// with a variable key), the warning is suppressed for that stage because the
/// accessed keys cannot be statically determined.
///
/// `stages` is a slice of `(produced_keys, percent_field_accesses, uses_dynamic_percent)`:
/// - `produced_keys`: static key names from the stage's final expression.
/// - `percent_field_accesses`: static `%.key` field access names from the stage's document.
/// - `uses_dynamic_percent`: whether the stage uses `%` in a non-field context (dynamic access).
///
/// Returns a vec of `TypeDiagnostic` warnings for abandoned outputs.
pub fn lint_pipeline_stages(
    stages: &[(
        Vec<String>,      // produced keys
        Vec<String>,      // percent field accesses from next doc
        bool,             // next doc uses dynamic percent access
        crate::ast::Span, // span of the producing document's final expression
    )],
) -> Vec<TypeDiagnostic> {
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
                diagnostics.push(TypeDiagnostic {
                    level: DiagnosticLevel::Warn,
                    kind: "abandoned-output",
                    message: format!(
                        "key '{}' is produced but never consumed by the next pipeline stage",
                        key
                    ),
                    spans: vec![(span.clone(), String::new())],
                    notes: vec![],
                });
            }
        }
    }
    diagnostics
}

/// Collect all static `%.key` field accesses from a document.
///
/// Walks the document's AST and returns the set of string field names accessed
/// on `%` (the pipeline input). Also returns whether `%` is used in a non-field
/// context (e.g., passed to a function, used as a match scrutinee), which
/// indicates dynamic access that prevents static key analysis.
pub fn collect_percent_accesses(doc: &crate::ast::SurfaceDocument) -> (Vec<String>, bool) {
    let mut field_accesses = Vec::new();
    let mut dynamic_use = false;
    for item in &doc.items {
        if let SurfaceItem::Expr(node) = item {
            collect_percent_accesses_node(node, &mut field_accesses, &mut dynamic_use);
        }
    }
    (field_accesses, dynamic_use)
}

/// Recursive helper: collect %.key accesses and detect dynamic % usage.
fn collect_percent_accesses_node(
    node: &Arc<SurfaceNode>,
    accesses: &mut Vec<String>,
    dynamic_use: &mut bool,
) {
    match &node.expr {
        SurfaceExpression::Field {
            expr: Some(target),
            field,
            ..
        } => {
            // Check if target is VarRef("%")
            if matches!(&target.expr, SurfaceExpression::VarRef { name, .. } if name == "%") {
                // %.key — target is VarRef("%"), no need to recurse into it.
                // For Ident keys: record the key name as a consumed pipeline key.
                // For Int keys: % used as an indexed sequence — no named key to record.
                if let crate::ast::DotKey::Ident(key) = field {
                    accesses.push(key.clone());
                }
                // DotKey::Int: % used as an indexed sequence — treat as dynamic access
                // because the pipeline lint operates on named keys only.
                else {
                    *dynamic_use = true;
                }
            } else {
                collect_percent_accesses_node(target, accesses, dynamic_use);
            }
        }
        SurfaceExpression::VarRef { name, .. } if name == "%" => {
            // % used in a non-field context — dynamic access
            *dynamic_use = true;
        }
        // Recurse into child expressions
        SurfaceExpression::Dict(entries) => {
            for entry in entries {
                if let Some(key) = &entry.node.key {
                    collect_percent_accesses_node(key, accesses, dynamic_use);
                }
                collect_percent_accesses_node(&entry.node.value, accesses, dynamic_use);
            }
        }
        SurfaceExpression::Fn { body, .. } => {
            collect_percent_accesses_node(body, accesses, dynamic_use);
        }
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } => {
            collect_percent_accesses_node(func, accesses, dynamic_use);
            for arg in args {
                collect_percent_accesses_node(arg, accesses, dynamic_use);
            }
            for na in named_args {
                collect_percent_accesses_node(&na.node.value, accesses, dynamic_use);
            }
        }
        SurfaceExpression::Sequential(exprs) => {
            for e in exprs {
                collect_percent_accesses_node(e, accesses, dynamic_use);
            }
        }
        SurfaceExpression::Pipe { lhs, rhs, .. } => {
            collect_percent_accesses_node(lhs, accesses, dynamic_use);
            collect_percent_accesses_node(rhs, accesses, dynamic_use);
        }
        SurfaceExpression::TypeAssert { expr, .. } => {
            collect_percent_accesses_node(expr, accesses, dynamic_use);
        }
        SurfaceExpression::Match { scrutinee, arms } => {
            collect_percent_accesses_node(scrutinee, accesses, dynamic_use);
            for arm in arms {
                collect_percent_accesses_node(&arm.pattern, accesses, dynamic_use);
                if let Some(guard) = &arm.guard {
                    collect_percent_accesses_node(guard, accesses, dynamic_use);
                }
                for body_expr in &arm.body {
                    collect_percent_accesses_node(body_expr, accesses, dynamic_use);
                }
            }
        }
        SurfaceExpression::CaseArm { pattern, body, .. } => {
            collect_percent_accesses_node(pattern, accesses, dynamic_use);
            collect_percent_accesses_node(body, accesses, dynamic_use);
        }
        SurfaceExpression::Unquote(inner) | SurfaceExpression::UnquoteSplice(inner) => {
            collect_percent_accesses_node(inner, accesses, dynamic_use);
        }
        _ => {}
    }
}

/// Extract static string-keyed names from a SurfaceExpression::Dict's entries.
///
/// Handles two cases that the lowerer also handles, so the resolver's letrec scope matches
/// the evaluator's letrec environment exactly:
///
/// 1. Keyed entries — VarRef or string literal keys become static scope slots.
/// 2. Anonymous InstanceDecl entries (no outer key) — the lowerer flattens these into
///    ɪ-prefixed binding names (`ɪɴꜱᴛᴀɴᴄᴇ⧼Class∷method⟨T⟩⧽`) in the outer dict.
///    We register those same names so the resolver can find instance methods directly
///    when they are referenced from within the same letrec scope.
fn surface_dict_static_keys(entries: &[Spanned<SurfaceEntry>]) -> Vec<String> {
    // Pass 1: Collect all explicitly-named keys from non-ClassDecl entries.
    // This lets us avoid injecting a class method name that shadows an explicit
    // user binding (e.g. `=: [fn [let x y] Boolean.False]` in the same dict).
    let explicit_keys: std::collections::HashSet<String> = entries
        .iter()
        .filter_map(|entry| {
            let key_node = entry.node.key.as_ref()?;
            // Skip ClassDecl entries — their outer key is not an "explicit" binding
            // that should block method name injection.
            let is_class_decl = matches!(
                &entry.node.value.expr,
                SurfaceExpression::Decl(d) if matches!(d.as_ref(), crate::ast::SurfaceDeclaration::ClassDecl { .. })
            );
            if is_class_decl {
                return None;
            }
            match &key_node.expr {
                SurfaceExpression::VarRef {
                    name,
                    escaped: false,
                    ..
                } => Some(name.clone()),
                SurfaceExpression::StringLiteral { content, .. } => Some(content.clone()),
                _ => None,
            }
        })
        .collect();

    // Pass 2: Build the key list. ClassDecl method names come BEFORE the class
    // outer name so the lowerer's slot layout matches (methods first, then class).
    let mut keys = Vec::new();
    for entry in entries {
        if let Some(key_node) = &entry.node.key {
            // Check if this keyed entry is a ClassDecl.
            if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                if let crate::ast::SurfaceDeclaration::ClassDecl { methods, .. } = decl.as_ref() {
                    // Inject method names that are NOT explicitly defined elsewhere
                    // in this dict. This makes `+`, `<`, etc. resolvable as bindings.
                    for me in methods {
                        let method_name = match me.node.key.as_ref() {
                            Some(k) => match &k.expr {
                                SurfaceExpression::StringLiteral { content, .. } => content.clone(),
                                SurfaceExpression::VarRef { name, .. } => name.clone(),
                                _ => continue,
                            },
                            None => continue,
                        };
                        if !explicit_keys.contains(&method_name) {
                            keys.push(method_name);
                        }
                    }
                    // Then push the class outer name itself.
                    match &key_node.expr {
                        SurfaceExpression::StringLiteral { content, .. } => {
                            keys.push(content.clone());
                        }
                        SurfaceExpression::VarRef {
                            name,
                            escaped: false,
                            ..
                        } => {
                            keys.push(name.clone());
                        }
                        _ => {}
                    }
                    continue;
                }
            }
            // Non-ClassDecl keyed entry: push the key name (existing behavior).
            match &key_node.expr {
                SurfaceExpression::StringLiteral { content, .. } => keys.push(content.clone()),
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
                                SurfaceExpression::StringLiteral { content, .. } => content.clone(),
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

/// Extract static string-keyed names WITH their definition spans from a Dict node's entries.
///
/// Returns only the simple (non-ClassDecl, non-InstanceDecl) keyed entries that produce
/// direct letrec scope bindings — the same set that `surface_dict_static_keys` emits for
/// non-class entries.  Class method injections and instance method injections are excluded
/// because their slots are implementation details, not user-written bindings.
fn surface_dict_keys_with_spans(
    entries: &[Spanned<SurfaceEntry>],
) -> Vec<(String, crate::ast::Span)> {
    let mut result = Vec::new();
    for entry in entries {
        let key_node = match &entry.node.key {
            Some(k) => k,
            None => continue,
        };
        // Skip ClassDecl and InstanceDecl entries — their method injections are
        // implementation details, not plain user bindings.
        let is_special_decl = matches!(
            &entry.node.value.expr,
            SurfaceExpression::Decl(d)
                if matches!(
                    d.as_ref(),
                    crate::ast::SurfaceDeclaration::ClassDecl { .. }
                    | crate::ast::SurfaceDeclaration::InstanceDecl { .. }
                )
        );
        if is_special_decl {
            continue;
        }
        match &key_node.expr {
            SurfaceExpression::StringLiteral { content, .. } => {
                result.push((content.clone(), key_node.span.clone()));
            }
            SurfaceExpression::VarRef {
                name,
                escaped: false,
                ..
            } => {
                result.push((name.clone(), key_node.span.clone()));
            }
            _ => {}
        }
    }
    result
}

#[cfg(test)]
mod tests {
    // All prior tests used resolve_file(), Expr, File, and ast_convert::surface_program_to_file()
    // which were deleted in the runtime-v2 migration. Tests now use resolve_surface_program()
    // with the SurfaceProgram/SurfaceNode AST directly.
    use super::*;
    use crate::ast::{node_id, NodeId, SurfaceExpression};

    fn test_file(src: &str) -> Arc<crate::ast::SourceFile> {
        Arc::new(crate::ast::SourceFile {
            path: Arc::from(file!()),
            content: Arc::from(src),
        })
    }

    /// Parse `src`, desugar, and resolve. Returns (program, table).
    fn parse_and_resolve(src: &str) -> (crate::ast::SurfaceProgram, ResolutionTable) {
        let output = crate::parser::parse(src, test_file(src)).expect("parse failed");
        let mut program = output.program;
        crate::desugar::desugar_surface_program(&mut program);
        // No runtime env in unit tests — dict-internal and lexical references still
        // resolve via the resolver's scope tracking; env-provided names (builtins) use None.
        let (table, _frames) = resolve_surface_program(&program, &[]);
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
                    if let Some(guard) = &arm.guard {
                        collect_varrefs_in_node(guard, name, out);
                    }
                    for body_expr in &arm.body {
                        collect_varrefs_in_node(body_expr, name, out);
                    }
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
        let src = "[x: 1  result: [match $x Int: [+ $x 1] ...: 0]]";
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

    /// Helper: parse a single document and resolve with given initial frames.
    /// Returns (program, diagnostics).
    fn parse_and_resolve_doc_with_frames(
        src: &str,
        initial_frames: &[indexmap::IndexMap<String, u32>],
    ) -> (crate::ast::SurfaceProgram, Vec<TypeDiagnostic>) {
        let output = crate::parser::parse(src, test_file(src)).expect("parse failed");
        let mut program = output.program;
        crate::desugar::desugar_surface_program(&mut program);
        let doc = &program.documents[0].node;
        let (_, diagnostics, _) = resolve_surface_document_inplace(doc, initial_frames);
        (program, diagnostics)
    }

    // --- T-1741: Abandoned pipeline input % ---

    /// T-1741: When % is in the env but the document never references it,
    /// emit an abandoned-input warning.
    #[test]
    fn abandoned_input_warns_when_percent_unused() {
        let mut frame = indexmap::IndexMap::new();
        frame.insert("%".to_string(), 0u32);
        let (_, diagnostics) = parse_and_resolve_doc_with_frames("[+ 1 2]", &[frame]);
        let abandoned = diagnostics
            .iter()
            .filter(|d| d.kind == "abandoned-input")
            .count();
        assert_eq!(
            abandoned, 1,
            "expected 1 abandoned-input warning when % is unused"
        );
    }

    /// T-1741: When % is in the env and the document DOES reference it,
    /// no abandoned-input warning.
    #[test]
    fn abandoned_input_no_warn_when_percent_used() {
        let mut frame = indexmap::IndexMap::new();
        frame.insert("%".to_string(), 0u32);
        // $% is an escaped VarRef that references %
        let (_, diagnostics) = parse_and_resolve_doc_with_frames("$%", &[frame]);
        let abandoned = diagnostics
            .iter()
            .filter(|d| d.kind == "abandoned-input")
            .count();
        assert_eq!(
            abandoned, 0,
            "expected no abandoned-input warning when % is used"
        );
    }

    /// T-1741: When % is NOT in the env (first document in pipeline),
    /// no abandoned-input warning regardless of usage.
    #[test]
    fn abandoned_input_no_warn_when_percent_not_in_env() {
        let (_, diagnostics) = parse_and_resolve_doc_with_frames("[+ 1 2]", &[]);
        let abandoned = diagnostics
            .iter()
            .filter(|d| d.kind == "abandoned-input")
            .count();
        assert_eq!(
            abandoned, 0,
            "expected no abandoned-input warning when % is not in env"
        );
    }

    // --- T-1743: Transitive lost-binding detection ---

    /// Helper: parse and resolve, returning only the lost-binding diagnostics.
    fn lost_binding_diagnostics(src: &str) -> Vec<TypeDiagnostic> {
        let output = crate::parser::parse(src, test_file(src)).expect("parse failed");
        let mut program = output.program;
        crate::desugar::desugar_surface_program(&mut program);
        let (_, _frames) = resolve_surface_program(&program, &[]);
        // resolve_surface_program doesn't return diagnostics, so use per-doc resolve.
        let doc = &program.documents[0].node;
        let (_, diagnostics, _) = resolve_surface_document_inplace(doc, &[]);
        diagnostics
            .into_iter()
            .filter(|d| d.kind == "lost-binding")
            .collect()
    }

    /// T-1743: Both a and b are lost when b references a but neither
    /// is referenced from the final expression.
    #[test]
    fn lost_binding_transitive_both_lost() {
        // [fn [let x] [a: [+ x 1]] [b: [+ $a 2]] 42]
        // a and b are both intermediate bindings; final expression is 42.
        let diagnostics = lost_binding_diagnostics("[fn [let x] [a: [+ $x 1]] [b: [+ $a 2]] 42]");
        let lost_names: Vec<&str> = diagnostics
            .iter()
            .filter_map(|d| {
                d.message
                    .strip_prefix("intermediate binding '")
                    .and_then(|s| s.split('\'').next())
            })
            .collect();
        assert!(
            lost_names.contains(&"a"),
            "expected lost-binding warning for 'a', got: {:?}",
            lost_names
        );
        assert!(
            lost_names.contains(&"b"),
            "expected lost-binding warning for 'b', got: {:?}",
            lost_names
        );
    }

    /// T-1743: When b references a and the final expression references b,
    /// both are transitively reachable — no warnings.
    #[test]
    fn lost_binding_transitive_chain_consumed() {
        let diagnostics = lost_binding_diagnostics("[fn [let x] [a: [+ $x 1]] [b: [+ $a 2]] $b]");
        assert!(
            diagnostics.is_empty(),
            "expected no lost-binding warnings when chain is consumed, got: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    /// T-1743: When b references a but only a is referenced from the final
    /// expression, b is lost but a is reachable.
    #[test]
    fn lost_binding_transitive_partial_reachability() {
        let diagnostics = lost_binding_diagnostics("[fn [let x] [a: [+ $x 1]] [b: [+ $a 2]] $a]");
        let lost_names: Vec<&str> = diagnostics
            .iter()
            .filter_map(|d| {
                d.message
                    .strip_prefix("intermediate binding '")
                    .and_then(|s| s.split('\'').next())
            })
            .collect();
        assert!(
            lost_names.contains(&"b"),
            "expected lost-binding warning for 'b', got: {:?}",
            lost_names
        );
        assert!(
            !lost_names.contains(&"a"),
            "expected NO lost-binding warning for 'a' (directly referenced), got: {:?}",
            lost_names
        );
    }

    /// T-1743: Multi-binding-per-body — only the binding that uses an earlier-body name
    /// is linked to it in the BFS; the other binding in the same body is NOT transitively
    /// linked.
    ///
    /// Structure: fn with 3 bodies:
    ///   body 0: [c_val: 99]
    ///   body 1: [a: [+ x 1]   b: [+ x c_val]]   ← a is pure, b refs c_val from body 0
    ///   final:  a                                 ← only a is directly referenced
    ///
    /// BFS (correct): reachable = {a}; a's per-binding refs = [] (a doesn't use c_val);
    ///   c_val is never reached → warning for c_val. b is not reachable → warning for b.
    ///
    /// BFS (old buggy body-level): reachable = {a}; body 1's shared refs = [c_val];
    ///   c_val would be incorrectly marked reachable → missing warning (false negative).
    #[test]
    fn lost_binding_multi_binding_per_body_granularity() {
        // body 0: c_val; body 1: a (pure), b (uses c_val); final: $a
        let diagnostics =
            lost_binding_diagnostics("[fn [let x] [c_val: 99] [a: [+ $x 1]  b: [+ $x $c_val]] $a]");
        let lost_names: Vec<&str> = diagnostics
            .iter()
            .filter_map(|d| {
                d.message
                    .strip_prefix("intermediate binding '")
                    .and_then(|s| s.split('\'').next())
            })
            .collect();
        // b is not reachable: final refs a, a refs nothing from body 0 or 1.
        assert!(
            lost_names.contains(&"b"),
            "expected lost-binding warning for 'b' (not reachable), got: {:?}",
            lost_names
        );
        // c_val is not reachable: only b uses it, and b is not reachable.
        assert!(
            lost_names.contains(&"c_val"),
            "expected lost-binding warning for 'c_val' (only b uses it, b is not reachable), got: {:?}",
            lost_names
        );
        // a IS reachable: directly referenced from the final expression.
        assert!(
            !lost_names.contains(&"a"),
            "expected NO lost-binding warning for 'a' (directly referenced), got: {:?}",
            lost_names
        );
    }

    /// T-1743: Shadowing bug — two bodies define a binding with the same name.
    ///
    /// When body 2 defines `a` that is reachable (final refs it), and body 1 also defines
    /// `a` (shadowed by body 2's `a`), the BFS must NOT expand refs from body 1's `a` when
    /// processing body 2's `a`. Using (body_index, name) as the unique key prevents this.
    ///
    /// Structure:
    ///   body 0: [c: 99]
    ///   body 1: [a: $c]          ← a references c from body 0 (cross-body ref)
    ///   body 2: [a: [+ $p 1]]    ← a shadows body 1's a; no cross-body refs
    ///   final:  $a               ← resolves to body 2's a (innermost)
    ///
    /// Correct (new BFS): reachable = {(2,a)}; (2,a)'s refs = []; c and (1,a) not reachable → WARN.
    /// Bug (old BFS by name): dequeues "a", finds both body 1's AND body 2's `a`.
    ///   Expands body 1's `a`'s refs = [(0,"c")] → c incorrectly marked reachable → false negative.
    #[test]
    fn lost_binding_shadowing_body_index_key() {
        // Three bodies: body 0 has c; body 1 has a (uses c); body 2 has a (shadows, pure).
        // Final uses body 2's a. body 1's a and body 0's c should BOTH warn.
        let diagnostics = lost_binding_diagnostics("[fn [let p] [c: 99] [a: $c] [a: [+ $p 1]] $a]");
        let lost_names: Vec<&str> = diagnostics
            .iter()
            .filter_map(|d| {
                d.message
                    .strip_prefix("intermediate binding '")
                    .and_then(|s| s.split('\'').next())
            })
            .collect();
        // body 0's c is NOT reachable: only body 1's a (not reachable) uses it.
        // Bug: old BFS would incorrectly mark c as reachable via body 2's a finding body 1's a's refs.
        assert!(
            lost_names.contains(&"c"),
            "expected lost-binding warning for 'c' (only body 1's a uses it, body 1's a is not reachable), got: {:?}",
            lost_names
        );
        // body 1's a is not reachable: final resolved to body 2's a (innermost scope wins).
        assert!(
            lost_names.contains(&"a"),
            "expected lost-binding warning for body 1's 'a' (shadowed by body 2's a), got: {:?}",
            lost_names
        );
        // body 2's a IS reachable: final expression directly references it.
        // Since both body 1's and body 2's a would warn for 'a', check that body 2's a is NOT warned.
        // We can verify this indirectly: exactly one 'a' warning (body 1's), not two.
        let a_count = lost_names.iter().filter(|&&n| n == "a").count();
        assert_eq!(
            a_count, 1,
            "expected exactly 1 warning for 'a' (body 1 only, not body 2's which is reachable), got: {:?}",
            lost_names
        );
    }

    // --- T-1742: Pipeline stage lint unit tests ---

    /// T-1742: All keys produced by stage are consumed — no warnings.
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

    /// T-1742: Stage produces a key that the next stage does not consume — warning fires.
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

    /// T-1742: Dynamic % access (uses_dynamic = true) suppresses all warnings.
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

    /// T-1742: Pass-through pattern — next stage consumes a superset of produced keys.
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
}
