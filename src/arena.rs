//! Arena allocation for thunks and environments.
//!
//! This module provides index-based arenas for environments, replacing the
//! `Arc<RwLock<Environment>>` chain model with `EnvId` handles that index into
//! `Vec<FlatEnv>` backing stores. Thunks are owned directly by `FlatEnv` slots,
//! addressed by `ThunkId { env_id, slot }`.

use std::sync::Arc;

/// A handle to a thunk in the arena. Copy-cheap (8 bytes).
/// `env_id` indexes into `EnvArena.envs`; `slot` indexes into that FlatEnv's slots.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThunkId {
    /// Which FlatEnv owns this thunk (absolute index into EnvArena.envs).
    pub env_id: u32,
    /// Which slot within that FlatEnv.
    pub slot: u32,
}

/// A handle to an environment in the arena. Copy-cheap (4 bytes), indexes into `EnvArena`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EnvId(pub u32);

/// Arena for environment allocation. Stores `FlatEnv` indexed by `EnvId`.
///
/// `FlatEnv` slots directly own `Arc<Thunk>` values — thunks are addressed by
/// `ThunkId { env_id, slot }` which indexes into the owning FlatEnv.
#[derive(Debug)]
pub(crate) struct EnvArena {
    pub(crate) envs: Vec<FlatEnv>,
}

impl EnvArena {
    /// Create a new empty environment arena.
    pub fn new() -> Self {
        Self { envs: Vec::new() }
    }

    /// Allocate a root environment (no parent) with the given slot capacity.
    ///
    /// The display vector is initialized to contain only the new environment's own EnvId.
    pub fn alloc_root(&mut self, slot_count: usize) -> EnvId {
        let len = self.envs.len();
        assert!(
            len < u32::MAX as usize,
            "EnvArena overflow: more than {} environments allocated",
            u32::MAX
        );
        let id = EnvId(len as u32);
        let env = FlatEnv {
            slots: Vec::with_capacity(slot_count),
            display: vec![id],
            alloc_count: std::sync::atomic::AtomicU64::new(0),
        };
        self.envs.push(env);
        id
    }

    /// Allocate a child environment with the given parent.
    ///
    /// The display vector is cloned from the parent and extended with the new environment's EnvId.
    pub fn alloc_child(&mut self, parent_id: EnvId, slot_count: usize) -> EnvId {
        let len = self.envs.len();
        assert!(
            len < u32::MAX as usize,
            "EnvArena overflow: more than {} environments allocated",
            u32::MAX
        );
        let id = EnvId(len as u32);

        // Clone parent's display vector and extend with self
        let parent_display = self.envs[parent_id.0 as usize].display.clone();
        let mut display = parent_display;
        display.push(id);

        let env = FlatEnv {
            slots: Vec::with_capacity(slot_count),
            display,
            alloc_count: std::sync::atomic::AtomicU64::new(0),
        };
        self.envs.push(env);
        id
    }

    /// Fill a pre-allocated slot with an Arc<Thunk>.
    #[cfg(test)]
    pub fn fill_slot_thunk(&mut self, id: ThunkId, thunk: Arc<crate::value::Thunk>) {
        let env = &mut self.envs[EnvId(id.env_id).0 as usize];
        let slot_idx = id.slot as usize;
        if slot_idx >= env.slots.len() {
            env.slots.resize_with(slot_idx + 1, || None);
        }
        env.slots[slot_idx] = Some(thunk);
    }

    /// Fill a pre-allocated letrec slot with a thunk (for dict letrec scoping).
    ///
    /// The slot must have been pre-allocated via `alloc_child`. This method fills
    /// slot `slot_idx` in the environment identified by `env_id` with the given thunk.
    /// Used by `eval_dict_core` to batch-fill letrec slots after all entries are created,
    /// and by `bind_args_thunks` to fill function call frame parameter slots.
    pub fn fill_letrec_slot(&mut self, env_id: EnvId, slot_idx: u32, thunk_id: ThunkId) {
        let thunk = {
            // Get the thunk from the ThunkId's source env. The thunk was just allocated
            // in this same arena, so the lookup is always valid.
            let src_env = &self.envs[thunk_id.env_id as usize];
            src_env.slots[thunk_id.slot as usize]
                .clone()
                .expect("fill_letrec_slot: source ThunkId points to an unfilled slot — alloc_slot_thunk must be called before fill_letrec_slot")
        };
        let env = &mut self.envs[env_id.0 as usize];
        let idx = slot_idx as usize;
        // Extend slots if needed (letrec group may not have pre-sized)
        if idx >= env.slots.len() {
            env.slots.resize_with(idx + 1, || None);
        }
        env.slots[idx] = Some(thunk);
    }

    /// Allocate a new anonymous slot in an existing scope, returning its ThunkId.
    pub fn alloc_slot_thunk(&mut self, env_id: EnvId, thunk: Arc<crate::value::Thunk>) -> ThunkId {
        let env = &mut self.envs[env_id.0 as usize];
        let slot = env.slots.len() as u32;
        env.slots.push(Some(thunk));
        env.alloc_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ThunkId {
            env_id: env_id.0,
            slot,
        }
    }

    /// Get a thunk by ThunkId.
    ///
    /// # Panics
    ///
    /// Panics if the ThunkId is out of bounds or points to a dropped slot.
    pub fn get_thunk(&self, id: ThunkId) -> Arc<crate::value::Thunk> {
        let env = &self.envs[EnvId(id.env_id).0 as usize];
        env.slots
            .get(id.slot as usize)
            .expect("use-after-free: ThunkId accessed after arena scope was dropped")
            .as_ref()
            .expect("use-after-free: ThunkId slot is empty after scope drop")
            .clone()
    }

