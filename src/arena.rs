#![allow(dead_code)]
//! Arena allocation for thunks and environments (Phase 2 of arena allocation strategy).
//!
//! This module provides index-based arenas for thunks and environments, replacing the
//! `Rc<Thunk>` / `Rc<RefCell<Environment>>` model with `ThunkId` / `EnvId` handles
//! that index into `Vec<Rc<Thunk>>` / `Vec<FlatEnv>` backing stores.
//!
//! For now (Phase 2), the arena stores `Rc<Thunk>` values — the migration from `Rc`
//! to direct ownership happens in Phase 3 (`arena-eval`). This phase establishes the
//! arena API and the `ThunkId` / `EnvId` handle types.

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::Span;
use crate::value::Thunk;

/// A handle to a thunk in the arena. Copy-cheap (4 bytes), indexes into `ThunkArena`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThunkId(u32);

/// A handle to an environment in the arena. Copy-cheap (4 bytes), indexes into `EnvArena`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EnvId(u32);

/// Arena for thunk allocation. Stores `Rc<Thunk>` indexed by `ThunkId`.
///
/// Phase 2 API: the arena wraps a `Vec<Rc<Thunk>>`. Phase 3 will migrate to `Vec<Thunk>`
/// for direct ownership. All public methods remain the same across phases.
#[derive(Debug)]
pub(crate) struct ThunkArena {
    thunks: Vec<Rc<Thunk>>,
}

impl ThunkArena {
    /// Create a new empty arena.
    pub fn new() -> Self {
        Self { thunks: Vec::new() }
    }

