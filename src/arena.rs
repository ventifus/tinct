//! Arena allocation for thunks and lexical scope frames.
//!
//! A `Scope` is one lexical scope frame in a de Bruijn scope chain. Named bindings
//! are stored in insertion order; variables are addressed by `(level, slot)` pairs
//! where level = parent-chain hops and slot = ordinal position within that Scope.
//!
//! `ScopeArena` holds all Scopes indexed by `ScopeId`. `ThunkId { scope_id, slot }`
//! is a stable address into the arena for the lifetime of the program.

use std::sync::Arc;

/// Per-thunk byte estimate for memory budget tracking.
const PER_THUNK_BYTES: usize = std::mem::size_of::<crate::value::Thunk>() + 32;

/// A handle to a thunk in the arena. Copy-cheap (8 bytes).
/// `scope_id` indexes into `ScopeArena.scopes`; `slot` is the ordinal position within that Scope.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThunkId {
    /// Which Scope owns this thunk (absolute index into ScopeArena.scopes).
    pub scope_id: u32,
    /// Ordinal position within that Scope's slots.
    pub slot: u32,
}

/// A handle to a Scope in the arena. Copy-cheap (4 bytes), indexes into `ScopeArena`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeId(pub u32);

/// Arena for Scope allocation. Stores `Scope` frames indexed by `ScopeId`.
///
/// Each `Scope` directly owns its `Arc<Thunk>` values — thunks are addressed by
/// `ThunkId { scope_id, slot }` which indexes into the owning Scope.
///
/// Dropped scopes have their slots cleared but their `Scope` struct remains in the `scopes`
/// Vec at the same index (so existing `ScopeId` values remain valid). The `free_list` tracks
/// these cleared-slot structs so they can be reused by subsequent `alloc_root`/`alloc_child`
/// calls without growing the Vec further.
#[derive(Debug)]
pub struct ScopeArena {
    pub(crate) scopes: Vec<Scope>,
    /// Indices of scopes that have been dropped (slots cleared) and are available for reuse.
    /// `drop_scope` pushes to this list; `alloc_root`/`alloc_child` pop from it first.
    free_list: Vec<ScopeId>,
}

impl ScopeArena {
    /// Create a new empty environment arena.
    pub fn new() -> Self {
        Self {
            scopes: Vec::new(),
            free_list: Vec::new(),
        }
    }

    /// Allocate a root environment (no parent) with the given slot capacity.
    pub fn alloc_root(&mut self, slot_count: usize) -> ScopeId {
        if let Some(id) = self.free_list.pop() {
            // Reinitialize the recycled Scope: clear parent, reserve capacity.
            let scope = &mut self.scopes[id.0 as usize];
            scope.parent = None;
            // slots was cleared by drop_scope; reserve capacity for new use.
            scope.slots.reserve(slot_count);
            let actual_capacity = scope.slots.capacity();
            let scope_bytes = std::mem::size_of::<Scope>()
                + actual_capacity * std::mem::size_of::<Option<Arc<crate::value::Thunk>>>();
            crate::memory_budget::record_scope_alloc(scope_bytes);
            return id;
        }
        let len = self.scopes.len();
        assert!(
            len < u32::MAX as usize,
            "ScopeArena overflow: more than {} environments allocated",
            u32::MAX
        );
        let id = ScopeId(len as u32);
        self.scopes.push(Scope::new(None, slot_count));
        // Read the actual capacity immediately after Vec::with_capacity(slot_count); for a
        // freshly created Vec this equals slot_count, but using capacity() here ensures the
        // alloc-side matches the free-side (drop_scope) which also uses capacity().
        let actual_capacity = self.scopes.last().expect("just pushed").slots.capacity();
        let scope_bytes = std::mem::size_of::<Scope>()
            + actual_capacity * std::mem::size_of::<Option<Arc<crate::value::Thunk>>>();
        crate::memory_budget::record_scope_alloc(scope_bytes);
        id
    }

    /// Allocate a child environment with the given parent.
    pub fn alloc_child(&mut self, parent_id: ScopeId, slot_count: usize) -> ScopeId {
        if let Some(id) = self.free_list.pop() {
            // Reinitialize the recycled Scope: set parent, reserve capacity.
            let scope = &mut self.scopes[id.0 as usize];
            scope.parent = Some(parent_id);
            // slots was cleared by drop_scope; reserve capacity for new use.
            scope.slots.reserve(slot_count);
            let actual_capacity = scope.slots.capacity();
            let scope_bytes = std::mem::size_of::<Scope>()
                + actual_capacity * std::mem::size_of::<Option<Arc<crate::value::Thunk>>>();
            crate::memory_budget::record_scope_alloc(scope_bytes);
            return id;
        }
        let len = self.scopes.len();
        assert!(
            len < u32::MAX as usize,
            "ScopeArena overflow: more than {} environments allocated",
            u32::MAX
        );
        let id = ScopeId(len as u32);
        self.scopes.push(Scope::new(Some(parent_id), slot_count));
        // Read the actual capacity immediately after Vec::with_capacity(slot_count); for a
        // freshly created Vec this equals slot_count, but using capacity() here ensures the
        // alloc-side matches the free-side (drop_scope) which also uses capacity().
        let actual_capacity = self.scopes.last().expect("just pushed").slots.capacity();
        let scope_bytes = std::mem::size_of::<Scope>()
            + actual_capacity * std::mem::size_of::<Option<Arc<crate::value::Thunk>>>();
        crate::memory_budget::record_scope_alloc(scope_bytes);
        id
    }

    /// Allocate a slot in env_id, returning its ThunkId.
    pub fn push_slot(&mut self, env_id: ScopeId, thunk: Arc<crate::value::Thunk>) -> ThunkId {
        let slot = self.scopes[env_id.0 as usize].push(thunk);
        ThunkId {
            scope_id: env_id.0,
            slot,
        }
    }

    /// Reserve a slot for letrec phase 1 (None placeholder, filled later by fill_slot).
    pub fn reserve_slot(&mut self, env_id: ScopeId) -> u32 {
        self.scopes[env_id.0 as usize].reserve()
    }

    /// Fill a reserved slot (letrec phase 2). Fetches the thunk from src_thunk_id.
    pub fn fill_slot(&mut self, env_id: ScopeId, slot_idx: u32, src_thunk_id: ThunkId) {
        let thunk = {
            let src_env = &self.scopes[src_thunk_id.scope_id as usize];
            src_env
                .get(src_thunk_id.slot)
                .cloned()
                .expect("fill_slot: source ThunkId points to an unfilled slot")
        };
        self.scopes[env_id.0 as usize].fill(slot_idx, thunk);
    }

    /// Walk the parent chain `level` hops from `start_env_id`.
    ///
    /// Returns `Ok(target_env_id)` if the chain is deep enough, or `Err(depth_reached)`
    /// if the chain ran out of parents before reaching `level` hops.
    ///
    /// `level=0` returns `start_env_id` immediately (no hops). `level=1` returns
    /// the immediate parent, `level=2` the grandparent, and so on.
    pub fn walk_parent_chain(&self, start_env_id: u32, level: usize) -> Result<ScopeId, usize> {
        let mut current = ScopeId(start_env_id);
        for hop in 0..level {
            match self.scopes[current.0 as usize].parent {
                Some(p) => current = p,
                None => return Err(hop),
            }
        }
        Ok(current)
    }

