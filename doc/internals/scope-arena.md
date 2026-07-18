# Scope Arena

This document is for Rust contributors working in `src/arena.rs` and any code that allocates scopes, pushes slots, or resolves `ThunkId`s. Tinct developers: the arena is the reason all variable access is O(parent-chain depth) — each `[fn ...]` or dict scope adds one hop, and de Bruijn `level` counts those hops from innermost to outermost.

The scope arena is the runtime memory model for lexical scopes and thunk storage. It provides stable, copy-cheap handles to both scopes (`ScopeId`) and individual thunks (`ThunkId`), and implements de Bruijn coordinate-based variable lookup. The scope arena is also the **authoritative type-stage environment** — the type checker reads type-stage resolver functions by name from the arena via `lookup_name_in_scope_chain`.

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

`push_slot(env_id, name, thunk)` appends a `Some(thunk)` entry to the scope's `slots` vec and a corresponding `name` to `slot_names`. Returns the new `ThunkId`. The slot index equals the number of slots before the push.

### Reserved Slot (letrec, two-phase)

Dict construction is letrec-scoped: all entries can mutually reference each other. This requires two passes:

**Phase 1 — reserve:**
```rust
let slot_idx = arena.reserve_slot(env_id, name);
// slots[slot_idx] = None  (placeholder)
// slot_names[slot_idx] = name
```

**Phase 2 — fill:**
```rust
arena.fill_slot(env_id, slot_idx, src_thunk_id);
// slots[slot_idx] = Some(Arc::clone of src_thunk_id's Arc<Thunk>)
```

The `fill_slot` call copies the `Arc<Thunk>` from another `ThunkId`; it does not move the thunk or create a new one.

A reserved-but-unfilled slot (`None`) is an in-flight letrec placeholder. Forcing a thunk that references such a slot returns a circular-dependency error (same behaviour as an `InProgress` thunk — the two are indistinguishable at the `ThunkInner` level).

### Anonymous Slot (alloc_thunk)

The `EvalContext::alloc_thunk(env_id, thunk)` convenience wrapper calls `push_slot`. Once the `name` parameter is added to `push_slot` (see §Code Issues), `alloc_thunk` must pass a synthetic name (e.g., `"#anon"` or similar). This is the primary allocator used by the evaluator for expression-level thunks that do not need a user-visible name for diagnostics.

---

## Invariants

1. **Named slots only.** Every slot — reserved or filled — must have a name in `slot_names`. `push_slot` and `reserve_slot` take a `name: &str` parameter and record it at the same index. Synthetic slots use generated names (e.g., `#mig_N`).
2. **Parallel vecs.** `slots` and `slot_names` are always kept the same length by the API. Direct mutation of either vec is not allowed outside `Scope`'s own methods.
3. **Stable indices.** A `ScopeId` or `ThunkId` is valid for the lifetime of the `ScopeArena`. Once assigned, slot indices within a scope never shift. `drop_scope` clears the `Arc<Thunk>` refs but leaves the `ScopeId` in the arena permanently; `slot_names` is preserved for post-drop diagnostics.
4. **u32 overflow guard.** `alloc_root` and `alloc_child` assert `scopes.len() < u32::MAX`. In practice, memory exhausts first.
5. **Root scope at index 0.** `EvalContext::new_scope_arena()` allocates the root scope first, so `ScopeId(0)` is always the builtin root. All builtins are pushed into slot 0 at fixed positions matching the resolver's `build_core_env` slot ordering. Any `current_env_id: 0` reference is valid.

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

Appends a filled slot with its name. Returns the new `ThunkId`. The slot index equals the scope's current length before the push.

```rust
arena.reserve_slot(env_id: ScopeId, name: &str) -> u32
```

Letrec phase 1. Appends a `None` placeholder slot and records `name` in `slot_names`. Returns the slot index (not a `ThunkId` — the slot is not yet valid for `get_thunk`).

```rust
arena.fill_slot(env_id: ScopeId, slot_idx: u32, src_thunk_id: ThunkId)
```

Letrec phase 2. Copies the `Arc<Thunk>` from `src_thunk_id` into the reserved slot. Does not modify `slot_names`.

### Lookup

```rust
arena.get_thunk(id: ThunkId) -> Arc<Thunk>
```

O(1). Panics if the slot is `None` (unfilled placeholder — "use-after-free" message).

```rust
arena.lookup_name_in_scope_chain(start_env_id: u32, name: &str) -> Option<Arc<Thunk>>
```

Walks the parent chain from `start_env_id`, searching `slot_names` at each level for a slot whose name equals `name`. Returns the first match's `Arc<Thunk>`, or `None` if the name is not found in any ancestor. Used by the type checker's `resolve_type_head` (Step 4) and `normalize()` to look up type-stage resolver functions in the scope chain. The borrow of the arena must be released before calling `.await` on the result.

### Iteration

```rust
scope.iter_named() -> impl Iterator<Item = (&str, u32)>
```

Yields `(name, slot_index)` pairs for all named slots in this scope frame (no parent walk). Used by `lib.rs` to build the root scope frame for `with_scope_frames()`, and by diagnostics tools to enumerate scope contents.

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

## Type-Stage Integration

The scope arena is the authoritative type-stage environment. When the loader processes type-stage documents (those with `stage: "type"` in their header), it:

