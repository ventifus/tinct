//! Variable resolution pass: assigns VarAddr closure-converted addresses to VarRef nodes.
//!
//! This is Phase 1 of the closure-conversion strategy. The resolver walks the AST and
//! assigns VarAddr values to static variable references before evaluation begins:
//!
//! - `LetrecGroupMember(slot)` — reference to `accumulated_group[slot]`. Slot is an
//!   ABSOLUTE cumulative index: root-scope entries occupy slots 0..N-1 (from
//!   `enter_scope_from_frame`), and each document dict's entries follow at cumulative
//!   offsets assigned by `walk_surface_document_with_offset`. Cross-dict references
//!   within a document use LGM with the absolute slot — no runtime frame traversal needed.
//! - `Parameter(i)` — reference to the i-th parameter of the enclosing function.
//! - `ClosureCapture(i)` — fn capture: a free variable referenced inside a fn body from
//!   an outer scope. `i` is the index into the function's capture list (`resolved_captures`).
//!   Exclusively emitted when inside a fn boundary — never for cross-dict references alone.
//!
//! For each `SurfaceExpression::Fn` node, the resolver also sets `resolved_captures`:
//! an ordered list of `(name, original_addr)` pairs in first-occurrence order, where
//! `original_addr` is the VarAddr the captured binding held in the ENCLOSING frame
//! (before the resolver converted it to `ClosureCapture` for uses inside the function).
//! The evaluator uses `original_addr` at function-definition time to look up each
//! captured thunk in the accumulated_group (frame.group[slot]) and build the function's
//! `closure_env`. LGM(slot) original_addrs index frame.group directly.
//!
//! **Invariants:**
//! - Must run exactly once per AST (write-once OnceLock).
//! - Must run after desugaring (sees $_ as Fn nodes, not VarRef("_")).
//! - Must run before typechecking and evaluation (both consumers of resolved coords).
//!
//! See doc/whatif/arena-patterns.md §Variable Resolution Pass Design for the full specification.

use crate::ast::{
    class_decl_name, node_id, ResolutionTable, Spanned, SurfaceDeclaration, SurfaceDocument,
    SurfaceEntry, SurfaceExpression, SurfaceItem, SurfaceNode, SurfaceProgram, VarAddr,
};
use crate::error::TypeDiagnostic;
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

/// Whether a scope frame is a letrec dict scope or not.
///
/// Used by `resolve_name_parent` to implement leading-dot (`.name`) parent-scope lookup:
/// the nearest `Dict` scope and all non-`Dict` scopes above it are skipped, and the search
/// starts from the scope immediately outside the skipped `Dict` scope.
///
/// Also used to determine whether a scope corresponds to a real eval_dict_core frame boundary
/// at runtime. Only `Dict` scopes correspond to real runtime frames. `BlockBody` injection
/// scopes (both document-level and function-body sequential bodies) do not create separate frames.
#[derive(Clone, Copy, PartialEq)]
enum ScopeKind {
    /// A letrec dict scope — the scope created when entering a `[k1: v1  k2: v2 ...]` dict
    /// in a standalone or fn-body context. Corresponds to a real runtime eval_dict_core frame.
    /// Leading-dot `.name` skips to above the nearest Dict scope.
    Dict,
    /// Sequential block-body injection scope. Used for intermediate dict bodies in any
    /// sequential context — document-level or function-body. Both are semantically identical:
    /// inner scope sees outer, only the final expression is returned. NOT a separate runtime
    /// frame; entries accumulate into the enclosing context's group at their cumulative slots.
    /// These frames are collected into `SurfaceResolver::block_body_frames` so the type
    /// checker can always find slot bases via `resolver_frames` without context-specific logic.
    BlockBody,
    /// Any other scope: fn params, let/case arm bindings, root frames, env-dict scopes.
    /// Leading-dot does not skip these when searching for the Dict boundary.
    Other,
}

struct SurfaceResolver {
    /// Each scope frame is an (IndexMap<String, VarAddr>, ScopeKind) pair.
    /// The IndexMap maps name → VarAddr; ScopeKind marks whether this is
    /// a letrec dict scope (for leading-dot parent-scope resolution).
    scopes: Vec<(indexmap::IndexMap<String, VarAddr>, ScopeKind)>,
    table: ResolutionTable,
    /// Diagnostics accumulated during the walk (errors and warnings unified).
    /// - `kind = "resolve-error"`, `level = Err`: undefined variable in expression position.
    ///   Populated only when suppress_depth == 0 (annotation, static key, declaration, and
    ///   method-name positions are suppressed — they are not runtime variable references).
    diagnostics: Vec<TypeDiagnostic>,
    /// > 0 when inside a context where unresolved VarRefs are not errors
    /// > (annotation, static key, declaration position, etc.).
    suppress_depth: usize,
    /// The name of the field-access primitive function in the runtime's root group.
    /// Used by the Field arm to resolve dot-access expressions. Stored here rather than
    /// hardcoded in the arm so the resolver remains agnostic to the specific name used by
    /// any given prelude/loader stack. The default matches the standard builtin registration.
    field_getter_name: Arc<str>,
    /// Names from the env-dict (name-set) that were referenced during this document's walk.
    /// Used to compute unreferenced env names returned by resolve_surface_document_with_env_dict.
    referenced_env_names: std::collections::HashSet<String>,
    /// Depth of the env-dict scope in the scope stack (set by resolve_surface_document_with_env_dict).
    /// When set, only names resolved at this depth are recorded in referenced_env_names.
    env_scope_depth: Option<usize>,
    /// Stack of function boundaries, one entry per enclosing `Fn` node.
    /// Each entry is the value of `self.scopes.len()` at the moment the Fn was entered —
    /// i.e., the number of scope frames that belong to outer functions or document scope.
    /// Scopes at index < boundary are "outer" (free variables → ClosureCapture);
    /// scopes at index >= boundary are "local" (params, letrec members within this fn).
    fn_scope_boundaries: Vec<usize>,
    /// Per-function capture lists, parallel to `fn_scope_boundaries`.
    /// Each entry is the accumulating list of (name, original_addr) pairs for the corresponding Fn.
    /// Pairs are appended in first-occurrence order; the index within the list is the
    /// ClosureCapture index assigned to that name's VarRef nodes inside the function.
    /// `original_addr` is the VarAddr the binding held in the enclosing frame BEFORE the
    /// resolver converted it to ClosureCapture — the evaluator uses it to look up the thunk
    /// in the enclosing EvalFrame when building the function's closure_env at definition time.
    fn_capture_lists: Vec<Vec<(String, VarAddr)>>,
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
    /// All `BlockBody` sequential injection scope frames collected during the walk, in
    /// order of injection. Includes both document-level and function-body sequential bodies.
    /// Returned by `resolve_surface_program` as `resolver_frames` so the type checker can
    /// find the slot base for any sequential intermediate body without context-specific logic.
    block_body_frames: Vec<indexmap::IndexMap<String, u32>>,
}

impl SurfaceResolver {
    fn new() -> Self {
        Self {
            scopes: Vec::new(),
            table: ResolutionTable::new(),
            diagnostics: Vec::new(),
            suppress_depth: 0,
            referenced_env_names: std::collections::HashSet::new(),
            env_scope_depth: None,
            fn_scope_boundaries: Vec::new(),
            fn_capture_lists: Vec::new(),
            accumulated_dict_offset: 0,
            field_getter_name: Arc::from("builtin-dict-get"),
            block_body_frames: Vec::new(),
        }
    }

