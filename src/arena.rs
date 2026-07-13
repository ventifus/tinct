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

    /// Number of thunks ever allocated in a scope.
    pub fn scope_alloc_count(&self, env_id: EnvId) -> u64 {
        self.envs[env_id.0 as usize]
            .alloc_count
            .load(std::sync::atomic::Ordering::Relaxed)
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

        assert_eq!(arena.scope_alloc_count(env_id), 0);

        let t1 = Arc::new(Thunk::new_materialized(Value::Int(1), test_span()));
        let t2 = Arc::new(Thunk::new_materialized(Value::Int(2), test_span()));
        arena.alloc_slot_thunk(env_id, t1);
        arena.alloc_slot_thunk(env_id, t2);

        assert_eq!(arena.scope_alloc_count(env_id), 2);
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
}