    /// Collect the parent chain from `start_env_id` outward, returning a Vec where
    /// index 0 is the root (outermost) and the last element is `start_env_id` (innermost).
    ///
    /// Used by the resolver to seed scope levels outermost-first.
    pub fn collect_parent_chain(&self, start_env_id: u32) -> Vec<ScopeId> {
        let mut chain = Vec::new();
        let mut current = ScopeId(start_env_id);
        loop {
            chain.push(current);
            match self.scopes[current.0 as usize].parent {
                Some(p) => current = p,
                None => break,
            }
        }
        chain.reverse(); // outermost (root) first
        chain
    }

    /// Get a thunk by ThunkId.
    ///
    /// # Panics
    ///
    /// Panics if the ThunkId is out of bounds or points to a dropped slot.
    pub fn get_thunk(&self, id: ThunkId) -> Arc<crate::value::Thunk> {
        self.scopes[id.scope_id as usize]
            .get(id.slot)
            .expect(
                "use-after-free: ThunkId accessed after arena scope was dropped or slot is empty",
            )
            .clone()
    }

    /// Drop a scope: clear all thunk slots, freeing Arc<Thunk> references.
    ///
    /// The `Scope` struct itself is retained in `self.scopes` at the same index — the `ScopeId`
    /// remains a valid array index for the lifetime of the arena. The cleared `Scope` is pushed
    /// onto the `free_list` so that `alloc_root`/`alloc_child` can reuse the slot without
    /// growing the `scopes` Vec.
    pub fn drop_scope(&mut self, env_id: ScopeId) {
        let scope = &self.scopes[env_id.0 as usize];
        let live = scope.count_live();
        // Record scope deallocation: same formula as alloc_root/alloc_child.
        // Use slots.capacity() to match the with_capacity(slot_count) used at allocation time.
        let scope_bytes = std::mem::size_of::<Scope>()
            + scope.slots.capacity() * std::mem::size_of::<Option<Arc<crate::value::Thunk>>>();
        crate::memory_budget::record_thunk_free(live * PER_THUNK_BYTES, live);
        crate::memory_budget::record_scope_free(scope_bytes);
        self.scopes[env_id.0 as usize].clear();
        // Make this slot available for future alloc_root/alloc_child calls.
        self.free_list.push(env_id);
    }

    /// Drop all scopes in `[start_env_id, scopes.len())` and truncate the Vec.
    ///
    /// Unlike calling `drop_scope` in a loop followed by `scopes.truncate()`, this method
    /// does NOT push freed ScopeIds onto `free_list`. Doing so would be wrong: after
    /// `truncate(start_env_id)`, those indices are out of bounds, so any subsequent
    /// `alloc_root`/`alloc_child` that pops a stale entry would index out of bounds and panic.
    ///
    /// Additionally, any ScopeIds `>= start_env_id` that were already on `free_list` from
    /// prior `drop_scope` calls are removed (`retain`) before truncation, ensuring no
    /// stale out-of-bounds entries remain after the truncation.
    ///
    /// Used by `builtin-arena-drop` to reclaim an entire arena region atomically.
    pub fn drop_range_and_truncate(&mut self, start_env_id: u32) {
        let end = self.scopes.len() as u32;
        for eid in start_env_id..end {
            let scope = &self.scopes[eid as usize];
            let live = scope.count_live();
            let scope_bytes = std::mem::size_of::<Scope>()
                + scope.slots.capacity() * std::mem::size_of::<Option<Arc<crate::value::Thunk>>>();
            crate::memory_budget::record_thunk_free(live * PER_THUNK_BYTES, live);
            crate::memory_budget::record_scope_free(scope_bytes);
            self.scopes[eid as usize].clear();
            // Do NOT push to free_list — scopes.truncate() below will remove these entries.
        }
        // Remove any free_list entries that now point past the end of the truncated Vec.
        // Prior drop_scope calls may have pushed ScopeIds >= start_env_id onto free_list;
        // those indices are out-of-bounds after truncation and must not be reused.
        self.free_list.retain(|id| id.0 < start_env_id);
        self.scopes.truncate(start_env_id as usize);
    }

    /// Get a reference to the environment at the given handle.
    #[cfg(test)]
    fn get(&self, id: ScopeId) -> &Scope {
        &self.scopes[id.0 as usize]
    }

    /// Get a mutable reference to the environment at the given handle.
    #[cfg(test)]
    pub fn get_mut(&mut self, id: ScopeId) -> &mut Scope {
        &mut self.scopes[id.0 as usize]
    }
}

impl Default for ScopeArena {
    fn default() -> Self {
        Self::new()
    }
}

/// Flat environment representation: slot-based variable lookup via parent-chain traversal.
///
/// Slots directly own `Arc<Thunk>` values. Variables are assigned `(level, slot)` pairs
/// by the resolver (de Bruijn levels). Lookup walks the parent chain `level` times from
/// the current scope, then indexes into that environment's slots by ordinal position.
#[derive(Debug)]
pub struct Scope {
    pub(crate) slots: Vec<Option<Arc<crate::value::Thunk>>>,
    pub(crate) parent: Option<ScopeId>,
}

impl Scope {
    fn new(parent: Option<ScopeId>, capacity: usize) -> Self {
        Self {
            slots: Vec::with_capacity(capacity),
            parent,
        }
    }

    /// Append a slot. Returns the slot index.
    pub(crate) fn push(&mut self, thunk: Arc<crate::value::Thunk>) -> u32 {
        let slot = self.slots.len() as u32;
        self.slots.push(Some(thunk));
        crate::memory_budget::record_and_check(PER_THUNK_BYTES);
        slot
    }

    /// Reserve a slot for letrec phase 1 (None placeholder, filled later by fill()).
    pub(crate) fn reserve(&mut self) -> u32 {
        let slot = self.slots.len() as u32;
        self.slots.push(None);
        slot
    }

    /// Fill a previously reserved slot (letrec phase 2).
    pub(crate) fn fill(&mut self, slot: u32, thunk: Arc<crate::value::Thunk>) {
        self.slots[slot as usize] = Some(thunk);
    }

    /// Hot path: O(1) ordinal lookup by slot index.
    pub(crate) fn get(&self, slot: u32) -> Option<&Arc<crate::value::Thunk>> {
        self.slots.get(slot as usize)?.as_ref()
    }

    /// Number of slots (filled + reserved).
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Count live (filled) slots — for builtin-arena-stats.
    pub(crate) fn count_live(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Clear all slots, freeing Arc<Thunk> references (for drop_scope).
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
    }
}

