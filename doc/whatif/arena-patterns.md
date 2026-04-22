# What If: Safe Rust Arena Patterns for Thunks and Environments

What would it take to replace tinct's `Rc<Thunk>` / `Rc<RefCell<Environment>>`
allocation model with arena-based allocation for the Phase 2 iterative evaluator?

DESIGN.md §Allocation Strategy commits to index-based arenas (`Vec<Thunk>` +
`ThunkId`) with flat environments and selective migration at `---` boundaries.
This document evaluates the concrete Rust crate and pattern choices for
implementing that design.

## Current State

Every thunk allocation creates an `Rc<Thunk>` (24 bytes overhead: strong count
+ weak count + value pointer) containing a `RefCell<ThunkState>` (8 bytes
overhead: borrow flag). Every environment is `Rc<RefCell<Environment>>` with
an `IndexMap<String, Rc<Thunk>>` for bindings and a parent chain for lexical
scope lookup (O(depth) per variable).

Letrec creates cyclic Rc graphs: dict environments hold thunks that close over
the same environment. These cycles are semantically correct but prevent Rc
deallocation — leaked until the process exits.

The arena migration eliminates per-thunk Rc overhead, eliminates Rc cycle
leaks, and enables O(1) variable lookup via flat environments with slot
indices.

## Requirements

The arena must support four operations:

1. **Mutable thunks** — thunks transition through states (Unevaluated →
   InProgress → Materialized/Failed). Interior mutability is required.

2. **Self-referencing graphs** — letrec creates environments where thunks
   reference the environment that contains them. The arena handle type must
   be storable inside arena-allocated items.

3. **Bulk deallocation** — all thunks from one document section dropped at
   once when the arena is dropped.

4. **Stable handles** — handles must remain valid for the arena's lifetime,
   and must be `Copy` for cheap storage in environment slots and thunk states.

## Approaches

### Approach A: `Vec<Thunk>` + `ThunkId(usize)` (Hand-Rolled Index Arena)

The simplest approach: a `Vec` as the backing store, a newtype `usize` as the
handle.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ThunkId(u32);  // u32 sufficient for <4B thunks per section

pub struct ThunkArena {
    thunks: Vec<Thunk>,
}

impl ThunkArena {
    fn alloc(&mut self, thunk: Thunk) -> ThunkId {
        let id = ThunkId(self.thunks.len() as u32);
        self.thunks.push(thunk);
        id
    }

    fn get(&self, id: ThunkId) -> &Thunk { &self.thunks[id.0 as usize] }
}
```

Each `Thunk` retains its `RefCell<ThunkState>` for interior mutability. State
transitions use `thunk.set_state()` / `thunk.transition()` exactly as today.

**Letrec pattern:**
```rust
// Step 1: allocate placeholder slots
let id = arena.alloc(Thunk::placeholder());
// Step 2: build environment referencing id
let env = FlatEnv::new(/* ... with id ... */);
// Step 3: fill slot via RefCell
arena.get(id).set_state(ThunkState::Unevaluated { expr, env });
```

Safe because `ThunkId` is a plain integer (Copy, no lifetime), and
`arena.get(id)` returns `&Thunk` whose `RefCell` allows interior mutation.
The placeholder → real state transition is monotonic, matching the existing
Unevaluated → InProgress → Materialized lifecycle.

**Pros:**
- Zero dependencies, zero per-item overhead beyond the Thunk itself
- `ThunkId` is `Copy` — cheap to store in environments, thunk states, values
- Pre-allocate slots trivially (push placeholder, fill later)
- Bounds-checked indexing (0.5–3% overhead, often optimized away by LLVM)
- Bulk deallocation by dropping the `Vec`
- No `unsafe` required
- Identical mutation model to current code (RefCell)

**Cons:**
- No deletion of individual thunks (append-only) — not needed for tinct
- No generation counter (use-after-free detectable only by bounds check, not
  by stale-handle detection) — not needed when arena lifetime = section scope
- Must pass `&ThunkArena` to every function that accesses thunks

**Precedent:** cranelift's `PrimaryMap<K, V>` is exactly this pattern — a
`Vec<V>` with typed `K` index handles (`Inst`, `Block`, `Value` are all u32
newtypes). cranelift chose indices over pointers explicitly because Rust
ownership makes pointer-based graphs hard, and u32 saves space vs 64-bit
pointers. No deletion support, no generation counters.

### Approach B: `id-arena` Crate

The `id-arena` crate wraps the Vec + index pattern with a typed `Id<T>` handle
and arena-level generation counter (prevents cross-arena ID confusion).

```rust
use id_arena::{Arena, Id};

