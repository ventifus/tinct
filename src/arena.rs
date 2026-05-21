//! Arena allocation for thunks and environments (Phase 2 of arena allocation strategy).
//!
//! This module provides index-based arenas for thunks and environments, replacing the
//! `Arc<Thunk>` / `Arc<RwLock<Environment>>` model with `ThunkId` / `EnvId` handles
//! that index into `Vec<Arc<Thunk>>` / `Vec<FlatEnv>` backing stores.
//!
//! For now (Phase 2), the arena stores `Arc<Thunk>` values — the migration from `Rc`
//! to direct ownership happens in Phase 3 (`arena-eval`). This phase establishes the
//! arena API and the `ThunkId` / `EnvId` handle types.

use std::collections::HashMap;
use std::sync::Arc;

use crate::value::Thunk;

#[cfg(test)]
use crate::ast::Span;

/// A handle to a thunk in the arena. Copy-cheap (4 bytes), indexes into `ThunkArena`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThunkId(u32);

/// A handle to an environment in the arena. Copy-cheap (4 bytes), indexes into `EnvArena`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EnvId(u32);

/// Arena for thunk allocation. Stores `Arc<Thunk>` indexed by `ThunkId`.
///
/// Phase 2 API: the arena wraps a `Vec<Arc<Thunk>>`. Phase 3 will migrate to `Vec<Thunk>`
/// for direct ownership. All public methods remain the same across phases.
#[derive(Debug)]
pub(crate) struct ThunkArena {
    thunks: Vec<Arc<Thunk>>,
}

impl ThunkArena {
    /// Create a new empty arena.
    pub fn new() -> Self {
        Self { thunks: Vec::new() }
    }

    /// Allocate a thunk in the arena, returning its handle.
    pub fn alloc(&mut self, thunk: Arc<Thunk>) -> ThunkId {
        let len = self.thunks.len();
        assert!(
            len < u32::MAX as usize,
            "ThunkArena overflow: more than {} thunks allocated",
            u32::MAX
        );
        let id = ThunkId(len as u32);
        self.thunks.push(thunk);
        id
    }

    /// Get a reference to the thunk at the given handle.
    ///
    /// # Panics
    ///
    /// Panics if the handle is out of bounds (should never happen if all IDs come from `alloc`).
    pub fn get(&self, id: ThunkId) -> &Arc<Thunk> {
        &self.thunks[id.0 as usize]
    }

    /// Allocate a placeholder thunk for letrec. The placeholder is a sentinel
    /// `ThunkState::Placeholder` that must be filled via `set_materialized()` or
    /// `set_state()` before use.
    ///
    /// The letrec pattern (internal evaluator use):
    /// 1. Pre-allocate placeholder slots for all dict entries.
    /// 2. Create the shared `FlatEnv` with those `ThunkId`s.
    /// 3. Fill each placeholder via `arena.get(id).set_materialized(...)` (requires pub(crate) access).
    ///
    /// Forcing a placeholder before filling is a logic error (letrec construction bug)
    /// and will panic at materialization time. This maintains Launchbury's monotonicity
    /// invariant: Placeholder → Unevaluated is a forward state transition.
    ///
    /// Phase 3 (arena-eval): used when the evaluator builds letrec dicts via FlatEnv.
    #[cfg(test)]
    pub fn alloc_placeholder(&mut self) -> ThunkId {
        let thunk = Arc::new(Thunk::new_placeholder(Span::origin()));
        self.alloc(thunk)
    }

    /// Create a new arena pre-populated with clones of this arena's entries.
    ///
    /// Used to give each EvalContext its own growable arena while still sharing
    /// stdlib thunks: the child arena starts with Arc::clone of every thunk in self,
    /// preserving ThunkId validity (same indices 0..N), then appends its own thunks
    /// starting at N.  Dropping the child does not affect the parent's thunks.
    pub(crate) fn clone_for_child(&self) -> Self {
        Self {
            thunks: self.thunks.iter().map(Arc::clone).collect(),
        }
    }

    /// Number of thunks currently in the arena.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.thunks.len()
    }
}