    /// Drop a scope: clear all thunk slots, freeing Arc<Thunk> references.
    pub fn drop_scope(&mut self, env_id: EnvId) {
        let env = &mut self.envs[env_id.0 as usize];
        env.slots.clear();
    }


    /// Get a reference to the environment at the given handle.
    #[cfg(test)]
    fn get(&self, id: EnvId) -> &FlatEnv {
        &self.envs[id.0 as usize]
    }

    /// Get a mutable reference to the environment at the given handle.
    #[cfg(test)]
    pub fn get_mut(&mut self, id: EnvId) -> &mut FlatEnv {
        &mut self.envs[id.0 as usize]
    }
}

impl Default for EnvArena {
    fn default() -> Self {
        Self::new()
    }
}

/// Flat environment representation: O(1) slot-based variable lookup.
///
/// Slots directly own `Arc<Thunk>` values. Variables are assigned `(level, slot)` pairs
/// by the variable resolution pass (de Bruijn levels), allowing direct indexing.
///
/// **Display vector:** Prepopulated at creation with the `EnvId` of every ancestor
/// scope from level 0 to current level. This enables true O(1) access via
/// `display[level].slots[slot]` without walking the parent chain.
/// The display vector encodes the full parent chain: `display[display.len()-2]` is the
/// immediate parent, `display[0]` is the root. No separate `parent` field is needed.
#[derive(Debug)]
pub(crate) struct FlatEnv {
    /// Named slots (dict entries, function params) + anonymous slots.
    /// Indexed by slot number. Named slots filled via fill_slot_thunk; anonymous via alloc_slot_thunk.
    pub(crate) slots: Vec<Option<Arc<crate::value::Thunk>>>,
    /// Display vector: prepopulated at creation with ancestor EnvIds indexed by level.
    /// For a scope at level `k`, `display[0..k]` contains the EnvIds of all outer scopes,
    /// and `display[k]` is self. Enables O(1) variable lookup.
    pub(crate) display: Vec<EnvId>,
    /// Stats counters for builtin-arena-stats.
    pub(crate) alloc_count: std::sync::atomic::AtomicU64,
}

impl FlatEnv {
    /// Get a thunk by slot index (static key, assigned by the resolver).
    ///
    /// Returns `None` if the slot is out of bounds or unfilled.
    #[cfg(test)]
    pub fn get_slot(&self, slot: u32) -> Option<Arc<crate::value::Thunk>> {
        self.slots.get(slot as usize).and_then(|opt| opt.clone())
    }

    /// Insert a thunk into a slot. Extends the slot vec if necessary.
    ///
    /// Used during FlatEnv construction when filling slots in order.
    #[cfg(test)]
    pub fn set_slot(&mut self, slot: u32, thunk: Arc<crate::value::Thunk>) {
        let slot_idx = slot as usize;
        if slot_idx >= self.slots.len() {
            self.slots.resize_with(slot_idx + 1, || None);
        }
        self.slots[slot_idx] = Some(thunk);
    }

    /// Get the parent environment ID, if any.
    /// The parent is encoded in the display vector: `display[display.len()-2]` is the immediate
    /// parent for scopes with depth > 1; root scopes (depth 1) have no parent.
    #[cfg(test)]
    pub fn parent(&self) -> Option<EnvId> {
        if self.display.len() > 1 {
            Some(self.display[self.display.len() - 2])
        } else {
            None
        }
    }
}