let mut arena = Arena::new();
let id: Id<Thunk> = arena.alloc(thunk);
let thunk_ref: &Thunk = &arena[id];
```

**Pros:**
- Arena-level generation prevents using an ID from arena A in arena B
- Typed `Id<T>` prevents mixing ThunkId with EnvId
- Well-tested, simple implementation

**Cons:**
- No pre-allocate support (`alloc` returns ID only after inserting the value)
  — letrec requires a workaround (allocate with dummy value, fill later)
- No meaningful advantage over hand-rolled Vec for tinct's use case
- Additional dependency for ~50 lines of equivalent hand-rolled code
- Arena-level generation adds 8 bytes to every `Id` (vs 4 bytes for plain u32)

**Assessment:** Functionally equivalent to Approach A with minor ergonomic
differences. The generation counter prevents cross-arena confusion but adds
overhead that's unnecessary when arena lifetime is structurally scoped to
document sections.

### Approach C: `slotmap` / `thunderdome` (Generational Arenas)

Generational arenas add a per-slot generation counter to detect use-after-free.

```rust
use thunderdome::{Arena, Index};

let mut arena = Arena::new();
let idx: Index = arena.insert(thunk);
let thunk_ref: &Thunk = &arena[idx];
arena.remove(idx);  // invalidates idx's generation
```

**Pros:**
- Per-slot generation counters detect stale handles at runtime
- Support individual deletion + reuse of slots
- `thunderdome` supports non-Copy types on stable Rust

**Cons:**
- 4 bytes overhead per slot (generation counter) — wasted for append-only use
- 8-byte handles (index + generation) vs 4-byte u32
- Deletion support adds complexity not needed for tinct
- `slotmap` requires `Copy` on stable Rust for its default map
- Pre-allocate pattern is awkward (insert dummy, get key, modify via `&mut`)

**Assessment:** Designed for entity-component systems where entities are
created and destroyed dynamically. tinct thunks are never individually deleted
— they live until the arena drops. The generation counter and deletion
machinery are pure overhead.

### Approach D: `typed-arena` / `bumpalo` (Reference-Based Arenas)

These arenas return `&'arena T` references instead of integer handles.

```rust
use typed_arena::Arena;

let arena = Arena::new();
let thunk: &Thunk = arena.alloc(Thunk::new(/* ... */));
```

**Pros:**
- Zero per-item overhead
- Ergonomic — returns `&T` directly, no handle lookup

**Cons:**
- **Cannot create self-referencing structures.** The borrow checker prevents
  storing `&'arena Thunk` inside another `&'arena Thunk` because allocation
  requires `&mut self` on the arena, which conflicts with outstanding `&T`
  borrows. This is the fundamental limitation for interpreter runtimes.
- Letrec is impossible without `unsafe` — you can't get a reference to a
  thunk and then modify it (allocation invalidates existing references).
- Would require `unsafe` pointer manipulation for all cyclic structures.
- `bumpalo` skips destructors by default (fine for tinct's `Thunk` but
  surprising).

**Assessment:** Reference-based arenas don't work for tinct. The
self-referencing requirement (letrec environments ↔ thunks) is fundamental,
and these crates can't express it without `unsafe`. rustc uses `typed-arena`
because types are immutable after allocation — tinct's thunks are mutable.

### Approach E: GhostCell / qcell (Zero-Cost Interior Mutability)

Replace `RefCell<ThunkState>` with compile-time-checked interior mutability.

**GhostCell** uses a branded lifetime `'id` to prove at compile time that only
one `GhostToken<'id>` exists, enabling safe mutation without runtime borrow
checks:

```rust
use ghost_cell::{GhostToken, GhostCell};

GhostToken::new(|token| {
    let cell = GhostCell::new(ThunkState::Unevaluated { .. });
    let state: &ThunkState = cell.borrow(&token);
    let state_mut: &mut ThunkState = cell.borrow_mut(&mut token);
});
```

**qcell** variants:
- `QCell`: runtime ID check (like RefCell but owner-based)
- `TCell`/`LCell`: zero-cost like GhostCell, type-level or lifetime-level
  branding

**Pros:**
- Zero runtime overhead (no borrow flag, no runtime checks)
- Eliminates RefCell panic risk (borrow violations caught at compile time)
- GhostCell is formally verified (RustBelt project)

**Cons:**
- **Severe ergonomic burden.** The `'id` lifetime parameter propagates through
  every type and function that touches thunks — `ThunkArena<'id>`,
  `ThunkState<'id>`, `Environment<'id>`, `eval<'id>()`, `materialize<'id>()`,
  every builtin signature. This is a pervasive change to the entire evaluator.
