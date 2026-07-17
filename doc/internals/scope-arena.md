# Scope Arena

The scope arena is the runtime memory model for lexical scopes and thunk storage. It provides stable, copy-cheap handles to both scopes (`ScopeId`) and individual thunks (`ThunkId`), and implements de Bruijn coordinate-based variable lookup.

---

## Data Model

```
ScopeArena
└── scopes: Vec<Scope>    ← indexed by ScopeId(u32)

Scope
├── slots: Vec<Option<Arc<Thunk>>>   ← indexed by slot (u32 ordinal)
├── slot_names: Vec<String>          ← parallel to slots; always same length
└── parent: Option<ScopeId>          ← parent-chain link (None = root)

ThunkId { scope_id: u32, slot: u32 } ← 8 bytes; Copy
ScopeId(u32)                          ← 4 bytes; Copy
```

`ScopeArena` is a flat `Vec<Scope>`. A `ScopeId` is an absolute index into that vec — it never changes once assigned and is never reused. A `ThunkId` is a stable `(scope_id, slot)` pair: `scope_id` selects the `Scope` and `slot` is the ordinal position of the thunk within that scope's `slots` vec.

Both handle types are `Copy`. Passing them around is free.

---

## De Bruijn Coordinate System

Variables are addressed by `(level, slot)` de Bruijn coordinates assigned by the resolver (before evaluation):

- **`level`** — number of parent-chain hops from the current scope to the scope that owns the binding. Level 0 = current scope. Level 1 = immediate parent. Level N = N-th ancestor.
- **`slot`** — ordinal position of the binding within that ancestor scope's `slots` vec.

At lookup time:

```
walk_parent_chain(current_env_id, level) → target ScopeId
arena.scopes[target].get(slot)           → Arc<Thunk>
```

The resolver assigns `(level, slot)` once per VarRef node (writing into a `ResolutionTable`); both the evaluator and type checker read the same coordinates. There is no coordinate-system mismatch between subsystems.

---

## Slot Lifecycle

### Filled Slot (normal binding)

`push_slot(env_id, name, thunk)` appends a `Some(thunk)` entry to the scope's `slots` vec and returns the new `ThunkId`. The slot index equals the number of slots before the push.

### Reserved Slot (letrec, two-phase)

Dict construction is letrec-scoped: all entries can mutually reference each other. This requires two passes:

**Phase 1 — reserve:**
```rust
let slot_idx = arena.reserve_slot(env_id, name);
// slots[slot_idx] = None  (placeholder)
```

**Phase 2 — fill:**
```rust
arena.fill_slot(env_id, slot_idx, src_thunk_id);
// slots[slot_idx] = Some(Arc::clone of src_thunk_id's Arc<Thunk>)
```

The `fill_slot` call copies the `Arc<Thunk>` from another `ThunkId`; it does not move the thunk or create a new one.

A reserved-but-unfilled slot (`None`) is an in-flight letrec placeholder. Forcing a thunk that references such a slot returns a circular-dependency error (same behaviour as an `InProgress` thunk — the two are indistinguishable at the `ThunkInner` level).

---

## Invariants

1. **Named slots only.** Every slot — reserved or filled — must have a name. `push_slot` and `reserve_slot` panic in debug builds if given an empty name. Synthetic slots must use gensym'd names.
2. **Parallel vecs.** `slots` and `slot_names` are always kept the same length by the API. Direct mutation of either vec is not allowed outside `Scope`'s own methods.
3. **Stable indices.** A `ScopeId` or `ThunkId` is valid for the lifetime of the `ScopeArena`. Once assigned, slot indices within a scope never shift. `drop_scope` clears the `Arc<Thunk>` refs but leaves the `slots` vec empty and the `ScopeId` in the arena permanently.
4. **u32 overflow guard.** `alloc_root` and `alloc_child` assert `scopes.len() < u32::MAX`. In practice, memory exhausts first.

---

## API

### Constructors

```rust
ScopeArena::new() -> ScopeArena
```

Creates an empty arena with no scopes.

```rust
arena.alloc_root(slot_count: usize) -> ScopeId
```

Allocates a root scope (no parent). `slot_count` pre-allocates capacity — not a hard limit.

```rust
arena.alloc_child(parent_id: ScopeId, slot_count: usize) -> ScopeId
```

Allocates a child scope whose parent is `parent_id`.

### Slot Management

```rust
arena.push_slot(env_id: ScopeId, name: &str, thunk: Arc<Thunk>) -> ThunkId
```

Appends a filled slot. Returns the new `ThunkId`. The slot index equals the scope's current length before the push.

```rust
arena.reserve_slot(env_id: ScopeId, name: &str) -> u32
```

Letrec phase 1. Appends a `None` placeholder slot. Returns the slot index (not a `ThunkId` — the slot is not yet valid for `get_thunk`).

```rust
arena.fill_slot(env_id: ScopeId, slot_idx: u32, src_thunk_id: ThunkId)
```

Letrec phase 2. Copies the `Arc<Thunk>` from `src_thunk_id` into the reserved slot.

### Lookup

```rust
arena.get_thunk(id: ThunkId) -> Arc<Thunk>
```

O(1). Panics if the slot is `None` (unfilled placeholder — "use-after-free" message).

### Parent Chain Traversal

```rust
arena.walk_parent_chain(start_env_id: u32, level: usize) -> Result<ScopeId, usize>
```

Walks the parent chain `level` hops from `start_env_id`. `level=0` returns `start_env_id` immediately. Returns `Err(depth_reached)` if the chain runs out before `level` hops. Used by variable lookup at eval time.

```rust
arena.collect_parent_chain(start_env_id: u32) -> Vec<ScopeId>
```

Returns the full parent chain outermost-first (`[root, ..., start]`). Used by the resolver to seed scope levels.

### Lifecycle

```rust
arena.drop_scope(env_id: ScopeId)
```

Clears all `Arc<Thunk>` refs in the scope (freeing the thunks if no other owners exist). `slot_names` is preserved for post-drop diagnostics. The `ScopeId` remains in the arena permanently.

---

## Arena Migration

When a document section boundary (`---`) is crossed, thunks that are reachable from the pipeline output (`%`) must survive the section's arena being logically released. The migration system copies the reachable graph of thunks and scopes into a permanent destination scope, translating all `ThunkId` and `ScopeId` references.

Three functions implement migration:

```rust
migrate_flat_env(src_env_id, src_range, dst_env_id, thunk_map, env_map, arena) -> u32
migrate_thunk_id(thunk_id, src_range, dst_env_id, thunk_map, env_map, arena) -> ThunkId
migrate_value(value, src_range, dst_env_id, thunk_map, env_map, arena) -> Value
```

- **`src_range`** — the range of `ScopeId` values (`u32`) that belong to the section being released. `ThunkId`s with `scope_id` inside this range are migrated; those outside are permanent and left as-is.
- **`thunk_map`** and **`env_map`** — translation tables (`HashMap`) that map source `ThunkId`/`ScopeId` to their destination counterparts. Entries are inserted before recursive calls (two-phase cycle-safety at both levels).
- **Two-phase protocol** — for each node (scope or thunk), a placeholder is allocated in the destination and inserted into the translation table *before* recursing into child nodes. This breaks reference cycles: if a thunk's value refers back to the same thunk, `migrate_thunk_id` finds it in `thunk_map` and returns the already-allocated destination `ThunkId` without recursing further.

`translate_unevaluated_state` handles unevaluated thunks: it rewrites all `env_id` and `ThunkId` fields inside the `UnevaluatedState` so the migrated thunk does not reference scopes in the released range.