/// Migrate a Scope from the source range to the destination scope.
///
/// Allocates a new Scope in the destination arena, migrates all slot ThunkIds,
/// and recursively migrates any parent Scopes in the source range (via the display chain).
///
/// The two-phase pattern is applied at the Scope level too: the new destination ScopeId is
/// inserted into `env_map` BEFORE slots are migrated, so that cycles (e.g., a slot value
/// containing a closure referencing this same Scope) do not loop infinitely.
///
/// Returns the destination ScopeId for the migrated Scope.
pub fn migrate_flat_env(
    src_env_id_u32: u32,
    src_range: &std::ops::Range<u32>,
    dst_env_id: ScopeId,
    thunk_map: &mut std::collections::HashMap<ThunkId, ThunkId>,
    env_map: &mut std::collections::HashMap<u32, u32>,
    arena: &mut ScopeArena,
) -> u32 {
    // Already migrated? Return cached result.
    if let Some(&mapped) = env_map.get(&src_env_id_u32) {
        return mapped;
    }

    // Collect the slot count and parent of the source Scope.
    let (slot_count, src_parent) = {
        let src_env = &arena.scopes[src_env_id_u32 as usize];
        (src_env.len(), src_env.parent)
    };

    // Allocate the new destination Scope.
    // Use alloc_root temporarily; the parent pointer is translated and set below after slots migrate.
    let new_env_id = arena.alloc_root(slot_count);

    // Pre-insert into env_map BEFORE migrating slots (two-phase cycle safety at env level).
    env_map.insert(src_env_id_u32, new_env_id.0);

    // Migrate each slot ThunkId from the source Scope into new_env_id.
    //
    // Thunks are allocated directly into new_env_id (not dst_env_id), so each source
    // Scope maps 1:1 to exactly one destination Scope. This eliminates ThunkId
    // aliasing that would arise if two different env_ids held the same logical thunk.
    //
    // Two-phase per-slot: first allocate all placeholder slots in new_env_id (establishing
    // the correct slot indices and inserting into thunk_map), then recurse to fill values.
    // This preserves slot ordering even when migrated values contain nested ThunkIds that
    // trigger additional allocations into other Scopes.

    // Phase 1: allocate placeholders and populate thunk_map for all filled slots.
    // None slots get a None pushed to preserve slot numbering.
    // Collect source slots for migration (slot index → Arc<Thunk>|None).
    let src_slots: Vec<Option<Arc<crate::value::Thunk>>> = {
        let src_env = &arena.scopes[src_env_id_u32 as usize];
        src_env.slots.clone()
    };
    for (slot_idx, slot_opt) in src_slots.iter().enumerate() {
        let src_tid = ThunkId {
            scope_id: src_env_id_u32,
            slot: slot_idx as u32,
        };
        match slot_opt {
            None => {
                arena.scopes[new_env_id.0 as usize].reserve();
            }
            Some(src_arc) => {
                let placeholder = Arc::new(crate::value::Thunk::placeholder(src_arc.span.clone()));
                let new_tid = arena.push_slot(new_env_id, Arc::clone(&placeholder));
                debug_assert_eq!(
                    new_tid.slot, slot_idx as u32,
                    "slot ordering must match source"
                );
                thunk_map.insert(src_tid, new_tid);
            }
        }
    }

    // Phase 2: fill each placeholder by recursing into migrate_value or translating
    // unevaluated state.
    for (slot_idx, slot_opt) in src_slots.iter().enumerate() {
        if let Some(src_arc) = slot_opt {
            let placeholder = arena.scopes[new_env_id.0 as usize].slots[slot_idx]
                .clone()
                .expect("placeholder must exist after phase 1");
            if let Some(value) = src_arc.try_get_materialized() {
                // Materialized: migrate the value recursively.
                let migrated =
                    migrate_value(&value, src_range, dst_env_id, thunk_map, env_map, arena);
                placeholder.settle(Ok(migrated));
            } else if let Some(state) = src_arc.try_claim() {
                // Unevaluated: translate all env_id / ThunkId fields so the migrated thunk
                // does not reference dropped Scopes after arena-drop src.
                let translated = translate_unevaluated_state(
                    state, src_range, dst_env_id, thunk_map, env_map, arena,
                );
                placeholder.reset(translated);
            }
            // If state is None (InProgress/concurrent transition), the placeholder stays
            // as-is — forcing it will return a circular_dependency error, which is correct
            // since an InProgress thunk has no stable content to migrate.
        }
    }

    // Translate the parent pointer: if the source parent is in src_range, migrate it
    // recursively; otherwise keep it as-is (permanent env outside the migrated range).
    let new_parent: Option<ScopeId> = src_parent.map(|p| {
        if src_range.contains(&p.0) {
            // Recursively migrate (env_map prevents infinite recursion)
            let migrated_id =
                migrate_flat_env(p.0, src_range, dst_env_id, thunk_map, env_map, arena);
            ScopeId(migrated_id)
        } else {
            // Outside src_range — permanent, keep as-is
            p
        }
    });

    // Set the translated parent on the new Scope.
    // Override the None set by alloc_root.
    arena.scopes[new_env_id.0 as usize].parent = new_parent;

    new_env_id.0
}

/// Recursively migrate a Value from source env_id range to destination scope.
///
/// ThunkIds in [src_range.start, src_range.end) are copied to dst; others are permanent.
/// `Value::Function.closure_env_id` values in src_range are translated via `env_map`,
/// which maps source Scope ScopeIds to their destination counterparts.
pub fn migrate_value(
    value: &crate::value::Value,
    src_range: &std::ops::Range<u32>,
    dst_env_id: ScopeId,
    thunk_map: &mut std::collections::HashMap<ThunkId, ThunkId>,
    env_map: &mut std::collections::HashMap<u32, u32>,
    arena: &mut ScopeArena,
) -> crate::value::Value {
    use crate::value::Value;
    match value {
        Value::Dict(map) => {
            let mut new_map = indexmap::IndexMap::with_capacity(map.len());
            for (key, &thunk_id) in map.iter() {
                let new_tid =
                    migrate_thunk_id(thunk_id, src_range, dst_env_id, thunk_map, env_map, arena);
                new_map.insert(key.clone(), new_tid);
            }
            Value::Dict(new_map)
        }
        Value::Seq { head, tail } => Value::Seq {
            head: migrate_thunk_id(*head, src_range, dst_env_id, thunk_map, env_map, arena),
            tail: migrate_thunk_id(*tail, src_range, dst_env_id, thunk_map, env_map, arena),
        },
        Value::Variant {
            tycon,
            ctor,
            payload,
        } => Value::Variant {
            tycon: tycon.clone(),
            ctor: ctor.clone(),
            payload: payload
                .map(|tid| migrate_thunk_id(tid, src_range, dst_env_id, thunk_map, env_map, arena)),
        },
        Value::Proxy { handler } => Value::Proxy {
            handler: migrate_thunk_id(*handler, src_range, dst_env_id, thunk_map, env_map, arena),
        },
        Value::Overlay(l, r) => Value::Overlay(
            migrate_thunk_id(*l, src_range, dst_env_id, thunk_map, env_map, arena),
            migrate_thunk_id(*r, src_range, dst_env_id, thunk_map, env_map, arena),
        ),
        // Value::Function carries closure_env_id: u32 — an ScopeArena index.
        // If closure_env_id is in src_range, migrate the closure Scope to the dst arena
        // so calling the function after drop does not reference dropped scopes.
        Value::Function {
            params,
            body,
            closure_env_id,
            annotation,
        } => {
            let new_closure_env_id = if src_range.contains(closure_env_id) {
                migrate_flat_env(
                    *closure_env_id,
                    src_range,
                    dst_env_id,
                    thunk_map,
                    env_map,
                    arena,
                )
            } else {
                *closure_env_id
            };
            Value::Function {
                params: params.clone(),
                body: body.clone(),
                closure_env_id: new_closure_env_id,
                annotation: annotation.clone(),
            }
        }
        // All other variants: return unchanged (primitives, seqs of non-ThunkId types, etc.)
        other => other.clone(),
    }
}