impl Default for ThunkArena {
    fn default() -> Self {
        Self::new()
    }
}

/// Arena for environment allocation. Stores `FlatEnv` indexed by `EnvId`.
///
/// Phase 3 (arena-eval): `EnvArena` and `FlatEnv` provide flat environment infrastructure.
/// `alloc_root` and `fill_letrec_slot` are called by `eval_dict` to populate FlatEnv slots
/// for dict scopes. `get` and display-vector lookup are scaffolding for the full O(1)
/// VarRef dispatch path, which is deferred until `take_unevaluated` propagates `env_id`.
#[derive(Debug)]
pub(crate) struct EnvArena {
    envs: Vec<FlatEnv>,
}

impl EnvArena {
    /// Create a new empty environment arena.
    pub fn new() -> Self {
        Self { envs: Vec::new() }
    }

    /// Allocate a root environment (no parent) with the given slot capacity.
    ///
    /// The display vector is initialized to contain only the new environment's own EnvId.
    #[allow(dead_code)] // arena-phase3 scaffolding: will be used for dict scope allocation
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
            overflow: HashMap::new(),
            parent: None,
            display: vec![id],
        };
        self.envs.push(env);
        id
    }

    /// Allocate a child environment with the given parent.
    ///
    /// The display vector is cloned from the parent and extended with the new environment's EnvId.
    ///
    /// Allocate a child FlatEnv with the given parent (arena-phase3).
    /// Used by function call to create param-binding scopes.
    pub fn alloc_child(&mut self, parent_id: EnvId, slot_count: u32) -> EnvId {
        let len = self.envs.len();
        assert!(
            len < u32::MAX as usize,
            "EnvArena overflow: more than {} environments allocated",
            u32::MAX
        );
        let id = EnvId(len as u32);

        // Clone parent's display vector and extend with self
        let parent_display = self.get(parent_id).display.clone();
        let mut display = parent_display;
        display.push(id);

        let env = FlatEnv {
            slots: Vec::with_capacity(slot_count as usize),
            overflow: HashMap::new(),
            parent: Some(parent_id),
            display,
        };
        self.envs.push(env);
        id
    }

    /// Get a reference to the environment at the given handle.
    ///
    /// # Panics
    ///
    /// Panics if the handle is out of bounds (should never happen if all IDs come from `alloc_*`).
    ///
    /// Used internally by `alloc_child` and in tests.
    fn get(&self, id: EnvId) -> &FlatEnv {
        &self.envs[id.0 as usize]
    }

    /// Get a mutable reference to the environment at the given handle.
    ///
    /// Used to fill slots after allocation (letrec pattern).
    #[cfg(test)]
    pub fn get_mut(&mut self, id: EnvId) -> &mut FlatEnv {
        &mut self.envs[id.0 as usize]
    }

    /// Get a mutable reference to the environment at the given handle (production-only path).
    ///
    /// Used internally by `fill_letrec_slot`.
    #[cfg(not(test))]
    fn get_mut(&mut self, id: EnvId) -> &mut FlatEnv {
        &mut self.envs[id.0 as usize]
    }

    /// Allocate a letrec group environment (for dict construction).
    ///
    /// Creates a new environment pre-sized for `static_key_count` slots, with all slots
    /// initially unfilled (None). The caller must fill each slot via `fill_letrec_slot`
    /// after creating the corresponding thunk.
    ///
    /// The display vector is cloned from the parent and extended with the new environment's EnvId.
    #[allow(dead_code)] // arena-phase3 will use this: when display-vector addressing is wired, this method allocates child FlatEnvs with parent linkage
    pub fn alloc_letrec_group(&mut self, static_key_count: usize, parent_id: EnvId) -> EnvId {
        self.alloc_child(parent_id, static_key_count as u32)
    }

    /// Fill a slot in a letrec environment with a ThunkId.
    ///
    /// Used during dict construction: after allocating the shared dict_env via
    /// `alloc_letrec_group`, fill each slot as its corresponding entry thunk is created.
    #[allow(dead_code)] // arena-phase3 scaffolding: will be used for dict/function scope slot filling
    pub fn fill_letrec_slot(&mut self, env_id: EnvId, slot: u32, thunk_id: ThunkId) {
        self.get_mut(env_id).set_slot(slot, thunk_id);
    }
}