/// Migrate a FlatEnv from the source range to the destination scope.
///
/// Allocates a new FlatEnv in the destination arena, migrates all slot ThunkIds,
/// and recursively migrates any parent FlatEnvs in the source range (via the display chain).
///
/// The two-phase pattern is applied at the FlatEnv level too: the new destination EnvId is
/// inserted into `env_map` BEFORE slots are migrated, so that cycles (e.g., a slot value
/// containing a closure referencing this same FlatEnv) do not loop infinitely.
///
/// Returns the destination EnvId for the migrated FlatEnv.
pub fn migrate_flat_env(
    src_env_id_u32: u32,
    src_range: &std::ops::Range<u32>,
    dst_env_id: EnvId,
    thunk_map: &mut std::collections::HashMap<ThunkId, ThunkId>,
    env_map: &mut std::collections::HashMap<u32, u32>,
    arena: &mut EnvArena,
) -> u32 {
    // Already migrated? Return cached result.
    if let Some(&mapped) = env_map.get(&src_env_id_u32) {
        return mapped;
    }

    // Collect the slot count and display vector of the source FlatEnv.
    let (slot_count, src_display) = {
        let src_env = &arena.envs[src_env_id_u32 as usize];
        (src_env.slots.len(), src_env.display.clone())
    };

    // Allocate the new destination FlatEnv.
    // Use alloc_root (no parent linkage needed — the display vector is rebuilt from migrated chain).
    let new_env_id = arena.alloc_root(slot_count);

    // Pre-insert into env_map BEFORE migrating slots (two-phase cycle safety at env level).
    env_map.insert(src_env_id_u32, new_env_id.0);

    // Migrate each slot ThunkId from the source FlatEnv into new_env_id.
    //
    // Thunks are allocated directly into new_env_id (not dst_env_id), so each source
    // FlatEnv maps 1:1 to exactly one destination FlatEnv. This eliminates ThunkId
    // aliasing that would arise if two different env_ids held the same logical thunk.
    //
    // Two-phase per-slot: first allocate all placeholder slots in new_env_id (establishing
    // the correct slot indices and inserting into thunk_map), then recurse to fill values.
    // This preserves slot ordering even when migrated values contain nested ThunkIds that
    // trigger additional allocations into other FlatEnvs.

    // Phase 1: allocate placeholders and populate thunk_map for all filled slots.
    // None slots get a None pushed to preserve slot numbering.
    let src_thunks: Vec<Option<Arc<crate::value::Thunk>>> = {
        (0..slot_count)
            .map(|i| arena.envs[src_env_id_u32 as usize].slots[i].clone())
            .collect()
    };
    for (slot_idx, slot_opt) in src_thunks.iter().enumerate() {
        let src_tid = ThunkId { env_id: src_env_id_u32, slot: slot_idx as u32 };
        match slot_opt {
            None => {
                // Preserve slot index by pushing None.
                arena.envs[new_env_id.0 as usize].slots.push(None);
            }
            Some(src_arc) => {
                // Allocate a placeholder into new_env_id at the next slot (= slot_idx).
                let placeholder = Arc::new(crate::value::Thunk::new_placeholder(src_arc.span.clone()));
                let new_tid = arena.alloc_slot_thunk(new_env_id, Arc::clone(&placeholder));
                debug_assert_eq!(new_tid.slot, slot_idx as u32, "slot ordering must match source");
                // Insert into thunk_map BEFORE recursing (cycle-safe at env level).
                thunk_map.insert(src_tid, new_tid);
            }
        }
    }

    // Phase 2: fill each placeholder by recursing into migrate_value or translating
    // unevaluated state. The placeholder Arc is already in new_env_id.slots[slot_idx]
    // and registered in thunk_map from Phase 1; we write into it here.
    for (slot_idx, slot_opt) in src_thunks.iter().enumerate() {
        if let Some(src_arc) = slot_opt {
            let placeholder = arena.envs[new_env_id.0 as usize].slots[slot_idx]
                .clone()
                .expect("placeholder must exist after phase 1");
            if let Some(value) = src_arc.try_get_materialized() {
                // Materialized: migrate the value recursively.
                let migrated = migrate_value(&value, src_range, dst_env_id, thunk_map, env_map, arena);
                placeholder.set_materialized(migrated);
            } else if let Some(state) = src_arc.peek_unevaluated_state() {
                // Unevaluated: translate all env_id / ThunkId fields so the migrated thunk
                // does not reference dropped FlatEnvs after arena-drop src.
                let translated = translate_unevaluated_state(
                    state,
                    src_range,
                    dst_env_id,
                    thunk_map,
                    env_map,
                    arena,
                );
                placeholder.restore_unevaluated(translated);
            }
            // If state is None (InProgress/concurrent transition), the placeholder stays
            // as-is — forcing it will return a circular_dependency error, which is correct
            // since an InProgress thunk has no stable content to migrate.
        }
    }

    // Rebuild the display vector: translate each FlatEnv in the source display chain.
    // FlatEnvs in src_range are migrated recursively; FlatEnvs outside src_range are permanent.
    let new_display: Vec<EnvId> = src_display.iter().map(|&d| {
        if src_range.contains(&d.0) {
            // Recursively migrate (env_map prevents infinite recursion)
            let migrated_id = migrate_flat_env(d.0, src_range, dst_env_id, thunk_map, env_map, arena);
            EnvId(migrated_id)
        } else {
            // Outside src_range — permanent, keep as-is
            d
        }
    }).collect();

    // Set the rebuilt display vector on the new FlatEnv.
    // Override the one set by alloc_root (which is just [new_env_id]).
    arena.envs[new_env_id.0 as usize].display = new_display;

    new_env_id.0
}