    /// Enter a letrec dict scope with a slot offset.
    /// Each key gets `LetrecGroupMember(offset + i)` as its VarAddr.
    /// Used by walk_surface_document to assign cumulative LGM indices to sequential
    /// dict scopes so that cross-document references don't collide with prior dicts.
    fn enter_scope_with_offset(&mut self, keys: &[String], offset: u32, kind: ScopeKind) {
        let mut scope: indexmap::IndexMap<String, VarAddr> =
            indexmap::IndexMap::with_capacity(keys.len());
        for (i, key) in keys.iter().enumerate() {
            // depth=0 is a placeholder; resolve_name overwrites depth with the actual
            // scope-stack traversal distance when the name is looked up.
            scope.insert(
                key.clone(),
                VarAddr::LetrecGroupMember {
                    depth: 0,
                    slot: offset + i as u32,
                },
            );
        }
        // Collect frame as name→slot map for resolver_frames output.
        let frame: indexmap::IndexMap<String, u32> = scope
            .iter()
            .filter_map(|(k, addr)| {
                if let VarAddr::LetrecGroupMember { slot, .. } = addr {
                    Some((k.clone(), *slot))
                } else {
                    None
                }
            })
            .collect();
        if kind == ScopeKind::BlockBody {
            // Collect BlockBody sequential injection frames so resolve_surface_program can
            // return them.
            self.block_body_frames.push(frame.clone());
        }
        self.scopes.push((scope, kind));
    }

    /// Enter a function parameter scope: each param gets `Parameter(i)` as its VarAddr.
    fn enter_param_scope(&mut self, params: &[String]) {
        let mut scope: indexmap::IndexMap<String, VarAddr> =
            indexmap::IndexMap::with_capacity(params.len());
        for (i, name) in params.iter().enumerate() {
            scope.insert(name.clone(), VarAddr::Parameter(i as u32));
        }
        self.scopes.push((scope, ScopeKind::Other));
    }

    /// Seed scope from an external frame (initial_frames from the loader).
    ///
    /// External frames carry u32 slot indices into the root group (accumulated_group).
    /// Root-scope names (builtins and capabilities) are assigned `LetrecGroupMember(slot)` —
    /// they occupy the first slots of the accumulated_group at runtime. Document dict entries
    /// follow at cumulative slot offsets above the root-scope slots.
    ///
    /// Initial frames (root builtins, capabilities, external frames) are `Other` —
    /// they are not letrec dict scopes and leading-dot does not skip them.
    fn enter_scope_from_frame(&mut self, frame: &indexmap::IndexMap<String, u32>) {
        let converted: indexmap::IndexMap<String, VarAddr> = frame
            .iter()
            // depth=0 placeholder; resolve_name overwrites depth with traversal offset at lookup.
            .map(|(k, &slot)| (k.clone(), VarAddr::LetrecGroupMember { depth: 0, slot }))
            .collect();
        self.scopes.push((converted, ScopeKind::Other));
    }

    fn exit_scope(&mut self) {
        self.scopes.pop().expect("scopes is empty");
    }