- Cannot return GhostCell values from closures (lifetime captured)
- The token must be threaded through all evaluation functions as a parameter
- Ecosystem-incompatible (IndexMap, Vec, etc. don't know about GhostCell)
- RefCell's runtime overhead is negligible — a single `usize` flag checked on
  borrow, branch-predicted to success. In profiling of interpreter runtimes,
  RefCell overhead is unmeasurable.

**Assessment:** GhostCell solves a problem tinct doesn't have. RefCell panics
are a theoretical risk eliminated by the thunk lifecycle's monotonic state
transitions (DESIGN.md §Thunk Lifecycle proves no double-borrow occurs). The
ergonomic cost of propagating `'id` through the entire evaluator far outweighs
the zero-cost benefit. RefCell is the right choice.

## Environment Representation

Alongside the thunk arena, environments transition from chain-based to flat:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EnvId(u32);

pub struct EnvArena {
    envs: Vec<FlatEnv>,
}

pub struct FlatEnv {
    slots: Vec<ThunkId>,       // indexed by compile-time slot number
    parent: Option<EnvId>,     // for stdlib/builtins root chain
}
```

Variable lookup becomes `env.slots[slot]` — O(1) instead of O(depth) chain
walk. The `(level, slot)` pair is assigned by a pre-eval variable resolution
pass (de Bruijn levels, not indices — no shifting needed under substitution).

The environment arena uses the same `Vec + newtype index` pattern as thunks.
Two separate arenas (`ThunkArena` + `EnvArena`) with cross-references via
their respective ID types.

**Migration identity preservation** requires two translation tables
(DESIGN.md §Allocation Strategy): `HashMap<ThunkId, Rc<Thunk>>` and
`HashMap<EnvId, Rc<RefCell<Environment>>>`. Without the env table, two
closures sharing an environment in the arena would become independent copies
after migration, breaking the sharing invariant.

## What Would Change

### ThunkState References

Every `Rc<Thunk>` in the codebase becomes `ThunkId`:

| Current | Arena |
|---------|-------|
| `Rc<Thunk>` | `ThunkId` |
| `Rc::new(Thunk::new_materialized(...))` | `arena.alloc(Thunk::new_materialized(...))` |
| `Rc::clone(&thunk)` | `id` (Copy, free) |
| `thunk.state()` | `arena.get(id).state()` |

### Value Type

`Value::Dict`, `Value::Seq`, `Value::Function` change from holding `Rc<Thunk>`
to `ThunkId`:

```rust
pub enum Value {
    // ... primitives unchanged ...
    Dict(IndexMap<Key, ThunkId>),
    Function { params: Rc<Vec<Param>>, body: Rc<Spanned<Expr>>, env: EnvId },
    Seq { head: ThunkId, tail: ThunkId },
}
```

### BuiltinFn Signature

Builtins need arena access:

```rust
pub type BuiltinFn = fn(BuiltinArgs, &mut ThunkArena) -> EvalResult<ThunkId>;
```

Or via EvalContext if arena is stored there.

### Evaluator Functions

`eval()` and `materialize()` receive `&mut ThunkArena` (or `&EvalContext`
containing the arena). In the CEK machine design, the arena is a field of the
machine state, accessed throughout the main loop.

## Recommendation

**Approach A: hand-rolled `Vec<Thunk>` + `ThunkId(u32)`, keep `RefCell` for
thunk mutation.**

### Rationale

1. **Simplest correct solution.** Zero dependencies, zero overhead, handles
   all four requirements. No crate offers a meaningful advantage for tinct's
   append-only, section-scoped, mutable-thunk use case.

2. **cranelift precedent.** cranelift's `entity` module (`PrimaryMap<K,V>`) is
   the same pattern deployed at production scale. They chose it for the same
   reasons: Rust ownership makes pointer graphs hard, indices are cheap and
   Copy, deletion isn't needed.

3. **RefCell is correct.** GhostCell/qcell solve a problem tinct doesn't have
   (RefCell panics are prevented by the thunk lifecycle's monotonic state
   machine). The ergonomic cost of branded lifetimes is prohibitive.

4. **No deletion needed.** slotmap/thunderdome's generation counters are
   overhead for an append-only arena. Stale handle bugs are prevented
   structurally: ThunkIds are only valid within the arena's section lifetime,
   and the migration algorithm translates all reachable IDs before the arena
   drops.

5. **`u32` is sufficient.** 2³² = ~4 billion thunks per section. A single
   document section producing 4 billion thunks would exhaust memory long
   before the index overflows. Use `u32` (4 bytes) instead of `usize` (8
   bytes) to match cranelift and halve handle size.

### Implementation Sketch

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ThunkId(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EnvId(u32);

pub struct ThunkArena {
    thunks: Vec<Thunk>,
}

impl ThunkArena {
    pub fn with_capacity(cap: usize) -> Self {
        Self { thunks: Vec::with_capacity(cap) }
    }

    pub fn alloc(&mut self, thunk: Thunk) -> ThunkId {
        let id = ThunkId(self.thunks.len() as u32);
        self.thunks.push(thunk);
        id
    }

    pub fn get(&self, id: ThunkId) -> &Thunk {
        &self.thunks[id.0 as usize]
    }
}

pub struct EnvArena {
    envs: Vec<FlatEnv>,
}

pub struct FlatEnv {
    slots: Vec<ThunkId>,
    parent: Option<EnvId>,
}
```

### Phased Adoption

This arena work is part of the `iterative-eval` sprint. The adoption order:

**Step 1: Variable resolution pass.** Pre-eval pass assigns `(level, slot)`
pairs to every `VarRef` in the AST. This is a prerequisite for flat
environments — without slot indices, `FlatEnv` can't do O(1) lookup. This
pass also enables TCO detection (identifying tail-position calls).

**Step 2: ThunkArena + EnvArena.** Introduce the arena types. Initially, the
evaluator creates one arena per `eval_file()` call. Thunks and environments
are allocated in the arena. `Value`, `ThunkState`, `BuiltinFn` signatures
change to use `ThunkId`/`EnvId`.

**Step 3: CEK machine.** The iterative evaluator loop holds the arena as part
of its machine state. The `Cont` enum's variants use `ThunkId`/`EnvId`
instead of `Rc<Thunk>`/`Rc<RefCell<Environment>>`.

**Step 4: Selective migration at `---`.** Implement the migration algorithm
from DESIGN.md §Allocation Strategy. Two translation tables
(`HashMap<ThunkId, Rc<Thunk>>` + `HashMap<EnvId, Rc<RefCell<Environment>>>`)
preserve identity across the boundary.

### Deferred: If Deletion Is Ever Needed

If a future feature requires individual thunk deletion (e.g., garbage
collection within long-running REPL sessions), migrate from `Vec<Thunk>` to
`thunderdome::Arena<Thunk>`. thunderdome's generation counters detect stale
handles, supports non-Copy types, and the API is compatible. This is a
localized change to `ThunkArena`'s internals — `ThunkId` would become
`thunderdome::Index`, and the rest of the codebase would not change.

## References

**Arena patterns in Rust:**
- Manish Goregaokar (2021). "Arenas in Rust." Blog post. — Survey of
  typed-arena, bumpalo, and index-based patterns.
- matklad (2018). "Newtype Index Pattern." Blog post. — The pattern used by
  cranelift and rust-analyzer.
- cranelift `entity` module — `PrimaryMap<K,V>`, `SecondaryMap<K,V>`, typed
  index handles.

**Production arena implementations:**
- rustc `rustc_arena` — DroplessArena for Copy types, TypedArena for Drop
  types. `'tcx` lifetime model. Immutable after allocation.
- salsa — `salsa::Id` (u32) for incremental computation. Stable identity
  across revisions.
- cranelift — `PrimaryMap<K,V>` (Vec-based) with u32 index handles. No
  deletion. Chosen explicitly because Rust ownership makes pointer graphs hard.

**Interior mutability:**
- Yanovski, J. et al. (2021). "GhostCell: Separating Permissions from Data
  in Rust." *ICFP '21.* — Zero-cost interior mutability via branded lifetimes.
  Formally verified. Ergonomic cost prohibitive for whole-evaluator adoption.
- `qcell` crate — QCell (runtime), TCell/LCell (zero-cost). Similar tradeoffs
  to GhostCell.

**Interpreter runtimes:**
- Nix — Boehm GC, flat Value arrays with de Bruijn levels. In-place thunk
  update. C++.
- Nickel — `Rc<RefCell<Closure>>`, same model as tinct's current approach.
  No arenas. Rust.

**Theory:**
- Tofte, M. & Talpin, J.-P. (1997). "Region-based memory management."
  *Information and Computation*, 132(2), 109–176. — Arena ≈ simplified region.
- de Bruijn, N.G. (1972). "Lambda calculus notation with nameless dummies."
  *Indagationes Mathematicae*, 34, 381–392. — Flat environments use de Bruijn
  levels (not indices).