/// Recursively migrate a Value from source env_id range to destination scope.
///
/// ThunkIds in [src_range.start, src_range.end) are copied to dst; others are permanent.
/// `Value::Function.closure_env_id` values in src_range are translated via `env_map`,
/// which maps source FlatEnv EnvIds to their destination counterparts.
pub fn migrate_value(
    value: &crate::value::Value,
    src_range: &std::ops::Range<u32>,
    dst_env_id: EnvId,
    thunk_map: &mut std::collections::HashMap<ThunkId, ThunkId>,
    env_map: &mut std::collections::HashMap<u32, u32>,
    arena: &mut EnvArena,
) -> crate::value::Value {
    use crate::value::Value;
    match value {
        Value::Dict(map) => {
            let mut new_map = indexmap::IndexMap::with_capacity(map.len());
            for (key, &thunk_id) in map.iter() {
                let new_tid = migrate_thunk_id(thunk_id, src_range, dst_env_id, thunk_map, env_map, arena);
                new_map.insert(key.clone(), new_tid);
            }
            Value::Dict(new_map)
        }
        Value::Seq { head, tail } => Value::Seq {
            head: migrate_thunk_id(*head, src_range, dst_env_id, thunk_map, env_map, arena),
            tail: migrate_thunk_id(*tail, src_range, dst_env_id, thunk_map, env_map, arena),
        },
        Value::Variant { tag, payload } => Value::Variant {
            tag: tag.clone(),
            payload: payload.map(|tid| migrate_thunk_id(tid, src_range, dst_env_id, thunk_map, env_map, arena)),
        },
        Value::Proxy { handler } => Value::Proxy {
            handler: migrate_thunk_id(*handler, src_range, dst_env_id, thunk_map, env_map, arena),
        },
        Value::Overlay(l, r) => Value::Overlay(
            migrate_thunk_id(*l, src_range, dst_env_id, thunk_map, env_map, arena),
            migrate_thunk_id(*r, src_range, dst_env_id, thunk_map, env_map, arena),
        ),
        // Value::Function carries closure_env_id: u32 — an EnvArena index.
        // If closure_env_id is in src_range, migrate the closure FlatEnv to the dst arena
        // so calling the function after drop does not reference dropped scopes.
        Value::Function { params, body, closure_env_id, annotation } => {
            let new_closure_env_id = if src_range.contains(closure_env_id) {
                migrate_flat_env(*closure_env_id, src_range, dst_env_id, thunk_map, env_map, arena)
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
fn translate_unevaluated_state(
    state: crate::value::UnevaluatedState,
    src_range: &std::ops::Range<u32>,
    dst_env_id: EnvId,
    thunk_map: &mut std::collections::HashMap<ThunkId, ThunkId>,
    env_map: &mut std::collections::HashMap<u32, u32>,
    arena: &mut EnvArena,
) -> crate::value::UnevaluatedState {
    use crate::value::UnevaluatedState;

    match state {
        UnevaluatedState::Surface { node, res, types, env_id, ctx } => {
            let new_env_id = if src_range.contains(&env_id) {
                migrate_flat_env(env_id, src_range, dst_env_id, thunk_map, env_map, arena)
            } else {
                env_id
            };
            UnevaluatedState::Surface { node, res, types, env_id: new_env_id, ctx }
        }
        UnevaluatedState::CoreExpr { expr, env_id, ctx } => {
            let new_env_id = if src_range.contains(&env_id) {
                migrate_flat_env(env_id, src_range, dst_env_id, thunk_map, env_map, arena)
            } else {
                env_id
            };
            UnevaluatedState::CoreExpr { expr, env_id: new_env_id, ctx }
        }
        UnevaluatedState::Builtin { def, args, named, call_span, caller_env_id, ctx } => {
            let new_caller_env_id = if src_range.contains(&caller_env_id) {
                migrate_flat_env(caller_env_id, src_range, dst_env_id, thunk_map, env_map, arena)
            } else {
                caller_env_id
            };
            let new_args = args.into_iter()
                .map(|tid| migrate_thunk_id(tid, src_range, dst_env_id, thunk_map, env_map, arena))
                .collect();
            let new_named = named.map(|map| {
                map.into_iter()
                    .map(|(k, tid)| {
                        let new_tid = migrate_thunk_id(tid, src_range, dst_env_id, thunk_map, env_map, arena);
                        (k, new_tid)
                    })
                    .collect()
            });
            UnevaluatedState::Builtin {
                def,
                args: new_args,
                named: new_named,
                call_span,
                caller_env_id: new_caller_env_id,
                ctx,
            }
        }
        UnevaluatedState::Call { func, args, named, call_span, caller_env_id, ctx, original_call } => {
            let new_caller_env_id = if src_range.contains(&caller_env_id) {
                migrate_flat_env(caller_env_id, src_range, dst_env_id, thunk_map, env_map, arena)
            } else {
                caller_env_id
            };
            let new_func = migrate_thunk_id(func, src_range, dst_env_id, thunk_map, env_map, arena);
            let new_args = args.into_iter()
                .map(|tid| migrate_thunk_id(tid, src_range, dst_env_id, thunk_map, env_map, arena))
                .collect();
            let new_named = named.map(|boxed_map| {
                Box::new(boxed_map.into_iter()
                    .map(|(k, tid)| {
                        let new_tid = migrate_thunk_id(tid, src_range, dst_env_id, thunk_map, env_map, arena);
                        (k, new_tid)
                    })
                    .collect())
            });
            UnevaluatedState::Call {
                func: new_func,
                args: new_args,
                named: new_named,
                call_span,
                caller_env_id: new_caller_env_id,
                ctx,
                original_call,
            }
        }
        UnevaluatedState::Guarded { inner, expected, field_path, guard_span, blame_label, default } => {
            let new_inner = migrate_thunk_id(inner, src_range, dst_env_id, thunk_map, env_map, arena);
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
        UnevaluatedState::AstNodeField { node, field, ctx } => {
            UnevaluatedState::AstNodeField { node, field, ctx }
        }
    }
}

/// Migrate a single ThunkId. If in src_range, copy to dst; otherwise keep as-is.
pub fn migrate_thunk_id(
    thunk_id: ThunkId,
    src_range: &std::ops::Range<u32>,
    dst_env_id: EnvId,
    thunk_map: &mut std::collections::HashMap<ThunkId, ThunkId>,
    env_map: &mut std::collections::HashMap<u32, u32>,
    arena: &mut EnvArena,
) -> ThunkId {
    // ThunkIds outside src_range are permanent — no copy needed
    if !src_range.contains(&thunk_id.env_id) {
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
        //   3. Fill the placeholder with the fully-migrated value via set_materialized.
        let placeholder = Arc::new(crate::value::Thunk::new_placeholder(src_thunk.span.clone()));
        let new_tid = arena.alloc_slot_thunk(dst_env_id, Arc::clone(&placeholder));
        thunk_map.insert(thunk_id, new_tid);
        // Recurse now that thunk_map has the cycle-breaking entry.
        let migrated_value = migrate_value(&value, src_range, dst_env_id, thunk_map, env_map, arena);
        // Fill the placeholder (same Arc already in the slot) with the migrated value.
        placeholder.set_materialized(migrated_value);
        new_tid
    } else {
        // Unevaluated thunk — translate all env_id / ThunkId fields in the UnevaluatedState
        // so the migrated thunk does not reference dropped FlatEnvs after arena-drop src.
        //
        // Two-phase cycle-safe pattern (same as the materialized branch above):
        //   1. Pre-allocate a placeholder in dst_env_id and insert into thunk_map BEFORE
        //      translating the state. This prevents infinite recursion if the state's own
        //      ThunkId fields refer back to this thunk.
        //   2. Translate the UnevaluatedState — all env_ids and ThunkIds in src_range are
        //      remapped through migrate_flat_env / migrate_thunk_id.
        //   3. Replace the placeholder's unevaluated field with the translated state.
        let placeholder = Arc::new(crate::value::Thunk::new_placeholder(src_thunk.span.clone()));
        let new_tid = arena.alloc_slot_thunk(dst_env_id, Arc::clone(&placeholder));
        thunk_map.insert(thunk_id, new_tid);

        // Peek the unevaluated state WITHOUT consuming it (source thunk stays intact).
        // If None (InProgress/Materialized/Failed), the thunk is already handled above;
        // a concurrent transition between our try_get_materialized check and this peek
        // is safe — worst case we create a placeholder that stays as-is (safe: the
        // Arc<Thunk> result channel is set atomically by the materializer on the original).
        if let Some(state) = src_thunk.peek_unevaluated_state() {
            let translated = translate_unevaluated_state(
                state,
                src_range,
                dst_env_id,
                thunk_map,
                env_map,
                arena,
            );
            // Write the translated state into the placeholder thunk.
            // new_placeholder() sets unevaluated=None; we must set it to Some(translated).
            placeholder.restore_unevaluated(translated);
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
    fn test_env_arena_alloc_get() {
        let mut arena = EnvArena::new();
        let id = arena.alloc_root(0);
        let retrieved = arena.get(id);
        assert_eq!(retrieved.slots.len(), 0);
        assert!(retrieved.parent().is_none());
        assert_eq!(retrieved.display, vec![id]);
    }

    #[test]
    fn test_env_arena_multiple_allocs() {
        let mut arena = EnvArena::new();
        let id1 = arena.alloc_root(1);
        let id2 = arena.alloc_root(2);
        let id3 = arena.alloc_root(3);

        assert_eq!(arena.get(id1).slots.capacity(), 1);
        assert_eq!(arena.get(id2).slots.capacity(), 2);
        assert_eq!(arena.get(id3).slots.capacity(), 3);

        // EnvId values should be distinct
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_alloc_slot_thunk_and_get_thunk() {
        let mut arena = EnvArena::new();
        let env_id = arena.alloc_root(0);

        let thunk = Arc::new(Thunk::new_materialized(Value::Int(42), test_span()));
        let id = arena.alloc_slot_thunk(env_id, Arc::clone(&thunk));

        assert_eq!(id.env_id, env_id.0);
        assert_eq!(id.slot, 0);

        let retrieved = arena.get_thunk(id);
        assert_eq!(retrieved.try_get_materialized(), Some(Value::Int(42)));
    }

    #[test]
    fn test_alloc_slot_thunk_multiple() {
        let mut arena = EnvArena::new();
        let env_id = arena.alloc_root(0);

        let t1 = Arc::new(Thunk::new_materialized(Value::Int(1), test_span()));
        let t2 = Arc::new(Thunk::new_materialized(Value::Int(2), test_span()));
        let t3 = Arc::new(Thunk::new_materialized(Value::Int(3), test_span()));

        let id1 = arena.alloc_slot_thunk(env_id, Arc::clone(&t1));
        let id2 = arena.alloc_slot_thunk(env_id, Arc::clone(&t2));
        let id3 = arena.alloc_slot_thunk(env_id, Arc::clone(&t3));

        assert_eq!(id1.slot, 0);
        assert_eq!(id2.slot, 1);
        assert_eq!(id3.slot, 2);

        assert_eq!(arena.get_thunk(id1).try_get_materialized(), Some(Value::Int(1)));
        assert_eq!(arena.get_thunk(id2).try_get_materialized(), Some(Value::Int(2)));
        assert_eq!(arena.get_thunk(id3).try_get_materialized(), Some(Value::Int(3)));
    }

    #[test]
    fn test_fill_slot_thunk() {
        let mut arena = EnvArena::new();
        let env_id = arena.alloc_root(3);

        // Pre-allocate a ThunkId (env_id=0, slot=0)
        let tid = ThunkId { env_id: env_id.0, slot: 0 };
        let thunk = Arc::new(Thunk::new_materialized(Value::Int(99), test_span()));
        arena.fill_slot_thunk(tid, Arc::clone(&thunk));

        let retrieved = arena.get_thunk(tid);
        assert_eq!(retrieved.try_get_materialized(), Some(Value::Int(99)));
    }

    #[test]
    fn test_flat_env_parent_chain() {
        let mut env_arena = EnvArena::new();

        // Create a root env (no parent)
        let root_id = env_arena.alloc_root(0);

        // Create a child env with root as parent
        let child_id = env_arena.alloc_child(root_id, 0);

        let child = env_arena.get(child_id);
        assert_eq!(child.parent(), Some(root_id));

        let root = env_arena.get(root_id);
        assert_eq!(root.parent(), None);

        // Verify display vectors
        assert_eq!(root.display, vec![root_id]);
        assert_eq!(child.display, vec![root_id, child_id]);
    }

    #[test]
    fn test_flat_env_set_slot() {
        let mut arena = EnvArena::new();
        let id = arena.alloc_root(3);
        let env = arena.get_mut(id);

        let t0 = Arc::new(Thunk::new_materialized(Value::Int(10), rust_span!()));
        let t1 = Arc::new(Thunk::new_materialized(Value::Int(20), rust_span!()));
        let t2 = Arc::new(Thunk::new_materialized(Value::Int(30), rust_span!()));

        env.set_slot(0, Arc::clone(&t0));
        env.set_slot(1, Arc::clone(&t1));
        env.set_slot(2, Arc::clone(&t2));

        assert_eq!(env.get_slot(0).and_then(|t| t.try_get_materialized()), Some(Value::Int(10)));
        assert_eq!(env.get_slot(1).and_then(|t| t.try_get_materialized()), Some(Value::Int(20)));
        assert_eq!(env.get_slot(2).and_then(|t| t.try_get_materialized()), Some(Value::Int(30)));
        assert!(env.get_slot(3).is_none());
    }

    #[test]
    fn test_set_slot_extends_vec() {
        let mut arena = EnvArena::new();
        let id = arena.alloc_root(0);
        let env = arena.get_mut(id);

        // Setting slot 5 should extend the vec to hold slots 0..=5
        let thunk = Arc::new(Thunk::new_materialized(Value::Int(42), rust_span!()));
        env.set_slot(5, Arc::clone(&thunk));

        assert!(env.slots.len() >= 6);
        assert!(env.get_slot(5).is_some());
    }

    #[test]
    fn test_thunk_id_struct_copy() {
        let id = ThunkId { env_id: 3, slot: 7 };
        let id_copy = id; // Copy — not moved
        assert_eq!(id, id_copy);
        assert_eq!(id.env_id, 3);
        assert_eq!(id.slot, 7);
    }

    #[test]
    fn test_env_id_copy_semantics() {
        let id = EnvId(456);
        let id_copy = id; // Should be a Copy, not a move

        assert_eq!(id, id_copy);
        assert_eq!(id.0, 456);
        assert_eq!(id_copy.0, 456);
    }

    #[test]
    fn test_scope_alloc_count() {
        let mut arena = EnvArena::new();
        let env_id = arena.alloc_root(0);

        assert_eq!(arena.envs[env_id.0 as usize].alloc_count.load(std::sync::atomic::Ordering::Relaxed), 0);

        let t1 = Arc::new(Thunk::new_materialized(Value::Int(1), test_span()));
        let t2 = Arc::new(Thunk::new_materialized(Value::Int(2), test_span()));
        arena.alloc_slot_thunk(env_id, t1);
        arena.alloc_slot_thunk(env_id, t2);

        assert_eq!(arena.envs[env_id.0 as usize].alloc_count.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn test_drop_scope() {
        let mut arena = EnvArena::new();
        let env_id = arena.alloc_root(0);

        let thunk = Arc::new(Thunk::new_materialized(Value::Int(42), test_span()));
        let id = arena.alloc_slot_thunk(env_id, Arc::clone(&thunk));

        // Verify thunk is present
        assert_eq!(arena.get_thunk(id).try_get_materialized(), Some(Value::Int(42)));

        // Drop the scope
        arena.drop_scope(env_id);

        // Slots should be cleared
        assert!(arena.get_mut(env_id).slots.is_empty());
    }

    #[test]
    fn test_empty_arena() {
        let env_arena = EnvArena::new();
        // Empty arena should be safe (no panics on construction)
        assert_eq!(env_arena.envs.len(), 0);
    }

    #[test]
    fn test_alloc_slot_returns_stable_thunkid() {
        let mut arena = EnvArena::new();
        let env_id = arena.alloc_root(0);
        let thunk1 = Arc::new(crate::value::Thunk::new_materialized(crate::value::Value::Int(1), rust_span!()));
        let thunk2 = Arc::new(crate::value::Thunk::new_materialized(crate::value::Value::Int(2), rust_span!()));
        let id1 = arena.alloc_slot_thunk(env_id, thunk1);
        let id2 = arena.alloc_slot_thunk(env_id, thunk2);
        assert_ne!(id1, id2);
        assert_eq!(id1.slot, 0);
        assert_eq!(id2.slot, 1);
        assert_eq!(arena.get_thunk(id1).try_get_materialized(), Some(crate::value::Value::Int(1)));
        assert_eq!(arena.get_thunk(id2).try_get_materialized(), Some(crate::value::Value::Int(2)));
    }

    #[test]
    fn test_drop_scope_clears_slots() {
        let mut arena = EnvArena::new();
        let env_id = arena.alloc_root(0);
        let thunk = Arc::new(crate::value::Thunk::new_materialized(crate::value::Value::Int(42), rust_span!()));
        let _id = arena.alloc_slot_thunk(env_id, Arc::clone(&thunk));
        assert_eq!(arena.envs[env_id.0 as usize].slots.len(), 1);
        arena.drop_scope(env_id);
        assert_eq!(arena.envs[env_id.0 as usize].slots.len(), 0);
    }

    #[test]
    fn test_drop_scope_leaves_other_scopes_intact() {
        let mut arena = EnvArena::new();
        let env0 = arena.alloc_root(0);
        let env1 = arena.alloc_child(env0, 0);
        let thunk = Arc::new(crate::value::Thunk::new_materialized(crate::value::Value::Int(99), rust_span!()));
        let id = arena.alloc_slot_thunk(env1, thunk);
        arena.drop_scope(env0);
        // env1 still intact
        assert_eq!(arena.get_thunk(id).try_get_materialized(), Some(crate::value::Value::Int(99)));
    }

    #[test]
    fn test_fill_slot_before_alloc_slot() {
        // Verify that fill_letrec_slot (named/static slots) and alloc_slot_thunk
        // (anonymous/dynamic slots) coexist correctly in one FlatEnv.
        let mut arena = EnvArena::new();
        let env_id = arena.alloc_root(0);

        // Allocate an anonymous slot to serve as the thunk source for fill_letrec_slot.
        let named_thunk = Arc::new(Thunk::new_materialized(Value::Int(100), rust_span!()));
        let src_id = arena.alloc_slot_thunk(env_id, Arc::clone(&named_thunk));

        // Fill slot 0 of the same env with the thunk via fill_letrec_slot.
        // fill_letrec_slot reads the thunk from src_id's slot and writes it to slot 0.
        // Since src_id.slot == 0 already, use a second env to hold the canonical src thunk.
        let env2 = arena.alloc_root(0);
        let src2_id = arena.alloc_slot_thunk(env2, Arc::new(Thunk::new_materialized(Value::Int(200), rust_span!())));

        // Fill slot 1 of env_id from env2 slot 0
        arena.fill_letrec_slot(env_id, 1, src2_id);

        // Also allocate an anonymous slot after the letrec fill
        let anon_thunk = Arc::new(Thunk::new_materialized(Value::Int(300), rust_span!()));
        let anon_id = arena.alloc_slot_thunk(env_id, anon_thunk);

        // All three ThunkIds must be distinct and retrieve the correct values.
        assert_eq!(arena.get_thunk(src_id).try_get_materialized(), Some(Value::Int(100)));
        assert_eq!(arena.get_thunk(src2_id).try_get_materialized(), Some(Value::Int(200)));
        assert_eq!(arena.get_thunk(anon_id).try_get_materialized(), Some(Value::Int(300)));
        // env_id slot 1 was filled via fill_letrec_slot from env2 slot 0
        let letrec_thunk_id = ThunkId { env_id: env_id.0, slot: 1 };
        assert_eq!(arena.get_thunk(letrec_thunk_id).try_get_materialized(), Some(Value::Int(200)));
        // anon_id landed in env_id slot 2 (after slot 0 from src_id alloc, slot 1 from letrec fill)
        assert_eq!(anon_id.env_id, env_id.0);
        assert_eq!(anon_id.slot, 2);
    }

    #[test]
    fn test_display_vector_depth_three() {
        let mut arena = EnvArena::new();
        let root = arena.alloc_root(0);
        let child = arena.alloc_child(root, 0);
        let grandchild = arena.alloc_child(child, 0);
        assert_eq!(arena.envs[grandchild.0 as usize].display, vec![root, child, grandchild]);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "use-after-free")]
    fn test_use_after_free_panics() {
        let mut arena = EnvArena::new();
        let env_id = arena.alloc_root(0);
        let thunk = Arc::new(crate::value::Thunk::new_materialized(crate::value::Value::Int(1), rust_span!()));
        let id = arena.alloc_slot_thunk(env_id, thunk);
        arena.drop_scope(env_id);
        let _ = arena.get_thunk(id); // should panic
    }

    #[tokio::test]
    async fn test_placeholder_force_errors() {
        use crate::eval::materialize;
        use crate::eval::EvalContext;
        use crate::env::Env;

        // Create a placeholder thunk (unfilled)
        let thunk = Arc::new(Thunk::new_placeholder(rust_span!()));

        // Create a minimal test context
        let env = Arc::new(std::sync::RwLock::new(Env::new()));
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        let ctx = EvalContext::new(base_dir, Arc::clone(&env), Arc::clone(&env), false);

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

        let mut arena = EnvArena::new();
        // Create source scope
        let src_env_id = arena.alloc_root(0);
        // Allocate two dict entries in the source scope
        let key1 = HashableValue::Str("a".into());
        let key2 = HashableValue::Str("b".into());
        let thunk1 = Arc::new(Thunk::new_materialized(Value::Int(1), test_span()));
        let thunk2 = Arc::new(Thunk::new_materialized(Value::Int(2), test_span()));
        let tid1 = arena.alloc_slot_thunk(src_env_id, thunk1);
        let tid2 = arena.alloc_slot_thunk(src_env_id, thunk2);

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
        let migrated_value = migrate_value(&dict_value, &src_range, dst_env_id, &mut thunk_map, &mut env_map, &mut arena);

        // Extract the migrated dict and verify ThunkIds point to dst_env_id
        if let Value::Dict(migrated_map) = &migrated_value {
            let migrated_tid1 = migrated_map.get(&key1).expect("key 'a' missing");
            let migrated_tid2 = migrated_map.get(&key2).expect("key 'b' missing");
            assert_eq!(migrated_tid1.env_id, dst_env_id.0, "migrated thunk1 should be in dst scope");
            assert_eq!(migrated_tid2.env_id, dst_env_id.0, "migrated thunk2 should be in dst scope");

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
            assert_eq!(val1, Some(Value::Int(1)), "value should still be accessible after source drop");
        }
    }

    /// Verify that migrate_value correctly translates Value::Function.closure_env_id when
    /// the closure's env_id falls within the source range.
    ///
    /// This is the unit test for the core T-1573 path: migrating a function whose closure
    /// scope is inside the source arena so that the closure_env_id is remapped to a new
    /// FlatEnv in the destination scope.
    #[test]
    fn test_migrate_function_closure_env_id() {
        use crate::ast::{CoreExpr, Param};
        use crate::value::Value;
        use std::collections::HashMap;
        use std::sync::Arc;

        let mut arena = EnvArena::new();

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
            params: std::rc::Rc::new(vec![Param { name: "x".to_string(), annotation: None, variadic: false }]),
            body: body_expr,
            closure_env_id: src_env_id.0,
            annotation: None,
        };

        // Source range covers src_env_id
        let src_range = src_env_id.0..(src_env_id.0 + 1);
        let mut thunk_map = HashMap::new();
        let mut env_map = HashMap::new();

        let migrated = migrate_value(&fn_value, &src_range, dst_env_id, &mut thunk_map, &mut env_map, &mut arena);

        // The migrated function must have a closure_env_id outside src_range (pointing to
        // the new FlatEnv allocated by migrate_flat_env in the destination).
        match migrated {
            Value::Function { closure_env_id, .. } => {
                assert!(
                    !src_range.contains(&closure_env_id),
                    "migrated closure_env_id ({closure_env_id}) must be outside src_range {src_range:?}"
                );
                // The new closure_env_id must be recorded in env_map
                let expected_new = *env_map.get(&src_env_id.0)
                    .expect("env_map must contain a mapping for the source closure env_id after migration");
                assert_eq!(
                    closure_env_id, expected_new,
                    "migrated closure_env_id must match the env_map entry for the source scope"
                );
            }
            other => panic!("migrate_value of Function should return Function, got {:?}", other),
        }
    }

    /// Verify that migrate_thunk_id correctly translates the env_id inside a CoreExpr
    /// unevaluated state when the thunk is in src_range.
    ///
    /// Before the fix, unevaluated thunks were Arc::clone'd — leaving stale env_ids
    /// pointing into the source arena. After the fix, translate_unevaluated_state rewrites
    /// the env_id through migrate_flat_env so the migrated thunk references a valid FlatEnv
    /// in the destination arena.
    #[test]
    fn test_migrate_unevaluated_thunk_env_id_translation() {
        use crate::value::{Thunk, UnevaluatedState, Value};
        use std::collections::HashMap;
        use std::sync::Arc;

        // We need a minimal EvalContext to create unevaluated thunks.
        // Use a placeholder approach: construct the UnevaluatedState directly via
        // Thunk::new_guarded (which has no env_id) to test the Guarded path,
        // and verify the inner ThunkId is translated.

        let mut arena = EnvArena::new();

        // dst_env_id: destination scope (outside src_range)
        let dst_env_id = arena.alloc_root(0);
        // src_env_id: source scope (in src_range)
        let src_env_id = arena.alloc_root(0);

        let src_range = src_env_id.0..(src_env_id.0 + 1);

        // Allocate a materialized thunk in src_env_id (the inner target for Guarded).
        let inner_thunk = Arc::new(Thunk::new_materialized(Value::Int(77), rust_span!()));
        let inner_tid = arena.alloc_slot_thunk(src_env_id, Arc::clone(&inner_thunk));
        assert!(src_range.contains(&inner_tid.env_id), "inner_tid must be in src_range for this test");

        // Create a Guarded thunk in src_env_id whose inner ThunkId points to inner_tid.
        let guarded_thunk = Arc::new(Thunk::new_guarded(
            inner_tid,
            crate::types::Type::Int,
            vec![],
            rust_span!(),
        ));
        let guarded_tid = arena.alloc_slot_thunk(src_env_id, Arc::clone(&guarded_thunk));

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
        assert_eq!(new_tid.env_id, dst_env_id.0, "migrated guarded thunk must be in dst");

        // The migrated thunk must have its inner ThunkId remapped to dst.
        let migrated_arc = arena.get_thunk(new_tid);
        let state = migrated_arc
            .peek_unevaluated_state()
            .expect("migrated thunk must still be unevaluated after migration (not forced)");
        match state {
            UnevaluatedState::Guarded { inner, .. } => {
                assert!(
                    !src_range.contains(&inner.env_id),
                    "migrated Guarded.inner ({:?}) must be outside src_range after translation",
                    inner
                );
                assert_eq!(
                    inner.env_id, dst_env_id.0,
                    "migrated Guarded.inner must point to dst_env_id"
                );
                // The inner thunk at the translated location must hold the materialized value.
                let inner_arc = arena.get_thunk(inner);
                assert_eq!(
                    inner_arc.try_get_materialized(),
                    Some(Value::Int(77)),
                    "inner thunk value must survive migration"
                );
            }
            other => panic!("expected Guarded state after migration, got {:?}", std::mem::discriminant(&other)),
        }

        // Drop the source scope — the migrated thunk must remain valid.
        arena.drop_scope(src_env_id);

        // Verify the migrated thunk's inner is still readable from dst (no use-after-free).
        let migrated_arc2 = arena.get_thunk(new_tid);
        let state2 = migrated_arc2
            .peek_unevaluated_state()
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
            other => panic!("expected Guarded state after source drop, got {:?}", std::mem::discriminant(&other)),
        }
    }
}