    /// Resolve `name` in the current scope stack, returning its `VarAddr`.
    ///
    /// All variable addressing uses exactly three variants (no OuterGroupRef):
    /// - `LetrecGroupMember(slot)` — absolute cumulative slot in the accumulated_group.
    ///   Root-scope entries occupy slots 0..N-1; document dict entries follow at cumulative
    ///   offsets. Cross-dict references (a name from dict_i referenced in dict_j where j>i)
    ///   resolve directly to LGM(slot) because walk_surface_document assigns cumulative slots
    ///   via enter_scope_with_offset, making each slot unique across the document.
    /// - `ClosureCapture(i)` — fn capture: a free variable in a fn body. The evaluator
    ///   builds closure_env at fn-creation time by looking up each capture's original_addr
    ///   in the enclosing EvalFrame (frame.group[slot] for LGM captures).
    /// - `Parameter(i)` — fn argument.
    ///
    /// If we are inside one or more functions (fn_scope_boundaries is non-empty) and the
    /// name is found in a scope frame that belongs to an outer function or document scope
    /// (i.e., the frame's absolute index < the innermost fn boundary), the name is a free
    /// variable. It is added to the current function's capture list (if not already present)
    /// and assigned `ClosureCapture(i)`.
    ///
    /// If the name is found in a local scope (within the current function), the stored
    /// VarAddr (Parameter(i) or LetrecGroupMember(i)) is returned directly.
    fn resolve_name(&mut self, name: &str) -> Option<VarAddr> {
        // Search from innermost scope outward.
        let scopes_len = self.scopes.len();
        let found = self
            .scopes
            .iter()
            .rev()
            .enumerate()
            .find_map(|(offset, (scope, _))| {
                scope.get(name).map(|addr| {
                    let frame_abs_idx = scopes_len.saturating_sub(1 + offset);
                    (frame_abs_idx, offset as u32, addr.clone())
                })
            });

        let (match_depth, traversal_offset, addr) = found?;
        // Inject the correct depth into LetrecGroupMember entries. The stored depth=0 is a
        // placeholder written at scope-construction time; the actual traversal offset is only
        // known at lookup time (here). The evaluator ignores depth and uses slot directly.
        let addr = match addr {
            VarAddr::LetrecGroupMember { slot, .. } => VarAddr::LetrecGroupMember {
                depth: traversal_offset,
                slot,
            },
            other => other,
        };

        if self.env_scope_depth.map_or(false, |d| match_depth == d) {
            self.referenced_env_names.insert(name.to_string());
        }

        // Closure conversion: if we are inside at least one function AND the resolved
        // scope frame belongs to an outer function or document scope, this is a free
        // variable → add to the innermost function's capture list.
        if let Some(&fn_boundary) = self.fn_scope_boundaries.last() {
            if match_depth < fn_boundary {
                // Free variable: found in a scope outside the current function boundary.
                //
                // Determine the original_addr to store in the captures list. This is the
                // VarAddr that the Fn arm uses at function-creation time to look up the thunk
                // from the enclosing EvalFrame.
                //
                // Capture inheritance: if the immediately enclosing fn already captured this
                // name, inherit its capture index as ClosureCapture(outer_idx). At inner fn
                // creation, frame.closure_env = outer fn's closure_env, so ClosureCapture(outer_idx)
                // correctly finds the thunk.
                //
                // For names not yet captured by any enclosing fn:
                //   - LGM(slot): the slot is an absolute cumulative index into accumulated_group.
                //     frame.group[slot] at fn creation time gives the thunk directly.
                //   - Parameter(i): pass through unchanged.
                //   - ClosureCapture(i): pass through unchanged (from a scope frame that's a
                //     FnSequentialBody scope carrying forwarded ClosureCapture addrs — rare).
                //
                // Multi-level cascade: for nested fns, walk from outermost to innermost fn,
                // cascading via ClosureCapture at each level so every intermediate fn gets the
                // name in its capture list (required for closure_env building at each level).
                let outer_capture_idx = self
                    .fn_capture_lists
                    .iter()
                    .rev()
                    .skip(1) // skip innermost (current fn being processed)
                    .next() // only check the immediately enclosing fn
                    .and_then(|captures| {
                        captures
                            .iter()
                            .position(|(n, _)| n == name)
                            .map(|pos| pos as u32)
                    });

                let original_addr = if let Some(outer_idx) = outer_capture_idx {
                    // Immediately enclosing fn already captured this name → inherit via closure_env.
                    VarAddr::ClosureCapture(outer_idx)
                } else {
                    // Not yet in any enclosing fn's capture list.
                    //
                    // addr is the VarAddr from the enclosing scope frame. With the new model:
                    //   - LGM(slot): absolute cumulative slot. Use directly — frame.group[slot].
                    //   - Parameter(i): use directly.
                    //   - ClosureCapture(i): use directly (scope frames from FnSequentialBody scopes
                    //     may carry these from an outer fn's body-sequential injection).
                    //
                    // Multi-level cascade for nested fns (fn_capture_lists.len() > 1):
                    // Process all enclosing fn levels from outermost to immediately enclosing.
                    // Each level either uses the name directly (from its frame) or inherits from
                    // the outer level via ClosureCapture.
                    let num_levels = self.fn_capture_lists.len(); // includes current fn (last)
                    if num_levels <= 1 {
                        // Single fn level: use addr directly.
                        addr.clone()
                    } else {
                        // Multi-level cascade: walk from outermost fn to immediately
                        // enclosing fn, cascading captures at each level.
                        //
                        // `last_addr` tracks the VarAddr the NEXT cascade level should
                        // use as its original_addr. It starts as `addr` (the raw VarAddr
                        // from the scope where the name was found).
                        //
                        // Key invariant: a fn does NOT capture names that are local to
                        // its own scope (parameters, LGM entries within its body). Those
                        // are already accessible at call time. Only names from OUTSIDE
                        // the fn boundary are captured. When a name is local to fn_i,
                        // fn_i+1 captures it directly using the raw addr (e.g.,
                        // Parameter(i) is available via frame.params at fn_i+1's
                        // creation time during fn_i's call).
                        let mut last_addr: VarAddr = addr.clone();
                        for level_idx in 0..(num_levels - 1) {
                            let level_fn_boundary = self.fn_scope_boundaries[level_idx];
                            if match_depth >= level_fn_boundary {
                                // Name is local to this fn level (parameter or LGM
                                // within its scope). This fn does NOT need to capture
                                // it — it's already accessible at call time.
                                // Pass the raw addr through to the next level.
                                last_addr = addr.clone();
                            } else {
                                // Name is outside this fn level's boundary — this
                                // level must capture it.
                                let level_original_addr = last_addr.clone();

                                // Add to this fn level's capture list (if not already present).
                                let existing_pos = self.fn_capture_lists[level_idx]
                                    .iter()
                                    .position(|(n, _)| n == name)
                                    .map(|p| p as u32);
                                let pos = if let Some(p) = existing_pos {
                                    p
                                } else {
                                    let idx = self.fn_capture_lists[level_idx].len() as u32;
                                    self.fn_capture_lists[level_idx]
                                        .push((name.to_string(), level_original_addr));
                                    idx
                                };
                                last_addr = VarAddr::ClosureCapture(pos);
                            }
                        }

                        // Current (innermost) fn uses last_addr as its original_addr.
                        // If no intermediate fn needed to capture (all levels had the
                        // name locally), last_addr is still the raw addr (e.g.,
                        // Parameter(i)), which is correct — the innermost fn captures
                        // directly from the enclosing fn's frame.
                        last_addr
                    }
                };
                let capture_list = self
                    .fn_capture_lists
                    .last_mut()
                    .expect("fn_capture_lists is empty");
                let capture_idx =
                    if let Some(pos) = capture_list.iter().position(|(n, _)| n == name) {
                        pos as u32
                    } else {
                        let idx = capture_list.len() as u32;
                        capture_list.push((name.to_string(), original_addr));
                        idx
                    };
                return Some(VarAddr::ClosureCapture(capture_idx));
            }
        }

        // Non-fn-capture path: return the addr as-is.
        //
        // With cumulative LGM slots (walk_surface_document uses enter_scope_with_offset),
        // cross-dict references within a document resolve directly to LGM(absolute_slot).
        // No OuterGroupRef adjustment needed: each dict's entries have unique absolute slots
        // that are valid regardless of which inner dict is doing the referencing.
        Some(addr)
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
    ///
    /// Capture detection is performed the same way as `resolve_name`: if the found scope
    /// frame is outside the current function boundary, the name becomes a ClosureCapture.
    fn resolve_name_parent(&mut self, name: &str) -> Option<VarAddr> {
        let mut passed_dict = false;
        let scopes_len = self.scopes.len();
        let found = self
            .scopes
            .iter()
            .rev()
            .enumerate()
            .find_map(|(offset, (scope, kind))| {
                if !passed_dict {
                    if *kind == ScopeKind::Dict {
                        passed_dict = true;
                    }
                    return None; // skip everything up to and including the nearest Dict scope
                }
                scope.get(name).map(|addr| {
                    let frame_abs_idx = scopes_len.saturating_sub(1 + offset);
                    (frame_abs_idx, offset as u32, addr.clone())
                })
            });

        let (match_depth, traversal_offset, addr) = found?;
        // Inject the correct depth into LetrecGroupMember entries (same as resolve_name).
        let addr = match addr {
            VarAddr::LetrecGroupMember { slot, .. } => VarAddr::LetrecGroupMember {
                depth: traversal_offset,
                slot,
            },
            other => other,
        };

        if self.env_scope_depth.map_or(false, |d| match_depth == d) {
            self.referenced_env_names.insert(name.to_string());
        }

        // Closure conversion: same as resolve_name — if the found scope frame is outside
        // the current function boundary, this is a free variable → ClosureCapture.
        // Store (name, original_addr) so the evaluator can look up the thunk in the
        // enclosing EvalFrame at function-definition time.
        if let Some(&fn_boundary) = self.fn_scope_boundaries.last() {
            if match_depth < fn_boundary {
                let capture_list = self
                    .fn_capture_lists
                    .last_mut()
                    .expect("fn_capture_lists is empty");
                let capture_idx =
                    if let Some(pos) = capture_list.iter().position(|(n, _)| n == name) {
                        pos as u32
                    } else {
                        let idx = capture_list.len() as u32;
                        capture_list.push((name.to_string(), addr.clone()));
                        idx
                    };
                return Some(VarAddr::ClosureCapture(capture_idx));
            }
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

                // Enter the dict's letrec scope with the correct absolute slot base.
                //
                // At runtime, eval_dict_core builds extended_group = outer_frame.group ++ letrec_slots,
                // placing this dict's entries at indices outer_frame.group.len()..outer_frame.group.len()+N.
                // The resolver must assign LGM(base+i) for entry i so that LGM(slot) → frame.group[slot]
                // resolves to the correct thunk. `accumulated_dict_offset` holds the value that
                // outer_frame.group.len() will have at the eval_dict_core call site for this dict.
                //
                // The Dict ScopeKind marks this as a real runtime eval_dict_core frame boundary.
                let base_offset = self.accumulated_dict_offset;
                self.enter_scope_with_offset(&static_keys, base_offset, ScopeKind::Dict);
                // Advance accumulated_dict_offset by this dict's key count so that any nested
                // dicts inside this dict's values use the correct base (= outer_frame.group.len()
                // at their eval_dict_core call site = base_offset + static_keys.len()).
                self.accumulated_dict_offset = base_offset + static_keys.len() as u32;

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
                // Restore accumulated_dict_offset after exiting this dict's scope.
                // Sibling dicts (same level as this one) use the same base_offset.
                self.accumulated_dict_offset = base_offset;
            }

            SurfaceExpression::Fn {
                return_ann: _,
                params,
                body,
                resolved_captures,
                ..
            } => {
                // Walk param annotations in outer scope (before entering fn boundary)
                for param in params {
                    if let Some(ann) = &param.node.annotation {
                        self.walk_surface_annotation(ann);
                    }
                }

                // Push fn boundary BEFORE entering the param scope so that the param scope
                // is considered "local" (scopes.len() after push > boundary).
                // The boundary is the number of scope frames that are "outer" — i.e., the
                // count of frames already on the stack when we enter this function.
                let fn_boundary = self.scopes.len();
                self.fn_scope_boundaries.push(fn_boundary);
                self.fn_capture_lists.push(Vec::new());

                // Reset accumulated_dict_offset to 0 at the fn boundary.
                // A function call creates EvalFrame::for_function_call with group = [] (empty),
                // so all dicts inside the fn body start accumulating from slot 0.
                let saved_dict_offset = self.accumulated_dict_offset;
                self.accumulated_dict_offset = 0;

                let param_names: Vec<String> = params.iter().map(|p| p.node.name.clone()).collect();
                self.enter_param_scope(&param_names);

                self.walk_surface_node(body);

                self.exit_scope();

                // Pop the fn boundary and capture list; set resolved_captures on the Fn node.
                self.fn_scope_boundaries.pop();
                let captures = self
                    .fn_capture_lists
                    .pop()
                    .expect("fn_capture_lists is empty");
                resolved_captures.set(Arc::new(captures));

                // Restore accumulated_dict_offset to its pre-fn value now that we have
                // exited the fn boundary and returned to the enclosing scope.
                self.accumulated_dict_offset = saved_dict_offset;
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
                        if let SurfaceExpression::Dict(entries) = &e.expr {
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
                                // No static keys: no scope injection needed. Walk values normally.
                                for entry in entries {
                                    self.walk_surface_node(&entry.node.value);
                                }
                            } else {
                                // Enter the per-body letrec scope for this intermediate dict.
                                //
                                // ScopeKind::Dict is used here because eval_dict_core IS called for
                                // each intermediate body dict (both at document level and inside fn
                                // bodies via LetrecChainStep). This creates a real runtime letrec frame.
                                // At runtime, LetrecChainStep builds updated_frame.group = prior_frame.group
                                // ++ this_body_thunks, placing body i's entries at indices
                                // sequential_offset..sequential_offset+all_keys.len()-1 in the group.
                                // The resolver must assign LGM(sequential_offset+j) for entry j so that
                                // LGM(slot) → frame.group[slot] resolves to the correct thunk.
                                self.enter_scope_with_offset(
                                    &all_keys,
                                    sequential_offset,
                                    ScopeKind::Dict,
                                );

                                // Walk key annotations inside the letrec scope
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

                                // Walk each entry's VALUE.
                                // Set accumulated_dict_offset to sequential_offset + all_keys.len()
                                // so that any nested dict literals inside this body's values see the
                                // correct base offset for their own letrec scopes. At runtime, those
                                // nested dicts are evaluated via eval_dict_core with outer_frame.group
                                // = updated_frame.group (which has sequential_offset + all_keys.len()
                                // entries).
                                let saved_seq_dict_offset = self.accumulated_dict_offset;
                                self.accumulated_dict_offset =
                                    sequential_offset + all_keys.len() as u32;

                                for entry in entries {
                                    self.walk_surface_node(&entry.node.value);
                                }

                                // Exit the per-body letrec scope and restore offset.
                                self.exit_scope();
                                // Restore accumulated_dict_offset. The FnSequentialBody scope
                                // below does not create a new eval_dict_core frame, so
                                // accumulated_dict_offset remains at saved_seq_dict_offset
                                // for subsequent bodies (they use sequential_offset directly).
                                self.accumulated_dict_offset = saved_seq_dict_offset;

                                // Enter the Sequential scope (injected) so subsequent
                                // bodies can reference this body's bindings.
                                // Use cumulative offset so each body's LGM slots don't overlap
                                // with prior bodies' slots in the LetrecChainStep accumulated group.
                                //
                                // ALWAYS use FnSequentialBody for the accumulated sequential scope
                                // inside fn bodies. This scope is purely a name-availability
                                // bookkeeping scope — NOT a separate eval_dict_core frame.
                                // LetrecChainStep accumulates all body entries into a single flat
                                // group. LGM(cumulative_slot) indexes into that group directly.
                                // BlockBody is used for ALL sequential intermediate bodies (both
                                // document-level and function-body) — same semantics, same kind.
                                self.enter_scope_with_offset(
                                    &all_keys,
                                    sequential_offset,
                                    ScopeKind::BlockBody,
                                );
                                sequential_offset += all_keys.len() as u32;
                                injected += 1;
                            }
                        } else if let Some(keys) = surface_node_static_keys(e) {
                            // Non-dict node that nonetheless produces static keys
                            // (currently none, but keep the original fallback so the
                            // scope injection path stays correct for future cases).
                            self.walk_surface_node(e);
                            if !keys.is_empty() {
                                self.enter_scope_with_offset(
                                    &keys,
                                    sequential_offset,
                                    ScopeKind::Dict,
                                );
                                sequential_offset += keys.len() as u32;
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
                    // Resolve the field-access primitive to get its VarAddr at the current
                    // scope depth. The lowerer reads this VarAddr to locate the function that
                    // implements dot-access (e.g. `x.field`). The name is taken from
                    // `self.field_getter_name` so the resolver stays agnostic to the specific
                    // name used by the active prelude/loader stack. If the name is not in scope
                    // (resolver not seeded with env), leave the OnceLock unset — the lowerer
                    // will emit a diagnostic.
                    if let Some(addr) = self.resolve_name(&self.field_getter_name.clone()) {
                        resolution.set(Some(addr));
                    }
                } else if let crate::ast::DotKey::Ident(name) = field {
                    // Leading-dot `.name`: look up in the PARENT scope, skipping the
                    // nearest enclosing letrec dict scope. This prevents `[k: .k ...]`
                    // from creating a circular self-reference.
                    // `.a.b.c` chains work correctly: only this innermost `expr: None`
                    // case uses parent lookup; outer `.b` / `.c` desugar to field-getter calls.
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
            // However, any Fn expressions inside a quote must have resolved_captures set to
            // empty (no runtime captures — quoted fns are AST values, not runtime closures).
            // The lowerer requires resolved_captures to be set on every Fn node it encounters.
            SurfaceExpression::Quote(inner) => {
                set_empty_captures_in_quote(&inner.expr);
            }

            SurfaceExpression::Unquote(inner) | SurfaceExpression::UnquoteSplice(inner) => {
                self.walk_surface_node(inner);
            }

            SurfaceExpression::Match { scrutinee, arms } => {
                self.walk_surface_node(scrutinee);
                for arm in arms {
                    // If arm.let_bindings is Some(...), this is a [case [let names] pattern body] arm.
                    // Extract binding names, enter_param_scope, walk pattern and body, exit_scope.
                    if let Some(let_bindings) = &arm.let_bindings {
                        self.walk_surface_node(let_bindings);
                        // Extract binding variable names from [let name1 name2 ...].
                        // `_` is excluded (wildcard, not a binding) so the pattern position
                        // VarRef for `_` remains unresolved (Some(None)), which eval treats
                        // as wildcard rather than a pin.
                        let bound_names = extract_case_arm_binding_names(let_bindings);
                        let has_bindings = !bound_names.is_empty();

                        // Push fn boundary BEFORE entering the param scope so that case arm
                        // bindings (p, v, etc.) are INSIDE the boundary → local Parameters.
                        // Outer scope names referenced in pattern/guard/body become ClosureCaptures.
                        // This matches EvalFrame::for_function_call at eval time.
                        let fn_boundary = self.scopes.len();
                        self.fn_scope_boundaries.push(fn_boundary);
                        self.fn_capture_lists.push(Vec::new());
                        let saved_dict_offset = self.accumulated_dict_offset;
                        self.accumulated_dict_offset = 0;

                        if has_bindings {
                            self.enter_param_scope(&bound_names);
                        }

                        // Walk pattern INSIDE the fn boundary and param scope (suppress diagnostics).
                        // Binding names (p, exports, etc.) are now in scope as Parameter(i), so they
                        // lower to Var{addr:Parameter(i)} rather than Placeholder — enabling
                        // bind_or_pin_name to recognise them as bindings at eval time.
                        // External names (Option, builtin-dict-get) become ClosureCaptures, looked up
                        // from the pre_arm_frame (with closure_env but empty params) at eval time.
                        self.suppress_depth += 1;
                        self.walk_surface_node(&arm.pattern);
                        self.suppress_depth -= 1;

                        if let Some(guard) = &arm.guard {
                            self.walk_surface_node(guard);
                        }
                        for body_expr in &arm.body {
                            self.walk_surface_node(body_expr);
                        }
                        if has_bindings {
                            self.exit_scope();
                        }

                        // Pop fn boundary, collect captures, write to case_captures OnceLock.
                        self.fn_scope_boundaries.pop();
                        let captures = self
                            .fn_capture_lists
                            .pop()
                            .expect("fn_capture_lists is empty after case arm body walk");
                        arm.case_captures.set(Arc::new(captures));
                        self.accumulated_dict_offset = saved_dict_offset;
                    } else {
                        // Keyed arm (pattern: body) — no let_bindings, no scope entry.
                        // Walk the pattern with suppress_depth incremented.
                        self.suppress_depth += 1;
                        self.walk_surface_node(&arm.pattern);
                        self.suppress_depth -= 1;
                        if let Some(guard) = &arm.guard {
                            self.walk_surface_node(guard);
                        }
                        for body_expr in &arm.body {
                            self.walk_surface_node(body_expr);
                        }
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
                name,
                annotation: Some(ann),
                ..
            } if crate::eval::is_constructor_name(name) => {
                self.walk_ctor_annotation_values(ann);
            }
            SurfaceExpression::VarRef {
                name,
                annotation: None,
                ..
            } if crate::eval::is_constructor_name(name) => {}
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

    /// Walk a document, assigning cumulative LGM slots to each dict's entries.
    ///
    /// Each intermediate dict's entries are assigned absolute cumulative LGM slots so that
    /// cross-dict references within the document resolve directly to LGM(absolute_slot) without
    /// any runtime frame traversal. The cumulative offset starts at `initial_offset` (the number
    /// of root-scope entries already assigned by the caller) and increases by each dict's key count.
    ///
    /// Document-level scopes use `ScopeKind::Other` (not `ScopeKind::Dict`) because there
    /// is no per-dict eval_dict_core frame at document level — all entries are in the single
    /// accumulated_group at runtime. BlockBody intermediate bodies are collected into
    /// `self.block_body_frames` automatically via `enter_scope_with_offset`.
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
                        if let Some(keys) = surface_node_static_keys(node) {
                            if !keys.is_empty() {
                                // Document-level sequential intermediate dict: BlockBody scope
                                // with cumulative LGM(offset..offset+N-1) slots. Same kind as
                                // function-body sequential intermediates — both are block bodies.
                                // The frame is collected automatically by enter_scope_with_offset.
                                self.enter_scope_with_offset(
                                    &keys,
                                    cumulative_offset,
                                    ScopeKind::BlockBody,
                                );
                                cumulative_offset += keys.len() as u32;
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

        // BlockBody frames are collected automatically by enter_scope_with_offset.
        // Just exit the injected scopes.
        for _ in 0..injected {
            self.exit_scope();
        }
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
/// - `diagnostics`: unified bag of resolve diagnostics (errors only).
///   - `kind = "resolve-error"`, `level = Err`: undefined-variable VarRefs in expression position.
/// - `new_frames`: scope frames ADDED by this document (not including `initial_frames`).
/// Recursively set `resolved_captures = []` on every Fn node inside a Quote body.
///
/// The resolver intentionally skips Quote bodies (variables inside quotes are AST data,
/// not runtime bindings). However, the lowerer requires every Fn node to have
/// `resolved_captures` set. Quoted Fn nodes are AST values, not runtime closures, so
/// their captures are always empty.
fn set_empty_captures_in_quote(expr: &crate::ast::SurfaceExpression) {
    use crate::ast::SurfaceExpression;
    match expr {
        SurfaceExpression::Fn {
            resolved_captures,
            body,
            ..
        } => {
            // Set empty captures — quoted fns are AST values with no runtime closure.
            // CapturesCell::set uses get_or_init internally: first writer wins, so re-visits
            // of shared AST nodes during resolution do not overwrite a previously set value.
            resolved_captures.set(std::sync::Arc::new(vec![]));
            // Recurse into body in case it contains nested fns.
            set_empty_captures_in_quote(&body.expr);
        }
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } => {
            set_empty_captures_in_quote(&func.expr);
            for a in args {
                set_empty_captures_in_quote(&a.expr);
            }
            for na in named_args {
                set_empty_captures_in_quote(&na.node.value.expr);
            }
        }
        SurfaceExpression::Dict(entries) => {
            for e in entries {
                if let Some(k) = &e.node.key {
                    set_empty_captures_in_quote(&k.expr);
                }
                set_empty_captures_in_quote(&e.node.value.expr);
            }
        }
        SurfaceExpression::Sequential(exprs) => {
            for e in exprs {
                set_empty_captures_in_quote(&e.expr);
            }
        }
        SurfaceExpression::Pipe { lhs, rhs, .. } => {
            set_empty_captures_in_quote(&lhs.expr);
            set_empty_captures_in_quote(&rhs.expr);
        }
        SurfaceExpression::Quote(inner) => {
            // Nested quote — still recurse.
            set_empty_captures_in_quote(&inner.expr);
        }
        SurfaceExpression::Unquote(inner) | SurfaceExpression::UnquoteSplice(inner) => {
            set_empty_captures_in_quote(&inner.expr);
        }
        SurfaceExpression::TypeAssert { expr, .. } => {
            set_empty_captures_in_quote(&expr.expr);
        }
        // Leaf nodes: VarRef, Str, Int, Float, Placeholder, Field, etc. — nothing to do.
        _ => {}
    }
}

pub fn resolve_surface_document_inplace(
    doc: &crate::ast::SurfaceDocument,
    initial_frames: &[indexmap::IndexMap<String, u32>],
) -> (
    ResolutionTable,
    Vec<TypeDiagnostic>,
    Vec<indexmap::IndexMap<String, u32>>,
) {
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

    let block_body_frames = std::mem::take(&mut resolver.block_body_frames);
    let (table, diagnostics) = resolver.finish_with_errors();
    (table, diagnostics, block_body_frames)
}

/// Resolve a SurfaceDocument seeded with an env-dict name list.
///
/// This is the resolver entry point for the env-dict protocol (`builtin-eval` / T-1775).
/// Both this function and `resolve_surface_document_inplace` use `LetrecGroupMember(i)`
/// for all names. Env-dict names occupy the first `env_names.len()` LGM slots; document
/// dict entries follow at cumulative slots starting at `env_names.len()`.
///
/// At runtime, `eval_core_document_exprs` starts accumulated_group with the env-dict thunks
/// (slots 0..env_names.len()-1), then extends with each dict's entries at cumulative slots.
/// `LetrecGroupMember(i)` resolves to `group[i]` directly — no frame traversal.
///
/// `env_names`: ordered list of in-scope names from the env-dict (insertion order from
/// the name-set passed to `builtin-resolve`).  The i-th name gets `LetrecGroupMember(i)`.
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
/// LGM(root_group_len + i) slots match the runtime group ordering.
pub fn resolve_surface_document_with_env_dict(
    doc: &crate::ast::SurfaceDocument,
    env_names: &[String],
    root_group_len: u32,
) -> (ResolutionTable, Vec<TypeDiagnostic>, Vec<String>) {
    let mut resolver = SurfaceResolver::new();

    // Seed with LetrecGroupMember(root_group_len + i) for the i-th env-dict name.
    // This matches the runtime ordering: accumulated_group = [root_group, env_dict_thunks, ...]
    // so env-dict name i is at position root_group_len + i.
    //
    // ScopeKind::Other is intentional here (not ScopeKind::Dict).
    //
    // Leading-dot (`.name`) skips the nearest Dict scope to access names from the
    // enclosing parent scope — it is used to deliberately bypass the current letrec
    // frame and reach an outer binding. The skip rule only applies to `ScopeKind::Dict`
    // frames; `ScopeKind::Other` frames are NOT skipped.
    //
    // For env-dict names (the caller's accumulated environment — prelude bindings,
    // prior document exports, capability names, etc.), we want leading-dot to search
    // THROUGH these names rather than skip over them. If env-dict names were registered
    // as `ScopeKind::Dict`, a leading-dot inside a user document would skip the entire
    // env-dict layer, making env-dict names invisible to leading-dot lookups. That
    // would break access to any env-dict binding that a user tries to reach via `.name`.
    //
    // With `ScopeKind::Other`, leading-dot inside a user document skips only the user
    // document's own innermost Dict scope (as intended), then searches upward through
    // the env-dict names without skipping them — the correct behavior.
    // Enter env-dict scope with offset starting at root_group_len.
    // Env-dict name i gets LGM(root_group_len + i) to match the runtime accumulated_group
    // where env-dict thunks follow root_group entries.
    resolver.enter_scope_with_offset(env_names, root_group_len, ScopeKind::Other);
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
/// Returns `(table, diagnostics, unreferenced, block_body_frames)`.
/// `block_body_frames` — BlockBody sequential injection frames for the type checker and builtin-lower.
pub fn resolve_surface_document_with_seed_frames(
    doc: &crate::ast::SurfaceDocument,
    seed_frames: &[indexmap::IndexMap<String, u32>],
    env_names: &[String],
    root_group_len: u32,
) -> (
    ResolutionTable,
    Vec<TypeDiagnostic>,
    Vec<String>,
    Vec<indexmap::IndexMap<String, u32>>,
) {
    let mut resolver = SurfaceResolver::new();

    for frame in seed_frames {
        resolver.enter_scope_from_frame(frame);
    }

    resolver.enter_scope_with_offset(env_names, root_group_len, ScopeKind::Other);
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

    let block_body_frames = std::mem::take(&mut resolver.block_body_frames);

    let (table, diagnostics) = resolver.finish_with_errors();
    (table, diagnostics, unreferenced, block_body_frames)
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

    // All BlockBody sequential injection frames were collected during the walk.
    let block_body_frames = std::mem::take(&mut resolver.block_body_frames);
    let table = resolver.finish();
    (table, block_body_frames)
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
        SurfaceExpression::Dict(entries) => (surface_dict_static_keys(entries), span),
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
/// Returns a vec of `TypeDiagnostic` warnings for abandoned outputs.
pub fn lint_pipeline_stages(
    stages: &[(
        Vec<String>,      // produced keys
        Vec<String>,      // field accesses on the pipeline variable from next doc
        bool,             // next doc uses pipeline variable in non-field (dynamic) context
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
                diagnostics.push(TypeDiagnostic::warn(
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
        SurfaceExpression::Dict(entries) => {
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
                if let Some(let_bindings) = &arm.let_bindings {
                    collect_var_accesses_node(let_bindings, var_name, accesses, dynamic_use);
                }
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
pub(crate) fn surface_dict_static_keys(entries: &[Spanned<SurfaceEntry>]) -> Vec<String> {
    // Pass 1: Collect all explicitly-named keys from non-ClassDecl entries.
    // This lets us avoid injecting a class method name that shadows an explicit
    // user binding (e.g. `=: [fn [let x y] false]` in the same dict).
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

    // Pass 2: Build the key list.
    // ClassDecl is type-level only — push the outer class name, no method injection.
    // InstanceDecl (named or anonymous) injects both plain method names (for call-site
    // resolution) and mangled binding names (for dispatch), regardless of whether
    // the instance has an outer key. Plain method names come first so the lowerer's
    // slot layout matches.
    let mut keys = Vec::new();
    let mut injected_method_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
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
        // For any InstanceDecl (named or anonymous), also inject method names.
        if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
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
                        // Inject plain method name for call-site resolution (de-duplicated).
                        if !explicit_keys.contains(&method_name)
                            && injected_method_names.insert(method_name.clone())
                        {
                            keys.push(method_name.clone());
                        }
                        // Inject mangled binding name for dispatch.
                        keys.push(crate::type_def::instance_binding_name(
                            &class_decl_name(class_name),
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
                    if let Some(let_bindings) = &arm.let_bindings {
                        collect_varrefs_in_node(let_bindings, name, out);
                    }
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
    /// value should resolve to LetrecGroupMember(0) since `x` is the first key in scope.
    #[test]
    fn dict_sibling_key_scoping() {
        let (program, table) = parse_and_resolve("[x: 1  y: $x]");
        let refs = find_varref_nodes(&program, "x");
        assert!(!refs.is_empty(), "expected at least one VarRef for $x");
        // $x inside the dict value for y resolves to LetrecGroupMember(0) — first key
        let (id, _) = &refs[0];
        let addr = table
            .get(id)
            .expect("$x should be resolved (it's a sibling key)");
        assert_eq!(
            addr,
            &VarAddr::LetrecGroupMember { depth: 0, slot: 0 },
            "x should be LetrecGroupMember {{ depth: 0, slot: 0 }} (first key in same dict scope)"
        );
    }

    /// In a Fn body, VarRef to the param resolves to Parameter(0).
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
            &VarAddr::Parameter(0),
            "first fn param should be Parameter(0)"
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
            &VarAddr::Parameter(1),
            "b is the second param, should be Parameter(1)"
        );
    }

    /// A VarRef inside a fn body that refers to an outer dict key (closure capture)
    /// resolves to ClosureCapture(0) — the first free variable captured by this fn.
    #[test]
    fn fn_body_captures_outer_dict_key() {
        // outer: 42  inner: [fn [] $outer]
        // $outer is a free variable in the fn body — first capture → ClosureCapture(0)
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
        assert_eq!(
            addr,
            &VarAddr::ClosureCapture(0),
            "$outer is the first capture in the fn, should be ClosureCapture(0)"
        );
    }

    /// Match arm pattern bindings should be resolved in the arm body.
    /// Uses [case [let n] _ $n] form: [let n] declares the binding, _ matches anything,
    /// and $n in the body resolves to the case arm's scope as Parameter(0).
    ///
    /// Case arm bindings use enter_param_scope so they resolve to Parameter(i),
    /// not LetrecGroupMember(i). This avoids collision with root_group builtin slots
    /// (which also start at LGM(0)). At runtime, arm_frame.params[i] holds the bound thunk.
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
            &VarAddr::Parameter(0),
            "n is the first (and only) binding — Parameter(0) via enter_param_scope (B-598)"
        );
    }

    /// Case arm bodies see the bindings declared in [let ...].
    /// Bare lowercase names in match arm patterns are now Pin (not Variable).
    /// To bind a variable, use [case [let n] pattern body] form.
    ///
    /// Case arm bindings resolve to Parameter(i) so they don't conflict with
    /// root_group builtin slots (LGM(0) = builtin-int-add). The arm body accesses `n`
    /// via arm_frame.params[0] which holds the bound thunk from pattern matching.
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
            // The case arm scope introduces n as Parameter(0) via enter_param_scope (B-598)
            assert_eq!(
                addr,
                &VarAddr::Parameter(0),
                "n is Parameter(0) via enter_param_scope (B-598)"
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
        // All $x refs should resolve to the dict-level binding LetrecGroupMember(0)
        for (id, _) in &x_refs {
            let addr = table.get(id).expect("$x should be resolved (dict binding)");
            assert_eq!(
                addr,
                &VarAddr::LetrecGroupMember { depth: 0, slot: 0 },
                "$x is first binding, LetrecGroupMember {{ depth: 0, slot: 0 }}"
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
        // The first dict creates a scope with `a` as LetrecGroupMember(0)
        assert_eq!(
            addr,
            &VarAddr::LetrecGroupMember { depth: 0, slot: 0 },
            "a is first key from prior expr, LetrecGroupMember {{ depth: 0, slot: 0 }}"
        );
    }

    // --- B-600: Unreferenced env-name tracking ---

    /// Helper: parse, desugar, and resolve via the env-dict protocol,
    /// returning the unreferenced env names (third return value).
    fn resolve_with_env_dict(src: &str, env_names: &[String]) -> Vec<String> {
        let output = crate::parser::parse(src, test_file(src)).expect("parse failed");
        let program = crate::desugar::desugar_program_full(&output.program);
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

        // `x` resolves as sibling in outer dict: LGM(0)
        let x_refs = find_varref_nodes(&program, "x");
        assert!(!x_refs.is_empty(), "expected VarRef for $x");
        let (x_id, _) = &x_refs[0];
        let x_addr = table.get(x_id).expect("$x should be resolved");
        assert_eq!(
            x_addr,
            &VarAddr::LetrecGroupMember { depth: 1, slot: 0 },
            "$x should be LGM {{ depth: 1, slot: 0 }} — first entry of outer dict, seen from inner dict"
        );

        // Find the VarRef that is the VALUE of `ref` in the inner dict.
        // There is exactly one $x VarRef (in the inner dict's value).
        // It must resolve to LGM(0), not LGM(2) or something else.
        // (Already verified above — the same VarRef node is found.)
    }

    /// Nested dict with a non-zero initial frame offset.
    ///
    /// Simulates the case where R root entries exist (e.g., builtins) before the document
    /// dict. With initial frame [{builtin-dict-get: 0}] (R=1): outer dict has offset 1.
    ///   Outer dict: x→LGM(1), inner→LGM(2). accumulated_dict_offset advances to 3.
    ///   Inner dict: ref→LGM(3). $x → LGM(1).
    ///
    /// Before the fix, the Dict arm used enter_scope(&keys, Dict) with offset=0, giving
    ///   x→LGM(0), inner→LGM(1), ref→LGM(0). LGM(0) → group[0] = builtin-dict-get (wrong!).
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
        let (table, _diags, _frames) = resolve_surface_document_inplace(doc, &[root_frame]);

        // Outer dict: x at LGM(1), inner at LGM(2) (offset=1 from root frame).
        let x_refs = find_varref_nodes(&program, "x");
        assert!(!x_refs.is_empty(), "expected VarRef for $x");
        let (x_id, _) = &x_refs[0];
        let x_addr = table.get(x_id).expect("$x should be resolved");
        assert_eq!(
            x_addr,
            &VarAddr::LetrecGroupMember { depth: 1, slot: 1 },
            "$x must be LGM(depth=1, slot=1) — x is in the outer dict (one scope up from the reference site)"
        );
    }

    /// Nested dict inside a fn body intermediate dict value.
    ///
    /// `[fn [let x] [a: $x  nested: [c: $x]] ...]`
    /// Fn resets accumulated_dict_offset to 0. Body dict: a→LGM(0), nested→LGM(1),
    /// offset advances to 2. Inner dict: c→LGM(2). $x in c's value → Parameter(0).
    #[test]
    fn nested_dict_inside_fn_body_dict() {
        // fn with body dict containing a nested dict value.
        // The sequential body is [a: $x  nested: [c: $x]]; $x should be Parameter(0) both times.
        let (program, table) = parse_and_resolve("[fn [let x] [a: $x  nested: [c: $x]] a]");

        // All $x VarRefs inside the fn body should resolve to Parameter(0).
        let x_refs = find_varref_nodes(&program, "x");
        assert!(
            x_refs.len() >= 2,
            "expected at least 2 VarRefs for $x (in a's value and c's value)"
        );
        for (id, _) in &x_refs {
            let addr = table.get(id).expect("$x should be resolved to fn param");
            assert_eq!(
                addr,
                &VarAddr::Parameter(0),
                "$x in fn body must be Parameter(0)"
            );
        }
    }

    // --- Helpers for Fn capture inspection ---

    /// Recursively collect all Fn nodes from the program, returning each Fn's
    /// resolved_captures list (if set by the resolver).
    fn collect_fn_captures(
        program: &crate::ast::SurfaceProgram,
    ) -> Vec<(Vec<String>, Arc<Vec<(String, VarAddr)>>)> {
        let mut results = Vec::new();
        for doc_spanned in &program.documents {
            for item in &doc_spanned.node.items {
                if let crate::ast::SurfaceItem::Expr(node) = item {
                    collect_fn_nodes(node, &mut results);
                }
            }
        }
        results
    }

    fn collect_fn_nodes(
        arc: &Arc<SurfaceNode>,
        out: &mut Vec<(Vec<String>, Arc<Vec<(String, VarAddr)>>)>,
    ) {
        match &arc.expr {
            SurfaceExpression::Fn {
                params,
                body,
                resolved_captures,
                ..
            } => {
                let param_names: Vec<String> = params.iter().map(|p| p.node.name.clone()).collect();
                if let Some(captures) = resolved_captures.get() {
                    out.push((param_names, Arc::clone(captures)));
                }
                collect_fn_nodes(body, out);
            }
            SurfaceExpression::Dict(entries) => {
                for entry in entries {
                    if let Some(key) = &entry.node.key {
                        collect_fn_nodes(key, out);
                    }
                    collect_fn_nodes(&entry.node.value, out);
                }
            }
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                collect_fn_nodes(func, out);
                for arg in args {
                    collect_fn_nodes(arg, out);
                }
                for na in named_args {
                    collect_fn_nodes(&na.node.value, out);
                }
            }
            SurfaceExpression::Sequential(exprs) => {
                for e in exprs {
                    collect_fn_nodes(e, out);
                }
            }
            SurfaceExpression::Match { scrutinee, arms } => {
                collect_fn_nodes(scrutinee, out);
                for arm in arms {
                    collect_fn_nodes(&arm.pattern, out);
                    if let Some(let_bindings) = &arm.let_bindings {
                        collect_fn_nodes(let_bindings, out);
                    }
                    if let Some(guard) = &arm.guard {
                        collect_fn_nodes(guard, out);
                    }
                    for body_expr in &arm.body {
                        collect_fn_nodes(body_expr, out);
                    }
                }
            }
            SurfaceExpression::Pipe { lhs, rhs, .. } => {
                collect_fn_nodes(lhs, out);
                collect_fn_nodes(rhs, out);
            }
            SurfaceExpression::TypeAssert { expr, .. } => collect_fn_nodes(expr, out),
            SurfaceExpression::Field {
                expr: Some(expr), ..
            } => collect_fn_nodes(expr, out),
            _ => {}
        }
    }

    // --- B-577: nested fn capturing outer fn's parameter ---

    /// When fn_B is nested inside fn_A and captures fn_A's parameter,
    /// fn_A must NOT have that parameter in its own capture list (it's already
    /// available as a parameter). fn_B must capture it with original_addr = Parameter(i).
    #[test]
    fn nested_fn_does_not_capture_outer_fn_own_param() {
        // [outer: [fn [let x] [fn [let y] $x]]]
        // outer fn (params: [x]) should have NO captures related to x.
        // inner fn (params: [y]) should capture x with original_addr = Parameter(0).
        let (program, table) = parse_and_resolve("[outer: [fn [let x] [fn [let y] $x]]]");

        // $x inside inner fn should be ClosureCapture(0).
        let x_refs = find_varref_nodes(&program, "x");
        assert!(
            !x_refs.is_empty(),
            "expected VarRef for $x in inner fn body"
        );
        let (id, _) = &x_refs[0];
        let addr = table.get(id).expect("$x should be resolved");
        assert_eq!(
            addr,
            &VarAddr::ClosureCapture(0),
            "$x in inner fn body should be ClosureCapture(0)"
        );

        // Now check the capture lists. Collect all Fn nodes.
        let fns = collect_fn_captures(&program);
        assert_eq!(fns.len(), 2, "expected 2 Fn nodes (outer and inner)");

        // outer fn (params: [x]): should have NO captures (x is its own parameter).
        let (ref outer_params, ref outer_captures) = fns[0];
        assert!(
            outer_params.contains(&"x".to_string()),
            "outer fn should have param x"
        );
        assert!(
            !outer_captures.iter().any(|(n, _)| n == "x"),
            "outer fn must NOT capture its own parameter x — captures: {:?}",
            outer_captures
        );

        // inner fn (params: [y]): should capture x with original_addr = Parameter(0).
        let (ref inner_params, ref inner_captures) = fns[1];
        assert!(
            inner_params.contains(&"y".to_string()),
            "inner fn should have param y"
        );
        let x_capture = inner_captures
            .iter()
            .find(|(n, _)| n == "x")
            .expect("inner fn must capture x");
        assert_eq!(
            x_capture.1,
            VarAddr::Parameter(0),
            "inner fn's capture of x should have original_addr = Parameter(0) \
             (from outer fn's parameter, available at inner fn creation time)"
        );
    }

    /// Triple nesting: fn_A → fn_B → fn_C, where fn_C captures fn_A's parameter.
    /// fn_A must not capture its own param. fn_B must capture via Parameter(i).
    /// fn_C must capture via ClosureCapture from fn_B.
    #[test]
    fn triple_nested_fn_captures_outermost_param() {
        // [f: [fn [let x] [fn [let y] [fn [let z] $x]]]]
        let (program, table) = parse_and_resolve("[f: [fn [let x] [fn [let y] [fn [let z] $x]]]]");

        // $x in innermost fn should be ClosureCapture.
        let x_refs = find_varref_nodes(&program, "x");
        assert!(!x_refs.is_empty());
        let (id, _) = &x_refs[0];
        let addr = table.get(id).expect("$x should be resolved");
        assert_eq!(
            addr,
            &VarAddr::ClosureCapture(0),
            "$x in fn_C should be ClosureCapture(0)"
        );

        let fns = collect_fn_captures(&program);
        assert_eq!(fns.len(), 3, "expected 3 Fn nodes");

        // fn_A (params: [x]): no captures of x.
        assert!(
            !fns[0].1.iter().any(|(n, _)| n == "x"),
            "fn_A must not capture its own parameter x"
        );

        // fn_B (params: [y]): captures x with original_addr = Parameter(0).
        let b_x_capture = fns[1]
            .1
            .iter()
            .find(|(n, _)| n == "x")
            .expect("fn_B must capture x");
        assert_eq!(
            b_x_capture.1,
            VarAddr::Parameter(0),
            "fn_B captures x from fn_A's Parameter(0)"
        );

        // fn_C (params: [z]): captures x with original_addr = ClosureCapture(pos_in_fn_B).
        let c_x_capture = fns[2]
            .1
            .iter()
            .find(|(n, _)| n == "x")
            .expect("fn_C must capture x");
        let b_x_pos = fns[1].1.iter().position(|(n, _)| n == "x").unwrap() as u32;
        assert_eq!(
            c_x_capture.1,
            VarAddr::ClosureCapture(b_x_pos),
            "fn_C captures x via ClosureCapture from fn_B"
        );
    }

    #[test]
    fn collect_var_accesses_works_with_non_percent_var_name() {
        // Verifies collect_var_accesses is truly generic: it works with any var_name,
        // not just "%". Source accesses "input" field on the named variable "src".
        let output = crate::parser::parse("[result: src.input]", test_file("[result: src.input]"))
            .expect("parse failed");
        let program = crate::desugar::desugar_program_full(&output.program);
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
}
