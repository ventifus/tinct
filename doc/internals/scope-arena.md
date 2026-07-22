# Scope Arena

This document is for Rust contributors working in `src/arena.rs` and any code that allocates scopes, pushes slots, or resolves `ThunkId`s. Tinct developers: the arena is the reason all variable access is O(parent-chain depth) — each `[fn ...]` or dict scope adds one hop, and de Bruijn `level` counts those hops from innermost to outermost.

The scope arena is the runtime memory model for lexical scopes and thunk storage. It provides stable, copy-cheap handles to both scopes (`ScopeId`) and individual thunks (`ThunkId`), and implements de Bruijn coordinate-based variable lookup. The scope arena is the **authoritative type-stage environment** — the type checker reads type-stage resolver functions from the arena via `type_stage_map` (a pre-computed `HashMap<String, TypeStageEntry>` built by the loader).

---

## Data Model

```
ScopeArena
└── scopes: Vec<Scope>    ← indexed by ScopeId(u32)

Scope
├── slots: Vec<Option<Arc<Thunk>>>   ← indexed by slot (u32 ordinal)
└── parent: Option<ScopeId>          ← parent-chain link (None = root)

ThunkId { scope_id: u32, slot: u32 } ← 8 bytes; Copy
ScopeId(u32)                          ← 4 bytes; Copy
```