impl Default for EnvArena {
    fn default() -> Self {
        Self::new()
    }
}

/// Flat environment representation: O(1) slot-based variable lookup.
///
/// Replaces the chain-based `Environment` with parent links. Variables are assigned
/// `(level, slot)` pairs by the variable resolution pass (de Bruijn levels), allowing
/// direct indexing into the `slots` vec.
///
/// **Hybrid model:** Static keys (known at parse time) use `slots` for O(1) lookup.
/// Computed keys (e.g., `[$expr: value]`) fall back to the `overflow` HashMap.
///
/// **Display vector:** Prepopulated at creation with the `EnvId` of every ancestor
/// scope from level 0 to current level. This enables true O(1) access via
/// `display[level].slots[slot]` without walking the parent chain.
#[derive(Debug)]
pub(crate) struct FlatEnv {
    /// Static keys indexed by compile-time slot number from the resolver.
    #[allow(dead_code)] // arena-phase3 scaffolding: will be used for O(1) variable lookup
    pub(crate) slots: Vec<Option<ThunkId>>,
    /// Computed keys (resolver returned None) indexed by name.
    #[allow(dead_code)] // arena-phase3: used for computed keys when wired into eval
    pub(crate) overflow: HashMap<String, ThunkId>,
    /// Parent environment for stdlib/builtins root chain. Most environments don't need this;
    /// user-scope lookups use `(level, slot)` pairs that resolve to the correct FlatEnv directly.
    #[allow(dead_code)] // arena-phase3: used for fallback lookup chain
    pub(crate) parent: Option<EnvId>,
    /// Display vector: prepopulated at creation with ancestor EnvIds indexed by level.
    /// For a scope at level `k`, `display[0..k]` contains the EnvIds of all outer scopes,
    /// and `display[k]` is self. Enables O(1) variable lookup: `arena.get(display[level]).slots[slot]`.
    #[allow(dead_code)] // arena-phase3: used for display-vector addressing
    pub(crate) display: Vec<EnvId>,
}

impl FlatEnv {
    /// Get a thunk by slot index (static key, assigned by the resolver).
    ///
    /// Returns `None` if the slot is out of bounds or unfilled.
    #[cfg(test)]
    pub fn get_slot(&self, slot: u32) -> Option<ThunkId> {
        self.slots.get(slot as usize).and_then(|&opt| opt)
    }

    /// Get a thunk by name from the overflow table (computed key, name-based lookup).
    #[cfg(test)]
    pub fn get_by_name(&self, name: &str) -> Option<ThunkId> {
        self.overflow.get(name).copied()
    }

    /// Insert a thunk into a slot. Extends the slot vec if necessary.
    ///
    /// This is used during FlatEnv construction when filling slots in order.
    /// If `slot` is beyond the current vec length, intermediate slots are filled with
    /// `None` (unfilled placeholders). Callers must not query unfilled slots.
    #[allow(dead_code)] // arena-phase3 scaffolding: called by fill_letrec_slot
    pub fn set_slot(&mut self, slot: u32, id: ThunkId) {
        let slot_idx = slot as usize;
        if slot_idx >= self.slots.len() {
            self.slots.resize(slot_idx + 1, None);
        }
        self.slots[slot_idx] = Some(id);
    }

    /// Insert a thunk into the overflow table (computed key).
    #[cfg(test)]
    pub fn insert_overflow(&mut self, name: String, id: ThunkId) {
        self.overflow.insert(name, id);
    }

    /// Get the parent environment ID, if any.
    #[cfg(test)]
    pub fn parent(&self) -> Option<EnvId> {
        self.parent
    }
}