/// Translate all env_id and ThunkId fields in an UnevaluatedState from src_range to dst.
///
/// Called by `migrate_thunk_id` when a thunk is unevaluated: creates a translated clone of
/// the state with all src_range env_ids remapped through `migrate_flat_env` and all
/// src_range ThunkIds remapped through `migrate_thunk_id`.
///
/// Variants with no env_id or ThunkId fields (AstNodeField) are returned unchanged.
///
/// **Precondition:** `AnnotatedWrap` thunks must be fully materialized before the caller
/// invokes `migrate_thunk_id`. This function panics (`unreachable!`) if an unevaluated
/// `AnnotatedWrap` is encountered. The `ctx` field in `AnnotatedWrap` references the source
/// `EvalContext`; migrating it unevaluated would leave `ctx` pointing to the source arena
/// while `inner` points to the destination arena — a use-after-free class bug (B-514 / D-6).
fn translate_unevaluated_state(
    state: crate::value::UnevaluatedState,
    src_range: &std::ops::Range<u32>,
    dst_env_id: ScopeId,
    thunk_map: &mut std::collections::HashMap<ThunkId, ThunkId>,
    env_map: &mut std::collections::HashMap<u32, u32>,
    arena: &mut ScopeArena,
) -> crate::value::UnevaluatedState {
    use crate::value::UnevaluatedState;

    match state {
        UnevaluatedState::Surface {
            node,
            res,
            types,
            env_id,
            ctx,
        } => {
            let new_env_id = if src_range.contains(&env_id) {
                migrate_flat_env(env_id, src_range, dst_env_id, thunk_map, env_map, arena)
            } else {
                env_id
            };
            UnevaluatedState::Surface {
                node,
                res,
                types,
                env_id: new_env_id,
                ctx,
            }
        }
        UnevaluatedState::CoreExpr { expr, env_id, ctx } => {
            let new_env_id = if src_range.contains(&env_id) {
                migrate_flat_env(env_id, src_range, dst_env_id, thunk_map, env_map, arena)
            } else {
                env_id
            };
            UnevaluatedState::CoreExpr {
                expr,
                env_id: new_env_id,
                ctx,
            }
        }
        UnevaluatedState::BuiltinCall {
            def,
            args,
            named,
            call_span,
            caller_env_id,
            ctx,
        } => {
            let new_caller_env_id = if src_range.contains(&caller_env_id) {
                migrate_flat_env(
                    caller_env_id,
                    src_range,
                    dst_env_id,
                    thunk_map,
                    env_map,
                    arena,
                )
            } else {
                caller_env_id
            };
            let new_args = args
                .into_iter()
                .map(|tid| migrate_thunk_id(tid, src_range, dst_env_id, thunk_map, env_map, arena))
                .collect();
            let new_named = named.map(|map| {
                map.into_iter()
                    .map(|(k, tid)| {
                        let new_tid =
                            migrate_thunk_id(tid, src_range, dst_env_id, thunk_map, env_map, arena);
                        (k, new_tid)
                    })
                    .collect()
            });
            UnevaluatedState::BuiltinCall {
                def,
                args: new_args,
                named: new_named,
                call_span,
                caller_env_id: new_caller_env_id,
                ctx,
            }
        }
        UnevaluatedState::FnCall {
            func,
            args,
            named,
            call_span,
            caller_env_id,
            ctx,
            original_call,
        } => {
            let new_caller_env_id = if src_range.contains(&caller_env_id) {
                migrate_flat_env(
                    caller_env_id,
                    src_range,
                    dst_env_id,
                    thunk_map,
                    env_map,
                    arena,
                )
            } else {
                caller_env_id
            };
            let new_func = migrate_thunk_id(func, src_range, dst_env_id, thunk_map, env_map, arena);
            let new_args = args
                .into_iter()
                .map(|tid| migrate_thunk_id(tid, src_range, dst_env_id, thunk_map, env_map, arena))
                .collect();
            let new_named = named.map(|boxed_map| {
                Box::new(
                    boxed_map
                        .into_iter()
                        .map(|(k, tid)| {
                            let new_tid = migrate_thunk_id(
                                tid, src_range, dst_env_id, thunk_map, env_map, arena,
                            );
                            (k, new_tid)
                        })
                        .collect(),
                )
            });
            UnevaluatedState::FnCall {
                func: new_func,
                args: new_args,
                named: new_named,
                call_span,
                caller_env_id: new_caller_env_id,
                ctx,
                original_call,
            }
        }
        UnevaluatedState::Guarded {
            inner,
            expected,
            field_path,
            guard_span,
            blame_label,
            default,
        } => {
            let new_inner =
                migrate_thunk_id(inner, src_range, dst_env_id, thunk_map, env_map, arena);
            let new_default = default.map(|(expr, env_id)| {
                let new_env_id = if src_range.contains(&env_id) {
                    migrate_flat_env(env_id, src_range, dst_env_id, thunk_map, env_map, arena)
                } else {
                    env_id
                };
                (expr, new_env_id)
            });
            UnevaluatedState::Guarded {
                inner: new_inner,
                expected,
                field_path,
                guard_span,
                blame_label,
                default: new_default,
            }
        }
        // AstNodeField has no env_id or ThunkId fields — return as-is.
        UnevaluatedState::AstField { node, field, ctx } => {
            UnevaluatedState::AstField { node, field, ctx }
        }
        // AnnotatedWrap must never be migrated in unevaluated form.
        //
        // Precondition: all AnnotatedWrap thunks are materialized before arena migration is
        // called — the migration caller (builtin_arena_migrate / deep-materialize path) must
        // force AnnotatedWrap thunks to materialized before invoking migrate_thunk_id.
        //
        // If this arm is reached, the caller violated the precondition: an AnnotatedWrap
        // thunk was present in the arena without being forced first. The `ctx` field would
        // reference the source EvalContext while `inner` (after translation) would point to
        // the destination arena — a use-after-free class bug (same pattern as B-514 for
        // BuiltinCall/FnCall). See Finding 4 of sprint S-934.
        UnevaluatedState::AnnotatedWrap { inner, .. } => {
            unreachable!(
                "AnnotatedWrap thunk (inner={:?}) reached translate_unevaluated_state — \
                 AnnotatedWrap thunks must be fully materialized before arena migration. \
                 This is a precondition violation: the caller must force all AnnotatedWrap \
                 thunks before calling migrate_thunk_id.",
                inner,
            )
        }
    }
}