Slot identity is purely positional (`slot` ordinal within the scope's `slots` vec). Variable names are carried by `Thunk.span.name` — set by `span.with_name(name)` at thunk creation time in `eval_dict.rs`. There is no `slot_names` parallel vec on `Scope`.

`ScopeArena` is a flat `Vec<Scope>`. A `ScopeId` is an absolute index into that vec — it never changes once assigned and is never reused. A `ThunkId` is a stable `(scope_id, slot)` pair: `scope_id` selects the `Scope` and `slot` is the ordinal position of the thunk within that scope's `slots` vec.

Both handle types are `Copy`. Passing them around is free.

The arena is owned by `EvalContext` as `Rc<RefCell<ScopeArena>>`. All arena access goes through borrow/borrow_mut on this `RefCell`. The `Rc` allows child `EvalContext` instances to share the same arena — the evaluation runtime is strictly single-threaded.

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

Scope frames (`scope_frames` on `EvalContext`) mirror this: they are the resolver's view of the same scope structure, stored as a `Vec<IndexMap<String, u32>>` where the outer Vec is indexed by level and the inner map is indexed by name. `with_scope_frames()` attaches these frames to a context after `resolve_surface_program` runs on the init (loader) program, enabling `lower()` to resolve `call_dispatch` mangled instance binding names to correct de Bruijn coordinates.

---

## Slot Lifecycle

### Filled Slot (normal binding)

`push_slot(env_id, thunk)` appends a `Some(thunk)` entry to the scope's `slots` vec. Returns the new `ThunkId`. The slot index equals the number of slots before the push. The thunk's name (for diagnostics) is set via `thunk.span.name` at creation time, not stored in the scope.

### Reserved Slot (letrec, two-phase)

Dict construction is letrec-scoped: all entries can mutually reference each other. This requires two passes:

**Phase 1 — reserve:**
```rust
let slot_idx = arena.reserve_slot(env_id);
// slots[slot_idx] = None  (placeholder)
```

**Phase 2 — fill:**
```rust
arena.fill_slot(env_id, slot_idx, src_thunk_id);
// slots[slot_idx] = Some(Arc::clone of src_thunk_id's Arc<Thunk>)
```

The `fill_slot` call copies the `Arc<Thunk>` from another `ThunkId`; it does not move the thunk or create a new one.

A reserved-but-unfilled slot (`None`) is an in-flight letrec placeholder. Forcing a thunk that references such a slot returns a circular-dependency error (same behaviour as an `InProgress` thunk — the two are indistinguishable at the `ThunkInner` level).

### Anonymous Slot (alloc_thunk)

The `EvalContext::alloc_thunk(env_id, thunk)` convenience wrapper calls `push_slot`. This is the primary allocator used by the evaluator for expression-level thunks. The thunk's span carries any available name information — there is no separate name parameter on `push_slot`.

---

## Invariants

1. **Slot identity is positional.** Slot indices within a scope never shift once assigned. Variable names are carried by `Thunk.span.name`, not by any parallel vec on `Scope`.
2. **Stable handles.** A `ScopeId` or `ThunkId` is valid for the lifetime of the `ScopeArena`. Once assigned, slot indices within a scope never shift. `drop_scope` clears the `Arc<Thunk>` refs but leaves the `ScopeId` in the arena permanently.
3. **u32 overflow guard.** `alloc_root` and `alloc_child` assert `scopes.len() < u32::MAX`. In practice, memory exhausts first.
4. **Root scope at index 0.** `EvalContext::new_scope_arena()` allocates the root scope first, so `ScopeId(0)` is always the builtin root. All builtins are pushed into slot 0 at fixed positions matching the resolver's `build_core_env` slot ordering. Any `current_env_id: 0` reference is valid.

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
arena.push_slot(env_id: ScopeId, thunk: Arc<Thunk>) -> ThunkId
```

Appends a filled slot. Returns the new `ThunkId`. The slot index equals the scope's current length before the push.

```rust
arena.reserve_slot(env_id: ScopeId) -> u32
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

Clears all `Arc<Thunk>` refs in the scope (freeing the thunks if no other owners exist). The `ScopeId` remains in the arena permanently.

---

## Type-Stage Integration

The scope arena is the authoritative type-stage environment. When the loader processes type-stage documents (those with `stage: "type"` in their header), it:

1. **Evaluates the type-stage documents** via `eval_surface_file`, producing a dict thunk.
2. **Forces all thunks** in the dict to build a `type_stage_map: HashMap<String, TypeStageEntry>` — either `Resolved(Type)` for primitive leaf types or `Function(ThunkId)` for parameterized type constructors.
3. **Sets `type_stage_scope_id`** on the `TypeContextData` to the scope ID of the evaluated type-stage environment via `builtin-tc-with-scope`. This scope ID is then threaded into `InferState.type_stage_scope_id` by `builtin-typecheck-doc`.

The type checker's `resolve_type_head` lookup order:

```
1. Kind constraints (Operator, Label)
2. class_env — type class names before tycon_env
3. type_stage_map — pre-computed TypeStageEntry map (handles the common case)
4. Undefined → TypeError
```

`type_stage_map` is the primary lookup mechanism for type-stage names. The scope arena's `type_stage_scope_id` identifies which scope the type-stage environment lives in, but name lookup goes through `type_stage_map` (built at load time), not by searching slot names in the arena at check time.

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

`translate_unevaluated_state` handles unevaluated thunks: it rewrites all `env_id` and `ThunkId` fields inside the `UnevaluatedState` so the migrated thunk does not reference scopes in the released range. The five `UnevaluatedState` variants handled are:

- `AstField` — no arena fields; returned unchanged
- `CoreExpr` — rewrites `env_id`
- `BuiltinCall` — rewrites `caller_env_id` and all `ThunkId`s in `args` and `named`
- `FnCall` — rewrites `caller_env_id`, `func` ThunkId, and all `ThunkId`s in `args` and `named`
- `Guarded` — rewrites `inner` ThunkId and the `env_id` inside the optional `default` field

`migrate_flat_env` copies slot thunks from the source scope into the destination scope via `push_slot` and `reserve_slot`. Migrated thunks retain their original `span.name` values for diagnostics.

---

## Scope vs. Env Distinction

The scope arena is **eval-stage only**. The type checker uses a parallel `Env` chain (`src/env.rs`) with `Arc<RwLock<Env>>` parent links. The two chains use the same de Bruijn coordinates — the resolver assigns `(level, slot)` pairs that are valid for both — but they are separate data structures:

| | `ScopeArena` / `Scope` | `Env` |
|---|---|---|
| Stage | Eval (runtime) | Type check |
| Threading | `Rc<RefCell<>>` (single-threaded) | `Arc<RwLock<>>` (shared) |
| Slot storage | `Arc<Thunk>` | `TypeScheme` |
| Name storage | `Thunk.span.name` (on each thunk) | `slots: IndexMap<String, EnvSlot>` |
| Lookup | `get_thunk(ThunkId)` | `get_scheme_at(level, slot)` |

The type-stage integration (`type_stage_scope_id`) is the only path where the type checker reads from the scope arena directly — it does so via the pre-computed `type_stage_map` (populated by the loader), not by scanning slot names in the arena.