/// Selective migration at `---` document boundaries (Phase 3 only).
///
/// **STATUS:** Not needed in Phase 2. Migration is only required when arenas own thunks
/// directly and have per-section lifetimes (Phase 3, arena-eval sprint).
///
/// **Current Phase 2 behavior:**
/// - Arena stores `Vec<Arc<Thunk>>` (Rc-wrapped, not direct ownership)
/// - Arena persists across `---` boundaries (not dropped per section)
/// - ThunkIds are stable indices that never invalidate
/// - `%` pipeline variable passes as `Arc<Thunk>` across documents (lazy, no materialization)
///
/// **When migration will be required (Phase 3):**
/// - Arena stores `Vec<Thunk>` (direct ownership, no Rc wrapper)
#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Thunk, Value};

    fn test_span() -> Span {
        Span::origin()
    }

    #[test]
    fn test_thunk_arena_alloc_get() {
        let mut arena = ThunkArena::new();
        let thunk = Arc::new(Thunk::new_materialized(Value::Int(42), test_span()));
        let id = arena.alloc(Arc::clone(&thunk));
        assert_eq!(arena.get(id).try_get_materialized(), Some(Value::Int(42)));
    }

    #[test]
    fn test_thunk_arena_multiple_allocs() {
        let mut arena = ThunkArena::new();
        let id1 = arena.alloc(Arc::new(Thunk::new_materialized(
            Value::Int(1),
            test_span(),
        )));
        let id2 = arena.alloc(Arc::new(Thunk::new_materialized(
            Value::Int(2),
            test_span(),
        )));
        let id3 = arena.alloc(Arc::new(Thunk::new_materialized(
            Value::Int(3),
            test_span(),
        )));

        assert_eq!(arena.get(id1).try_get_materialized(), Some(Value::Int(1)));
        assert_eq!(arena.get(id2).try_get_materialized(), Some(Value::Int(2)));
        assert_eq!(arena.get(id3).try_get_materialized(), Some(Value::Int(3)));

        // ThunkId values should be distinct and sequential
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_thunk_arena_id_indexing() {
        let mut arena = ThunkArena::new();
        let ids: Vec<ThunkId> = (0..10)
            .map(|i| {
                arena.alloc(Arc::new(Thunk::new_materialized(
                    Value::Int(i as i64),
                    test_span(),
                )))
            })
            .collect();

        // Verify all IDs are accessible and contain the right values
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(
                arena.get(id).try_get_materialized(),
                Some(Value::Int(i as i64))
            );
        }
    }

    #[test]
    fn test_thunk_arena_placeholder() {
        let mut arena = ThunkArena::new();
        let id = arena.alloc_placeholder();

        // Placeholder should not be materialized yet
        let thunk = arena.get(id);
        assert!(
            !thunk.is_materialized(),
            "expected placeholder to not be materialized"
        );

        // Fill it via set_materialized (forward transition: Placeholder → Materialized)
        thunk.set_materialized(Value::Int(99));

        // Verify the fill worked
        assert_eq!(arena.get(id).try_get_materialized(), Some(Value::Int(99)));
    }

    #[test]
    fn test_thunk_arena_letrec_pattern() {
        // Simulate letrec: pre-allocate two placeholders, then fill the placeholders.
        // This test focuses on ThunkArena placeholder lifecycle only.
        // Monotonicity: Placeholder → Unevaluated/Materialized (forward transitions).
        let mut arena = ThunkArena::new();

        // Step 1: allocate placeholders
        let id_x = arena.alloc_placeholder();
        let id_y = arena.alloc_placeholder();

        // Verify both start as placeholders (not materialized)
        assert!(!arena.get(id_x).is_materialized());
        assert!(!arena.get(id_y).is_materialized());

        // Step 2: fill placeholders (in real eval, these would be Unevaluated with a shared env)
        arena.get(id_x).set_materialized(Value::Int(10));
        arena.get(id_y).set_materialized(Value::Int(20));

        // Step 3: verify the thunks are accessible through the arena
        assert_eq!(arena.get(id_x).try_get_materialized(), Some(Value::Int(10)));
        assert_eq!(arena.get(id_y).try_get_materialized(), Some(Value::Int(20)));
    }

    #[test]
    #[should_panic(expected = "Placeholder")]
    fn test_placeholder_force_panics() {
        use crate::eval::materialize;
        use crate::eval::EvalContext;
        use crate::value::Environment;

        // Create a placeholder thunk (unfilled)
        let mut arena = ThunkArena::new();
        let id = arena.alloc_placeholder();
        let thunk = arena.get(id);

        // Create a minimal test context
        let env = Arc::new(std::sync::RwLock::new(Environment::new()));
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let ctx = EvalContext::new(base_dir, env, false);

        // Attempt to materialize the placeholder thunk - this should panic
        let _result = materialize(&thunk, None, &ctx);
    }

    #[test]
    fn test_env_arena_alloc_get() {
        let mut arena = EnvArena::new();
        let id = arena.alloc_root(0);
        let retrieved = arena.get(id);
        assert_eq!(retrieved.slots.len(), 0);
        assert_eq!(retrieved.overflow.len(), 0);
        assert!(retrieved.parent.is_none());
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
    fn test_flat_env_slot_lookup() {
        let mut arena = EnvArena::new();
        let id = arena.alloc_root(3);
        let env = arena.get_mut(id);

        let id0 = ThunkId(0);
        let id1 = ThunkId(1);
        let id2 = ThunkId(2);

        env.set_slot(0, id0);
        env.set_slot(1, id1);
        env.set_slot(2, id2);

        assert_eq!(env.get_slot(0), Some(id0));
        assert_eq!(env.get_slot(1), Some(id1));
        assert_eq!(env.get_slot(2), Some(id2));
        assert_eq!(env.get_slot(3), None);
    }

    #[test]
    fn test_flat_env_overflow_lookup() {
        let mut arena = EnvArena::new();
        let id = arena.alloc_root(0);
        let env = arena.get_mut(id);

        let id_x = ThunkId(10);
        let id_y = ThunkId(20);

        env.insert_overflow("x".to_string(), id_x);
        env.insert_overflow("y".to_string(), id_y);

        assert_eq!(env.get_by_name("x"), Some(id_x));
        assert_eq!(env.get_by_name("y"), Some(id_y));
        assert_eq!(env.get_by_name("z"), None);
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
    fn test_flat_env_hybrid_static_and_computed() {
        let mut arena = EnvArena::new();
        let id = arena.alloc_root(2);
        let env = arena.get_mut(id);

        // Static keys in slots
        let id_static_a = ThunkId(100);
        let id_static_b = ThunkId(200);
        env.set_slot(0, id_static_a);
        env.set_slot(1, id_static_b);

        // Computed keys in overflow
        let id_computed_x = ThunkId(300);
        let id_computed_y = ThunkId(400);
        env.insert_overflow("x".to_string(), id_computed_x);
        env.insert_overflow("y".to_string(), id_computed_y);

        // Verify slot lookups
        assert_eq!(env.get_slot(0), Some(id_static_a));
        assert_eq!(env.get_slot(1), Some(id_static_b));

        // Verify overflow lookups
        assert_eq!(env.get_by_name("x"), Some(id_computed_x));
        assert_eq!(env.get_by_name("y"), Some(id_computed_y));
    }

    #[test]
    fn test_empty_arena() {
        let thunk_arena = ThunkArena::new();
        let env_arena = EnvArena::new();

        // Empty arenas should be safe (no panics on construction)
        assert_eq!(thunk_arena.thunks.len(), 0);
        assert_eq!(env_arena.envs.len(), 0);
    }

    #[test]
    fn test_many_allocations() {
        let mut arena = ThunkArena::new();
        let count = 1000;

        let ids: Vec<ThunkId> = (0..count)
            .map(|i| {
                arena.alloc(Arc::new(Thunk::new_materialized(
                    Value::Int(i as i64),
                    test_span(),
                )))
            })
            .collect();

        // Verify all allocations are correct
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(
                arena.get(id).try_get_materialized(),
                Some(Value::Int(i as i64))
            );
        }
    }

    #[test]
    fn test_set_slot_extends_vec() {
        let mut arena = EnvArena::new();
        let id = arena.alloc_root(0);
        let env = arena.get_mut(id);

        // Setting slot 5 should extend the vec to hold slots 0..=5
        env.set_slot(5, ThunkId(42));

        assert_eq!(env.get_slot(5), Some(ThunkId(42)));
        assert!(env.slots.len() >= 6);
    }

    #[test]
    fn test_thunk_id_copy_semantics() {
        let id = ThunkId(123);
        let id_copy = id; // Should be a Copy, not a move

        // Both should be equal and usable
        assert_eq!(id, id_copy);
        assert_eq!(id.0, 123);
        assert_eq!(id_copy.0, 123);
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
    fn test_clone_for_child_snapshot_size() {
        // Test (a): snapshot.len() == parent.len() at snapshot time
        let mut parent = ThunkArena::new();
        let id1 = parent.alloc(Arc::new(Thunk::new_materialized(
            Value::Int(1),
            test_span(),
        )));
        let id2 = parent.alloc(Arc::new(Thunk::new_materialized(
            Value::Int(2),
            test_span(),
        )));
        let id3 = parent.alloc(Arc::new(Thunk::new_materialized(
            Value::Int(3),
            test_span(),
        )));

        let snapshot = parent.clone_for_child();

        // Snapshot should have the same length as parent at snapshot time
        assert_eq!(snapshot.len(), parent.len());
        assert_eq!(snapshot.len(), 3);

        // Verify snapshot contains the same ThunkIds
        assert_eq!(
            snapshot.get(id1).try_get_materialized(),
            Some(Value::Int(1))
        );
        assert_eq!(
            snapshot.get(id2).try_get_materialized(),
            Some(Value::Int(2))
        );
        assert_eq!(
            snapshot.get(id3).try_get_materialized(),
            Some(Value::Int(3))
        );
    }

    #[test]
    fn test_clone_for_child_independent_growth() {
        // Test (b): parent grows independently after snapshot
        let mut parent = ThunkArena::new();
        let id1 = parent.alloc(Arc::new(Thunk::new_materialized(
            Value::Int(1),
            test_span(),
        )));

        let mut snapshot = parent.clone_for_child();

        // Parent grows
        let id2 = parent.alloc(Arc::new(Thunk::new_materialized(
            Value::Int(2),
            test_span(),
        )));

        // Snapshot grows independently
        let id3 = snapshot.alloc(Arc::new(Thunk::new_materialized(
            Value::Int(3),
            test_span(),
        )));

        // Parent should have 2 thunks
        assert_eq!(parent.len(), 2);
        assert_eq!(parent.get(id1).try_get_materialized(), Some(Value::Int(1)));
        assert_eq!(parent.get(id2).try_get_materialized(), Some(Value::Int(2)));

        // Snapshot should have 2 thunks (original + new allocation)
        assert_eq!(snapshot.len(), 2);
        assert_eq!(
            snapshot.get(id1).try_get_materialized(),
            Some(Value::Int(1))
        );
        assert_eq!(
            snapshot.get(id3).try_get_materialized(),
            Some(Value::Int(3))
        );
    }

    #[test]
    fn test_clone_for_child_shared_rc_identity() {
        // Test (c): mutating a pre-snapshot thunk's state is visible in the snapshot
        let mut parent = ThunkArena::new();
        let thunk = Arc::new(Thunk::new_placeholder(test_span()));
        let id = parent.alloc(Arc::clone(&thunk));

        let snapshot = parent.clone_for_child();

        // Mutate the thunk's state via the parent's reference
        thunk.set_materialized(Value::Int(42));

        // The mutation should be visible in both parent and snapshot (shared Rc)
        assert_eq!(parent.get(id).try_get_materialized(), Some(Value::Int(42)));
        assert_eq!(
            snapshot.get(id).try_get_materialized(),
            Some(Value::Int(42))
        );

        // Verify they point to the same underlying Thunk (Rc identity)
        assert!(Arc::ptr_eq(parent.get(id), snapshot.get(id)));
    }
}