1. **Evaluates the type-stage documents** via `eval_surface_file`, producing a dict thunk.
2. **Forces all thunks** in the dict to build a `type_stage_map: HashMap<String, TypeStageEntry>` — either `Resolved(Type)` for primitive leaf types or `Function(ThunkId)` for parameterized type constructors.
3. **Sets `type_stage_scope_id`** on the `TypeContextData` to the scope ID of the evaluated type-stage environment via `builtin-tc-with-scope`. This scope ID is then threaded into `InferState.type_stage_scope_id` by `builtin-typecheck-doc`.

The type checker's `resolve_type_head` uses this scope ID in Step 4 of its lookup order:

```
1. Kind constraints (Operator, Label)
2. class_env — type class names before tycon_env
3. type_stage_map — pre-computed TypeStageEntry map
4. scope-chain lookup via type_stage_scope_id → lookup_name_in_scope_chain → materialize → call_strict_resolver
5. Undefined → TypeError
```

Step 3 (`type_stage_map`) handles the common case efficiently. Step 4 is the general mechanism: it borrows the arena to look up the name by walking `slot_names`, drops the borrow before the `.await`, then materializes the found thunk. This two-step borrow-then-await pattern is required because `RefCell` borrows cannot cross `.await` points.

Similarly, `normalize()` in `type_normalize.rs` uses the same pattern for `TypeStageApp` node reduction: it extracts `type_stage_scope_id` from the `TypeContext` lock, borrows the arena to call `lookup_name_in_scope_chain`, drops the borrow, then `.await`s the materialization.

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

`translate_unevaluated_state` handles unevaluated thunks: it rewrites all `env_id` and `ThunkId` fields inside the `UnevaluatedState` so the migrated thunk does not reference scopes in the released range. The six `UnevaluatedState` variants handled are:

- `Surface` — rewrites `env_id`
- `AstField` — no arena fields; returned unchanged
- `CoreExpr` — rewrites `env_id`
- `BuiltinCall` — rewrites `caller_env_id` and all `ThunkId`s in `args` and `named`
- `FnCall` — rewrites `caller_env_id`, `func` ThunkId, and all `ThunkId`s in `args` and `named`
- `Guarded` — rewrites `inner` ThunkId and the `env_id` inside the optional `default` field

`migrate_flat_env` preserves `slot_names` by collecting them from the source scope and passing them to `push_slot` and `reserve_slot` during migration. Slots with empty names get synthetic names (`#mig_N`).

---

## Scope vs. Env Distinction

The scope arena is **eval-stage only**. The type checker uses a parallel `Env` chain (`src/env.rs`) with `Arc<RwLock<Env>>` parent links. The two chains use the same de Bruijn coordinates — the resolver assigns `(level, slot)` pairs that are valid for both — but they are separate data structures:

| | `ScopeArena` / `Scope` | `Env` |
|---|---|---|
| Stage | Eval (runtime) | Type check |
| Threading | `Rc<RefCell<>>` (single-threaded) | `Arc<RwLock<>>` (shared) |
| Slot storage | `Arc<Thunk>` | `TypeScheme` |
| Name storage | `slot_names: Vec<String>` | `slots: IndexMap<String, EnvSlot>` |
| Lookup | `get_thunk(ThunkId)` | `get_scheme_at(level, slot)` |

The type-stage integration (`type_stage_scope_id`) is the only path where the type checker reads from the scope arena directly — it does so by name via `lookup_name_in_scope_chain`, not by de Bruijn coordinate.

---

## Code Issues

The following inconsistencies were observed in the current state of `src/arena.rs` (file marked `M` in git status) and should be resolved:

1. **Missing `slot_names` field on `Scope`**: The `Scope` struct definition in `src/arena.rs` shows only `slots` and `parent`, but migration code (line 293) references `src_env.slot_names`, tests (line 1261) access `arena.scopes[...].slot_names`, and the invariants described in this document require it. The field must be added: `slot_names: Vec<String>`.

2. **Missing `name` parameter on `push_slot` and `reserve_slot`**: The public `ScopeArena::push_slot` (line 79) and `reserve_slot` (line 88) signatures lack a `name: &str` parameter, but every caller (`eval.rs:191`, `eval_call.rs:314`, `eval_dict.rs:400`, `builtins_meta.rs:1877`, migration code, tests) passes one. The inner `Scope::push` and `Scope::reserve` methods are also missing the name parameter.

3. **Missing `lookup_name_in_scope_chain` method**: Called in `typecheck_annot.rs:2244` and `type_normalize.rs:132` on `ScopeArena`, but not defined anywhere in `src/arena.rs`. This method is required for the type-stage scope-chain lookup path (Step 4 of `resolve_type_head`).

4. **Missing `iter_named` method on `Scope`**: Called in `lib.rs:201`, `eval_core.rs:672`, `builtins.rs:769`, `builtins_meta.rs:1955`, and elsewhere. Not defined in `src/arena.rs`. Must return `impl Iterator<Item = (&str, u32)>` over `slot_names` and their slot indices.

5. **`drop_scope` does not clear `slot_names`**: `Scope::clear()` only clears `slots`, preserving `slot_names` for post-drop diagnostics — this is intentional per invariant 3 above. Ensure `drop_scope` documentation states this explicitly.

All four missing items are required for the codebase to compile correctly. They represent an incomplete edit of `src/arena.rs`.