/// Migrate a single ThunkId. If in src_range, copy to dst; otherwise keep as-is.
pub fn migrate_thunk_id(
    thunk_id: ThunkId,
    src_range: &std::ops::Range<u32>,
    dst_env_id: ScopeId,
    thunk_map: &mut std::collections::HashMap<ThunkId, ThunkId>,
    env_map: &mut std::collections::HashMap<u32, u32>,
    arena: &mut ScopeArena,
) -> ThunkId {
    // ThunkIds outside src_range are permanent — no copy needed
    if !src_range.contains(&thunk_id.scope_id) {
        return thunk_id;
    }
    // Already migrated? Return the mapped ThunkId
    if let Some(&mapped) = thunk_map.get(&thunk_id) {
        return mapped;
    }
    // Get the source thunk
    let src_thunk = arena.get_thunk(thunk_id);
    // Check if materialized
    if let Some(value) = src_thunk.try_get_materialized() {
        // Two-phase cycle-safe graph copy:
        //   1. Pre-allocate a placeholder slot in dst and insert into thunk_map BEFORE recursing.
        //      This breaks any cycles: if the value's nested ThunkIds refer back to thunk_id,
        //      migrate_thunk_id will find it in thunk_map and return new_tid immediately.
        //   2. Recurse into migrate_value (safe — thunk_map already has our entry).
        //   3. Fill the placeholder with the fully-migrated value via settle.
        let placeholder = Arc::new(crate::value::Thunk::placeholder(src_thunk.span.clone()));
        let new_tid = arena.push_slot(dst_env_id, Arc::clone(&placeholder));
        thunk_map.insert(thunk_id, new_tid);
        // Recurse now that thunk_map has the cycle-breaking entry.
        let migrated_value =
            migrate_value(&value, src_range, dst_env_id, thunk_map, env_map, arena);
        // Fill the placeholder (same Arc already in the slot) with the migrated value.
        placeholder.settle(Ok(migrated_value));
        new_tid
    } else {
        // Unevaluated thunk — translate all env_id / ThunkId fields in the UnevaluatedState
        // so the migrated thunk does not reference dropped Scopes after arena-drop src.
        //
        // Two-phase cycle-safe pattern (same as the materialized branch above):
        //   1. Pre-allocate a placeholder in dst_env_id and insert into thunk_map BEFORE
        //      translating the state. This prevents infinite recursion if the state's own
        //      ThunkId fields refer back to this thunk.
        //   2. Translate the UnevaluatedState — all env_ids and ThunkIds in src_range are
        //      remapped through migrate_flat_env / migrate_thunk_id.
        //   3. Replace the placeholder's unevaluated field with the translated state.
        let placeholder = Arc::new(crate::value::Thunk::placeholder(src_thunk.span.clone()));
        let new_tid = arena.push_slot(dst_env_id, Arc::clone(&placeholder));
        thunk_map.insert(thunk_id, new_tid);

        if let Some(state) = src_thunk.try_claim() {
            let translated = translate_unevaluated_state(
                state, src_range, dst_env_id, thunk_map, env_map, arena,
            );
            // Write the translated state into the placeholder thunk.
            // placeholder() sets unevaluated=None; we must set it to Some(translated).
            placeholder.reset(translated);
        }
        // If state is None here (concurrent materialization between the two checks),
        // the placeholder stays as a placeholder — harmless because the caller should
        // re-check try_get_materialized; and in practice arena migration is single-threaded.
        new_tid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust_span;
    use crate::value::{Thunk, Value};

    fn test_span() -> crate::ast::Span {
        rust_span!()
    }

    #[test]
    fn test_scope_arena_alloc_get() {
        let mut arena = ScopeArena::new();
        let id = arena.alloc_root(0);
        assert_eq!(arena.scopes[id.0 as usize].len(), 0);
        assert!(arena.scopes[id.0 as usize].parent.is_none());
    }

    #[test]
    fn test_scope_arena_multiple_allocs() {
        let mut arena = ScopeArena::new();
        let id1 = arena.alloc_root(1);
        let id2 = arena.alloc_root(2);
        let id3 = arena.alloc_root(3);

        // ScopeId values should be distinct
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_push_slot_and_get_thunk() {
        let mut arena = ScopeArena::new();
        let env_id = arena.alloc_root(0);

        let thunk = Arc::new(Thunk::value(Value::Int(42), test_span()));
        let id = arena.push_slot(env_id, Arc::clone(&thunk));

        assert_eq!(id.scope_id, env_id.0);
        assert_eq!(id.slot, 0);

        let retrieved = arena.get_thunk(id);
        assert_eq!(retrieved.try_get_materialized(), Some(Value::Int(42)));
    }

    #[test]
    fn test_push_slot_multiple() {
        let mut arena = ScopeArena::new();
        let env_id = arena.alloc_root(0);

        let t1 = Arc::new(Thunk::value(Value::Int(1), test_span()));
        let t2 = Arc::new(Thunk::value(Value::Int(2), test_span()));
        let t3 = Arc::new(Thunk::value(Value::Int(3), test_span()));

        let id1 = arena.push_slot(env_id, Arc::clone(&t1));
        let id2 = arena.push_slot(env_id, Arc::clone(&t2));
        let id3 = arena.push_slot(env_id, Arc::clone(&t3));

        assert_eq!(id1.slot, 0);
        assert_eq!(id2.slot, 1);
        assert_eq!(id3.slot, 2);

        assert_eq!(
            arena.get_thunk(id1).try_get_materialized(),
            Some(Value::Int(1))
        );
        assert_eq!(
            arena.get_thunk(id2).try_get_materialized(),
            Some(Value::Int(2))
        );
        assert_eq!(
            arena.get_thunk(id3).try_get_materialized(),
            Some(Value::Int(3))
        );
    }

    #[test]
    fn test_fill_slot_via_reserve_and_fill() {
        let mut arena = ScopeArena::new();
        let env_id = arena.alloc_root(0);

        // Reserve a slot, then fill it from another scope's ThunkId.
        let slot_idx = arena.reserve_slot(env_id);

        // Allocate a source thunk in a separate scope.
        let src_env = arena.alloc_root(0);
        let thunk = Arc::new(Thunk::value(Value::Int(99), test_span()));
        let src_tid = arena.push_slot(src_env, Arc::clone(&thunk));

        arena.fill_slot(env_id, slot_idx, src_tid);

        let tid = ThunkId {
            scope_id: env_id.0,
            slot: 0,
        };
        let retrieved = arena.get_thunk(tid);
        assert_eq!(retrieved.try_get_materialized(), Some(Value::Int(99)));
    }

    #[test]
    fn test_scope_arena_parent_chain() {
        let mut scope_arena = ScopeArena::new();

        // Create a root scope (no parent)
        let root_id = scope_arena.alloc_root(0);

        // Create a child scope with root as parent
        let child_id = scope_arena.alloc_child(root_id, 0);

        let child = scope_arena.get(child_id);
        assert_eq!(child.parent, Some(root_id));

        let root = scope_arena.get(root_id);
        assert_eq!(root.parent, None);

        // Verify parent chain via walk_parent_chain
        // From child: 0 hops → child itself; 1 hop → root
        assert_eq!(scope_arena.walk_parent_chain(child_id.0, 0), Ok(child_id));
        assert_eq!(scope_arena.walk_parent_chain(child_id.0, 1), Ok(root_id));
        assert!(scope_arena.walk_parent_chain(child_id.0, 2).is_err());
        // From root: 0 hops → root itself; 1 hop → out of chain
        assert_eq!(scope_arena.walk_parent_chain(root_id.0, 0), Ok(root_id));
        assert!(scope_arena.walk_parent_chain(root_id.0, 1).is_err());
    }

    #[test]
    fn test_scope_push_and_get() {
        let mut arena = ScopeArena::new();
        let id = arena.alloc_root(3);

        let t0 = Arc::new(Thunk::value(Value::Int(10), rust_span!()));
        let t1 = Arc::new(Thunk::value(Value::Int(20), rust_span!()));
        let t2 = Arc::new(Thunk::value(Value::Int(30), rust_span!()));

        let tid0 = arena.push_slot(id, Arc::clone(&t0));
        let tid1 = arena.push_slot(id, Arc::clone(&t1));
        let tid2 = arena.push_slot(id, Arc::clone(&t2));

        assert_eq!(
            arena.get_thunk(tid0).try_get_materialized(),
            Some(Value::Int(10))
        );
        assert_eq!(
            arena.get_thunk(tid1).try_get_materialized(),
            Some(Value::Int(20))
        );
        assert_eq!(
            arena.get_thunk(tid2).try_get_materialized(),
            Some(Value::Int(30))
        );
        // Slot 3 is out of bounds (only 3 slots pushed).
        let env = arena.get(id);
        assert!(env.get(3).is_none());
    }

    #[test]
    fn test_reserve_slot_sequential() {
        let mut arena = ScopeArena::new();
        let id = arena.alloc_root(0);

        // Reserve three named slots without filling them.
        let s0 = arena.reserve_slot(id);
        let s1 = arena.reserve_slot(id);
        let s2 = arena.reserve_slot(id);
        assert_eq!(s0, 0);
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);

        // All slots are None (unfilled).
        let env = arena.get(id);
        assert!(env.get(0).is_none());
        assert!(env.get(1).is_none());
        assert!(env.get(2).is_none());
    }

    #[test]
    fn test_thunk_id_struct_copy() {
        let id = ThunkId {
            scope_id: 3,
            slot: 7,
        };
        let id_copy = id; // Copy — not moved
        assert_eq!(id, id_copy);
        assert_eq!(id.scope_id, 3);
        assert_eq!(id.slot, 7);
    }

    #[test]
    fn test_scope_id_copy_semantics() {
        let id = ScopeId(456);
        let id_copy = id; // Should be a Copy, not a move

        assert_eq!(id, id_copy);
        assert_eq!(id.0, 456);
        assert_eq!(id_copy.0, 456);
    }

    #[test]
    fn test_drop_scope() {
        let mut arena = ScopeArena::new();
        let env_id = arena.alloc_root(0);

        let thunk = Arc::new(Thunk::value(Value::Int(42), test_span()));
        let id = arena.push_slot(env_id, Arc::clone(&thunk));

        // Verify thunk is present
        assert_eq!(
            arena.get_thunk(id).try_get_materialized(),
            Some(Value::Int(42))
        );

        // Drop the scope
        arena.drop_scope(env_id);

        // Slots should be cleared
        assert!(arena.get_mut(env_id).slots.is_empty());
    }

    #[test]
    fn test_empty_arena() {
        let scope_arena = ScopeArena::new();
        // Empty arena should be safe (no panics on construction)
        assert_eq!(scope_arena.scopes.len(), 0);
    }

    #[test]
    fn test_push_slot_returns_stable_thunkid() {
        let mut arena = ScopeArena::new();
        let env_id = arena.alloc_root(0);
        let thunk1 = Arc::new(crate::value::Thunk::value(
            crate::value::Value::Int(1),
            rust_span!(),
        ));
        let thunk2 = Arc::new(crate::value::Thunk::value(
            crate::value::Value::Int(2),
            rust_span!(),
        ));
        let id1 = arena.push_slot(env_id, thunk1);
        let id2 = arena.push_slot(env_id, thunk2);
        assert_ne!(id1, id2);
        assert_eq!(id1.slot, 0);
        assert_eq!(id2.slot, 1);
        assert_eq!(
            arena.get_thunk(id1).try_get_materialized(),
            Some(crate::value::Value::Int(1))
        );
        assert_eq!(
            arena.get_thunk(id2).try_get_materialized(),
            Some(crate::value::Value::Int(2))
        );
    }

    #[test]
    fn test_drop_scope_clears_slots() {
        let mut arena = ScopeArena::new();
        let env_id = arena.alloc_root(0);
        let thunk = Arc::new(crate::value::Thunk::value(
            crate::value::Value::Int(42),
            rust_span!(),
        ));
        let _id = arena.push_slot(env_id, Arc::clone(&thunk));
        assert_eq!(arena.scopes[env_id.0 as usize].slots.len(), 1);
        arena.drop_scope(env_id);
        assert_eq!(arena.scopes[env_id.0 as usize].slots.len(), 0);
    }

    #[test]
    fn test_drop_scope_leaves_other_scopes_intact() {
        let mut arena = ScopeArena::new();
        let env0 = arena.alloc_root(0);
        let env1 = arena.alloc_child(env0, 0);
        let thunk = Arc::new(crate::value::Thunk::value(
            crate::value::Value::Int(99),
            rust_span!(),
        ));
        let id = arena.push_slot(env1, thunk);
        arena.drop_scope(env0);
        // env1 still intact
        assert_eq!(
            arena.get_thunk(id).try_get_materialized(),
            Some(crate::value::Value::Int(99))
        );
    }

    #[test]
    fn test_reserve_and_fill_slot() {
        // Verify that reserve_slot + fill_slot work correctly with push_slot in one Scope.
        let mut arena = ScopeArena::new();
        let env_id = arena.alloc_root(0);

        // Push a named slot first (slot 0).
        let named_thunk = Arc::new(Thunk::value(Value::Int(100), rust_span!()));
        let src_id = arena.push_slot(env_id, Arc::clone(&named_thunk));
        assert_eq!(src_id.slot, 0);

        // Reserve slot 1 for a letrec-style binding, filled from a second scope.
        let slot1 = arena.reserve_slot(env_id);
        assert_eq!(slot1, 1);

        let env2 = arena.alloc_root(0);
        let src2_id = arena.push_slot(env2, Arc::new(Thunk::value(Value::Int(200), rust_span!())));

        // Fill slot 1 of env_id from env2 slot 0.
        arena.fill_slot(env_id, slot1, src2_id);

        // Push another named slot (slot 2).
        let anon_thunk = Arc::new(Thunk::value(Value::Int(300), rust_span!()));
        let anon_id = arena.push_slot(env_id, anon_thunk);

        // All three ThunkIds must be distinct and retrieve the correct values.
        assert_eq!(
            arena.get_thunk(src_id).try_get_materialized(),
            Some(Value::Int(100))
        );
        assert_eq!(
            arena.get_thunk(src2_id).try_get_materialized(),
            Some(Value::Int(200))
        );
        assert_eq!(
            arena.get_thunk(anon_id).try_get_materialized(),
            Some(Value::Int(300))
        );
        // env_id slot 1 was filled via fill_slot from env2 slot 0.
        let letrec_thunk_id = ThunkId {
            scope_id: env_id.0,
            slot: 1,
        };
        assert_eq!(
            arena.get_thunk(letrec_thunk_id).try_get_materialized(),
            Some(Value::Int(200))
        );
        // anon_id landed in env_id slot 2 (slot 0 = named, slot 1 = reserved+filled, slot 2 = pushed).
        assert_eq!(anon_id.scope_id, env_id.0);
        assert_eq!(anon_id.slot, 2);
    }

    #[test]
    fn test_parent_chain_depth_three() {
        let mut arena = ScopeArena::new();
        let root = arena.alloc_root(0);
        let child = arena.alloc_child(root, 0);
        let grandchild = arena.alloc_child(child, 0);

        // Parent pointers
        assert_eq!(arena.scopes[root.0 as usize].parent, None);
        assert_eq!(arena.scopes[child.0 as usize].parent, Some(root));
        assert_eq!(arena.scopes[grandchild.0 as usize].parent, Some(child));

        // walk_parent_chain from grandchild: 0→grandchild, 1→child, 2→root, 3→err
        assert_eq!(arena.walk_parent_chain(grandchild.0, 0), Ok(grandchild));
        assert_eq!(arena.walk_parent_chain(grandchild.0, 1), Ok(child));
        assert_eq!(arena.walk_parent_chain(grandchild.0, 2), Ok(root));
        assert!(arena.walk_parent_chain(grandchild.0, 3).is_err());

        // collect_parent_chain from grandchild: outermost first
        assert_eq!(
            arena.collect_parent_chain(grandchild.0),
            vec![root, child, grandchild]
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "use-after-free")]
    fn test_use_after_free_panics() {
        let mut arena = ScopeArena::new();
        let env_id = arena.alloc_root(0);
        let thunk = Arc::new(crate::value::Thunk::value(
            crate::value::Value::Int(1),
            rust_span!(),
        ));
        let id = arena.push_slot(env_id, thunk);
        arena.drop_scope(env_id);
        let _ = arena.get_thunk(id); // should panic
    }

    #[tokio::test]
    async fn test_placeholder_force_errors() {
        use crate::eval::materialize;
        use crate::eval::EvalContext;

        // Create a placeholder thunk (unfilled)
        let thunk = Arc::new(Thunk::placeholder(rust_span!()));

        // Create a minimal test context
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        let ctx = EvalContext::new(base_dir, false);

        // After the runtime-v2 ThunkInner representation change, placeholder thunks
        // (unevaluated=None, result=None) are indistinguishable from InProgress thunks
        // at runtime. Forcing a placeholder now returns a circular_dependency Err.
        let result = materialize(&thunk, None, &ctx).await;
        assert!(
            result.is_err(),
            "materializing an unfilled placeholder should fail, got Ok"
        );
    }

    #[test]
    fn test_migrate_dict_value() {
        use crate::value::{HashableValue, Value};
        use std::collections::HashMap;

        let mut arena = ScopeArena::new();
        // Create source scope
        let src_env_id = arena.alloc_root(0);
        // Allocate two dict entries in the source scope
        let key1 = HashableValue::Str("a".into());
        let key2 = HashableValue::Str("b".into());
        let thunk1 = Arc::new(Thunk::value(Value::Int(1), test_span()));
        let thunk2 = Arc::new(Thunk::value(Value::Int(2), test_span()));
        let tid1 = arena.push_slot(src_env_id, thunk1);
        let tid2 = arena.push_slot(src_env_id, thunk2);

        let mut dict_map = indexmap::IndexMap::new();
        dict_map.insert(key1.clone(), tid1);
        dict_map.insert(key2.clone(), tid2);
        let dict_value = Value::Dict(dict_map);

        // Create destination scope
        let dst_env_id = arena.alloc_root(0);

        // Migrate the dict value
        let src_range = src_env_id.0..(src_env_id.0 + 1);
        let mut thunk_map = HashMap::new();
        let mut env_map = HashMap::new();
        let migrated_value = migrate_value(
            &dict_value,
            &src_range,
            dst_env_id,
            &mut thunk_map,
            &mut env_map,
            &mut arena,
        );

        // Extract the migrated dict and verify ThunkIds point to dst_env_id
        if let Value::Dict(migrated_map) = &migrated_value {
            let migrated_tid1 = migrated_map.get(&key1).expect("key 'a' missing");
            let migrated_tid2 = migrated_map.get(&key2).expect("key 'b' missing");
            assert_eq!(
                migrated_tid1.scope_id, dst_env_id.0,
                "migrated thunk1 should be in dst scope"
            );
            assert_eq!(
                migrated_tid2.scope_id, dst_env_id.0,
                "migrated thunk2 should be in dst scope"
            );

            // Verify values are still accessible
            let val1 = arena.get_thunk(*migrated_tid1).try_get_materialized();
            let val2 = arena.get_thunk(*migrated_tid2).try_get_materialized();
            assert_eq!(val1, Some(Value::Int(1)), "migrated value 'a' should be 1");
            assert_eq!(val2, Some(Value::Int(2)), "migrated value 'b' should be 2");
        } else {
            panic!("migrate_value should return a Dict");
        }

        // Drop the source scope
        arena.drop_scope(src_env_id);

        // Verify dst dict entries are still accessible after source drop
        if let Value::Dict(migrated_map) = &migrated_value {
            let migrated_tid1 = migrated_map.get(&key1).expect("key 'a' missing after drop");
            let val1 = arena.get_thunk(*migrated_tid1).try_get_materialized();
            assert_eq!(
                val1,
                Some(Value::Int(1)),
                "value should still be accessible after source drop"
            );
        }
    }

    /// Verify that migrate_value correctly translates Value::Function.closure_env_id when
    /// the closure's env_id falls within the source range.
    ///
    /// This is the unit test for the core T-1573 path: migrating a function whose closure
    /// scope is inside the source arena so that the closure_env_id is remapped to a new
    /// Scope in the destination scope.
    #[test]
    fn test_migrate_function_closure_env_id() {
        use crate::ast::{CoreExpr, Param};
        use crate::value::Value;
        use std::collections::HashMap;
        use std::sync::Arc;

        let mut arena = ScopeArena::new();

        // dst_env_id: the destination root scope (allocated before src so it is outside src_range)
        let dst_env_id = arena.alloc_root(0);

        // src_env_id: the source scope where the closure "lives"
        let src_env_id = arena.alloc_root(0);

        // A minimal function value whose closure_env_id is the source scope.
        // The body is a trivial literal; only the closure_env_id matters for this test.
        let body_expr = Arc::new(crate::ast::Spanned {
            node: CoreExpr::Int(42),
            span: rust_span!(),
        });
        let fn_value = Value::Function {
            params: std::rc::Rc::new(vec![Param {
                name: "x".to_string(),
                annotation: None,
                variadic: false,
                slot: 0,
                resolved_type: None,
            }]),
            body: body_expr,
            closure_env_id: src_env_id.0,
            annotation: None,
        };

        // Source range covers src_env_id
        let src_range = src_env_id.0..(src_env_id.0 + 1);
        let mut thunk_map = HashMap::new();
        let mut env_map = HashMap::new();

        let migrated = migrate_value(
            &fn_value,
            &src_range,
            dst_env_id,
            &mut thunk_map,
            &mut env_map,
            &mut arena,
        );

        // The migrated function must have a closure_env_id outside src_range (pointing to
        // the new Scope allocated by migrate_flat_env in the destination).
        match migrated {
            Value::Function { closure_env_id, .. } => {
                assert!(
                    !src_range.contains(&closure_env_id),
                    "migrated closure_env_id ({closure_env_id}) must be outside src_range {src_range:?}"
                );
                // The new closure_env_id must be recorded in env_map
                let expected_new = *env_map.get(&src_env_id.0).expect(
                    "env_map must contain a mapping for the source closure env_id after migration",
                );
                assert_eq!(
                    closure_env_id, expected_new,
                    "migrated closure_env_id must match the env_map entry for the source scope"
                );
            }
            other => panic!(
                "migrate_value of Function should return Function, got {:?}",
                other
            ),
        }
    }

    /// Verify that migrate_thunk_id correctly translates the env_id inside a CoreExpr
    /// unevaluated state when the thunk is in src_range.
    ///
    /// Before the fix, unevaluated thunks were Arc::clone'd — leaving stale env_ids
    /// pointing into the source arena. After the fix, translate_unevaluated_state rewrites
    /// the env_id through migrate_flat_env so the migrated thunk references a valid Scope
    /// in the destination arena.
    #[test]
    fn test_migrate_unevaluated_thunk_env_id_translation() {
        use crate::value::{Thunk, UnevaluatedState, Value};
        use std::collections::HashMap;
        use std::sync::Arc;

        // We need a minimal EvalContext to create unevaluated thunks.
        // Use a placeholder approach: construct the UnevaluatedState directly via
        // Thunk::guarded (which has no env_id) to test the Guarded path,
        // and verify the inner ThunkId is translated.

        let mut arena = ScopeArena::new();

        // dst_env_id: destination scope (outside src_range)
        let dst_env_id = arena.alloc_root(0);
        // src_env_id: source scope (in src_range)
        let src_env_id = arena.alloc_root(0);

        let src_range = src_env_id.0..(src_env_id.0 + 1);

        // Allocate a materialized thunk in src_env_id (the inner target for Guarded).
        let inner_thunk = Arc::new(Thunk::value(Value::Int(77), rust_span!()));
        let inner_tid = arena.push_slot(src_env_id, Arc::clone(&inner_thunk));
        assert!(
            src_range.contains(&inner_tid.scope_id),
            "inner_tid must be in src_range for this test"
        );

        // Create a Guarded thunk in src_env_id whose inner ThunkId points to inner_tid.
        let guarded_thunk = Arc::new(Thunk::guarded(
            inner_tid,
            crate::types::Type::Int,
            vec![],
            rust_span!(),
            None,
            None,
        ));
        let guarded_tid = arena.push_slot(src_env_id, Arc::clone(&guarded_thunk));

        // Migrate guarded_tid from src to dst.
        let mut thunk_map = HashMap::new();
        let mut env_map = HashMap::new();
        let new_tid = migrate_thunk_id(
            guarded_tid,
            &src_range,
            dst_env_id,
            &mut thunk_map,
            &mut env_map,
            &mut arena,
        );

        // The migrated ThunkId must be in dst_env_id.
        assert_eq!(
            new_tid.scope_id, dst_env_id.0,
            "migrated guarded thunk must be in dst"
        );

        // The migrated thunk must have its inner ThunkId remapped to dst.
        let migrated_arc = arena.get_thunk(new_tid);
        let state = migrated_arc
            .try_claim()
            .expect("migrated thunk must still be unevaluated after migration (not forced)");
        match &state {
            UnevaluatedState::Guarded { inner, .. } => {
                assert!(
                    !src_range.contains(&inner.scope_id),
                    "migrated Guarded.inner ({:?}) must be outside src_range after translation",
                    inner
                );
                assert_eq!(
                    inner.scope_id, dst_env_id.0,
                    "migrated Guarded.inner must point to dst_env_id"
                );
                // The inner thunk at the translated location must hold the materialized value.
                let inner_arc = arena.get_thunk(*inner);
                assert_eq!(
                    inner_arc.try_get_materialized(),
                    Some(Value::Int(77)),
                    "inner thunk value must survive migration"
                );
            }
            other => panic!(
                "expected Guarded state after migration, got {:?}",
                std::mem::discriminant(other)
            ),
        }
        migrated_arc.reset(state);

        // Drop the source scope — the migrated thunk must remain valid.
        arena.drop_scope(src_env_id);

        // Verify the migrated thunk's inner is still readable from dst (no use-after-free).
        let migrated_arc2 = arena.get_thunk(new_tid);
        let state2 = migrated_arc2
            .try_claim()
            .expect("migrated thunk must still be unevaluated after source drop");
        match state2 {
            UnevaluatedState::Guarded { inner, .. } => {
                let inner_arc2 = arena.get_thunk(inner);
                assert_eq!(
                    inner_arc2.try_get_materialized(),
                    Some(Value::Int(77)),
                    "inner thunk value must be readable after source-drop (no use-after-free)"
                );
            }
            other => panic!(
                "expected Guarded state after source drop, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_free_list_reuse_bounds_scopes_len() {
        let mut arena = ScopeArena::new();

        // Alloc and drop a scope; its slot should go to the free list.
        let id0 = arena.alloc_root(0);
        assert_eq!(arena.scopes.len(), 1);
        arena.drop_scope(id0);
        assert_eq!(arena.free_list.len(), 1);

        // Next alloc should reuse the freed slot, not grow scopes.
        let id1 = arena.alloc_root(0);
        assert_eq!(
            arena.scopes.len(),
            1,
            "scopes.len() must stay 1 after reuse"
        );
        assert_eq!(
            arena.free_list.len(),
            0,
            "free_list should be empty after pop"
        );
        assert_eq!(id1.0, id0.0, "reused scope should have same index");

        // Alloc two, drop one, alloc one more — still bounded.
        let id2 = arena.alloc_root(0);
        assert_eq!(arena.scopes.len(), 2);
        arena.drop_scope(id1);
        let id3 = arena.alloc_root(0);
        assert_eq!(
            arena.scopes.len(),
            2,
            "scopes.len() must stay 2 after reuse"
        );
        assert_eq!(id3.0, id1.0, "reused slot index must match dropped id");
        let _ = id2; // keep id2 alive to verify it wasn't corrupted
    }

    #[test]
    fn test_drop_range_and_truncate_no_free_list_corruption() {
        // Regression test: drop_range_and_truncate must NOT push dropped ScopeIds
        // onto free_list, because scopes.truncate() removes them from the Vec.
        // If free_list contained stale indices >= start_env_id, subsequent
        // alloc_root/alloc_child would index out of bounds and panic.
        let mut arena = ScopeArena::new();

        let root = arena.alloc_root(0);
        let child1 = arena.alloc_child(root, 0);
        let child2 = arena.alloc_child(root, 0);
        assert_eq!(arena.scopes.len(), 3);

        // Simulate builtin-arena-drop: drop child1..end and truncate.
        arena.drop_range_and_truncate(child1.0);
        assert_eq!(
            arena.scopes.len(),
            1,
            "after truncate(child1.0), scopes.len() must be 1"
        );
        assert_eq!(
            arena.free_list.len(),
            0,
            "drop_range_and_truncate must not populate free_list with truncated IDs"
        );

        // This alloc must not panic (would panic if free_list had stale [1, 2]).
        let new_id = arena.alloc_root(0);
        assert_eq!(
            new_id.0, 1,
            "new alloc grows past root (no stale free_list entries)"
        );
        assert_eq!(arena.scopes.len(), 2);

        let _ = (child1, child2); // suppress unused warnings
    }

    #[test]
    fn test_drop_range_and_truncate_cleans_stale_free_list_entries() {
        // Regression test: drop_range_and_truncate must remove pre-existing free_list entries
        // that point into the truncation range. If drop_scope was called on a scope that is
        // later inside the truncation range, its ScopeId would remain on the free_list and
        // become an out-of-bounds index after scopes.truncate(). The retain() call in
        // drop_range_and_truncate is the safety net for this case.
        let mut arena = ScopeArena::new();

        let root = arena.alloc_root(0);
        let child1 = arena.alloc_child(root, 0);
        let child2 = arena.alloc_child(root, 0);
        assert_eq!(arena.scopes.len(), 3);

        // drop_scope on child2 — pushes ScopeId(2) onto free_list.
        // This simulates a caller that dropped an individual scope before deciding to
        // truncate the whole range starting at child1.
        arena.drop_scope(child2);
        assert_eq!(arena.free_list.len(), 1, "child2 should be on free_list");
        assert_eq!(arena.free_list[0].0, child2.0, "free_list entry must be child2");

        // Now truncate starting at child1 — this must remove child2's stale entry from
        // free_list before truncating, otherwise alloc_root/alloc_child would index
        // out of bounds on the next call.
        arena.drop_range_and_truncate(child1.0);
        assert_eq!(
            arena.scopes.len(),
            1,
            "after truncate(child1.0), scopes.len() must be 1"
        );
        assert_eq!(
            arena.free_list.len(),
            0,
            "retain must have removed the stale child2 entry from free_list"
        );

        // This alloc must not panic (would panic if free_list still had stale child2 entry).
        let new_id = arena.alloc_root(0);
        assert_eq!(
            new_id.0, 1,
            "new alloc grows past root (stale free_list entry was cleaned)"
        );
        assert_eq!(arena.scopes.len(), 2);

        let _ = child1; // suppress unused warning
    }
}