    /// Allocate a thunk in the arena, returning its handle.
    pub fn alloc(&mut self, thunk: Rc<Thunk>) -> ThunkId {
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
    pub fn get(&self, id: ThunkId) -> &Rc<Thunk> {
        &self.thunks[id.0 as usize]
    }

    /// Allocate a placeholder thunk for letrec. The placeholder is a sentinel
    /// `ThunkState::Placeholder` that must be filled via `set_state()` before use.
    ///
    /// The letrec pattern (internal evaluator use):
    /// 1. Pre-allocate placeholder slots for all dict entries.
    /// 2. Create the shared `FlatEnv` with those `ThunkId`s.
    /// 3. Fill each placeholder via `arena.get(id).set_state(...)` (requires pub(crate) access).
    ///
    /// Forcing a placeholder before filling is a logic error (letrec construction bug)
    /// and will panic at materialization time. This maintains Launchbury's monotonicity
    /// invariant: Placeholder → Unevaluated is a forward state transition.
    pub fn alloc_placeholder(&mut self) -> ThunkId {
        let thunk = Rc::new(Thunk::new_placeholder(Span::origin()));
        self.alloc(thunk)
    }

    /// Create a new arena pre-populated with clones of this arena's entries.
    ///
    /// Used to give each EvalContext its own growable arena while still sharing
    /// stdlib thunks: the child arena starts with Rc::clone of every thunk in self,
    /// preserving ThunkId validity (same indices 0..N), then appends its own thunks
    /// starting at N.  Dropping the child does not affect the parent's thunks.
    pub(crate) fn clone_for_child(&self) -> Self {
        Self {
            thunks: self.thunks.iter().map(Rc::clone).collect(),
        }
    }

    /// Number of thunks currently in the arena.
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
#[derive(Debug)]
pub(crate) struct EnvArena {
    envs: Vec<FlatEnv>,
}

impl EnvArena {
    /// Create a new empty environment arena.
    pub fn new() -> Self {
        Self { envs: Vec::new() }
    }

    /// Allocate an environment in the arena, returning its handle.
    pub fn alloc(&mut self, env: FlatEnv) -> EnvId {
        let len = self.envs.len();
        assert!(
            len < u32::MAX as usize,
            "EnvArena overflow: more than {} environments allocated",
            u32::MAX
        );
        let id = EnvId(len as u32);
        self.envs.push(env);
        id
    }

    /// Get a reference to the environment at the given handle.
    ///
    /// # Panics
    ///
    /// Panics if the handle is out of bounds (should never happen if all IDs come from `alloc`).
    pub fn get(&self, id: EnvId) -> &FlatEnv {
        &self.envs[id.0 as usize]
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
#[derive(Debug)]
pub(crate) struct FlatEnv {
    /// Static keys indexed by compile-time slot number from the resolver.
    pub(crate) slots: Vec<Option<ThunkId>>,
    /// Computed keys (resolver returned None) indexed by name.
    pub(crate) overflow: HashMap<String, ThunkId>,
    /// Parent environment for stdlib/builtins root chain. Most environments don't need this;
    /// user-scope lookups use `(level, slot)` pairs that resolve to the correct FlatEnv directly.
    pub(crate) parent: Option<EnvId>,
}

impl FlatEnv {
    /// Create a new flat environment with the given slot capacity and optional parent.
    pub fn new(slot_count: usize, parent: Option<EnvId>) -> Self {
        Self {
            slots: Vec::with_capacity(slot_count),
            overflow: HashMap::new(),
            parent,
        }
    }

    /// Create a new empty flat environment (no slots, no parent).
    pub fn empty() -> Self {
        Self::new(0, None)
    }

    /// Get a thunk by slot index (static key, assigned by the resolver).
    ///
    /// Returns `None` if the slot is out of bounds or unfilled.
    pub fn get_slot(&self, slot: u32) -> Option<ThunkId> {
        self.slots.get(slot as usize).and_then(|&opt| opt)
    }

    /// Get a thunk by name from the overflow table (computed key, name-based lookup).
    pub fn get_by_name(&self, name: &str) -> Option<ThunkId> {
        self.overflow.get(name).copied()
    }

    /// Insert a thunk into a slot. Extends the slot vec if necessary.
    ///
    /// This is used during FlatEnv construction when filling slots in order.
    /// If `slot` is beyond the current vec length, intermediate slots are filled with
    /// `None` (unfilled placeholders). Callers must not query unfilled slots.
    pub fn set_slot(&mut self, slot: u32, id: ThunkId) {
        let slot_idx = slot as usize;
        if slot_idx >= self.slots.len() {
            self.slots.resize(slot_idx + 1, None);
        }
        self.slots[slot_idx] = Some(id);
    }

    /// Insert a thunk into the overflow table (computed key).
    pub fn insert_overflow(&mut self, name: String, id: ThunkId) {
        self.overflow.insert(name, id);
    }

    /// Get the parent environment ID, if any.
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
/// - Arena stores `Vec<Rc<Thunk>>` (Rc-wrapped, not direct ownership)
/// - Arena persists across `---` boundaries (not dropped per section)
/// - ThunkIds are stable indices that never invalidate
/// - `%` pipeline variable passes as `Rc<Thunk>` across documents (lazy, no materialization)
///
/// **When migration will be required (Phase 3):**
/// - Arena stores `Vec<Thunk>` (direct ownership, no Rc wrapper)
#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Thunk, ThunkState, Value};
    use std::rc::Rc;

    fn test_span() -> Span {
        Span::origin()
    }

    #[test]
    fn test_thunk_arena_alloc_get() {
        let mut arena = ThunkArena::new();
        let thunk = Rc::new(Thunk::new_materialized(Value::Int(42), test_span()));
        let id = arena.alloc(Rc::clone(&thunk));
        assert_eq!(arena.get(id).try_get_materialized(), Some(Value::Int(42)));
    }

    #[test]
    fn test_thunk_arena_multiple_allocs() {
        let mut arena = ThunkArena::new();
        let id1 = arena.alloc(Rc::new(Thunk::new_materialized(Value::Int(1), test_span())));
        let id2 = arena.alloc(Rc::new(Thunk::new_materialized(Value::Int(2), test_span())));
        let id3 = arena.alloc(Rc::new(Thunk::new_materialized(Value::Int(3), test_span())));

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
                arena.alloc(Rc::new(Thunk::new_materialized(
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

        // Placeholder should be in Placeholder state (not Materialized)
        let thunk = arena.get(id);
        assert!(
            matches!(&*thunk.state(), ThunkState::Placeholder),
            "expected Placeholder state"
        );

        // Fill it via set_state (forward transition: Placeholder → Materialized)
        thunk.set_state(ThunkState::Materialized(Value::Int(99)));

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

        // Verify both start as Placeholder
        assert!(matches!(&*arena.get(id_x).state(), ThunkState::Placeholder));
        assert!(matches!(&*arena.get(id_y).state(), ThunkState::Placeholder));

        // Step 2: fill placeholders (in real eval, these would be Unevaluated with a shared env)
        arena
            .get(id_x)
            .set_state(ThunkState::Materialized(Value::Int(10)));
        arena
            .get(id_y)
            .set_state(ThunkState::Materialized(Value::Int(20)));

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
        use std::cell::RefCell;

        // Create a placeholder thunk (unfilled)
        let mut arena = ThunkArena::new();
        let id = arena.alloc_placeholder();
        let thunk = arena.get(id);

        // Create a minimal test context
        let env = Rc::new(RefCell::new(Environment::new()));
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let ctx = EvalContext::new(base_dir, env, false);

        // Attempt to materialize the placeholder thunk - this should panic
        let _result = materialize(&thunk, None, &ctx);
    }

    #[test]
    fn test_env_arena_alloc_get() {
        let mut arena = EnvArena::new();
        let env = FlatEnv::empty();
        let id = arena.alloc(env);
        let retrieved = arena.get(id);
        assert_eq!(retrieved.slots.len(), 0);
        assert_eq!(retrieved.overflow.len(), 0);
        assert!(retrieved.parent.is_none());
    }

    #[test]
    fn test_env_arena_multiple_allocs() {
        let mut arena = EnvArena::new();
        let id1 = arena.alloc(FlatEnv::new(1, None));
        let id2 = arena.alloc(FlatEnv::new(2, None));
        let id3 = arena.alloc(FlatEnv::new(3, None));

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
        let mut env = FlatEnv::new(3, None);
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
        let mut env = FlatEnv::empty();
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
        let root_id = env_arena.alloc(FlatEnv::new(0, None));

        // Create a child env with root as parent
        let child_id = env_arena.alloc(FlatEnv::new(0, Some(root_id)));

        let child = env_arena.get(child_id);
        assert_eq!(child.parent(), Some(root_id));

        let root = env_arena.get(root_id);
        assert_eq!(root.parent(), None);
    }

    #[test]
    fn test_flat_env_hybrid_static_and_computed() {
        let mut env = FlatEnv::new(2, None);

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
                arena.alloc(Rc::new(Thunk::new_materialized(
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
        let mut env = FlatEnv::empty();

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
}
