# Safe Rust Arena Patterns for Thunks and Environments

## Overview

tinct's allocation model uses index-based arenas (`Vec<Thunk>` + `ThunkId`)
with flat environments and selective migration at `---` boundaries, replacing
the previous `Rc<Thunk>` / `Rc<RefCell<Environment>>` model.

Arena allocation provides:

1. **Zero per-thunk Rc overhead** — thunks are plain struct entries in a
   `Vec`, accessed by `u32` index. No reference counting, no borrow flags.
2. **Cycle-free memory model** — letrec self-references are integer indices
   into the same arena. When the arena drops, everything drops.
3. **O(1) variable lookup** — flat environments with `(level, slot)` de Bruijn
   addressing replace parent-chain traversal.
4. **Section-scoped bulk deallocation** — dropping the arena frees all thunks
   and environments for a document section in one operation.
5. **Copy handles** — `ThunkId(u32)` is `Copy`, eliminating `Rc::clone`
   overhead in environment slots, thunk states, and value constructors.

## Supersession Notes

- **Phase 3 target state**: The §Design section describes full `ThunkId`-in-Value migration (`BuiltinFn = fn(BuiltinArgs, &mut ThunkArena) -> EvalResult<ThunkId>`, `Value::Dict(IndexMap<Key, ThunkId>)`). The §Implementation section describes both the Phase 2 intermediate form (`Vec<Rc<Thunk>>`) and the Phase 3 target.

## Design

A hand-rolled index arena: `Vec<Thunk>` as the backing store, `ThunkId(u32)`
as the handle.

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
transitions use `thunk.set_state()` / `thunk.transition()` exactly as before.

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

This is the simplest correct solution: zero dependencies, zero per-item
overhead beyond the `Thunk` itself, and no `unsafe` required. `ThunkId` is
`Copy` — cheap to store in environments, thunk states, and values.
Pre-allocate slots trivially (push placeholder, fill later). Bounds-checked
indexing adds 0.5--3% overhead, often optimized away by LLVM. Bulk
deallocation is achieved by dropping the `Vec`.

The append-only model means no individual thunk deletion and no generation
counters for stale-handle detection. Neither is needed: deletion is
unnecessary for tinct's section-scoped evaluation, and stale handles are
prevented structurally because `ThunkId`s are only valid within the arena's
section lifetime. The migration algorithm translates all reachable IDs
before the arena drops.

**Precedent:** cranelift's `PrimaryMap<K, V>` is exactly this pattern — a
`Vec<V>` with typed `K` index handles (`Inst`, `Block`, `Value` are all u32
newtypes). cranelift chose indices over pointers explicitly because Rust
ownership makes pointer-based graphs hard, and u32 saves space vs 64-bit
pointers. No deletion support, no generation counters.

### Why RefCell Is Correct

GhostCell (Yanovski et al., 2021) and qcell offer zero-cost interior
mutability via branded lifetimes, but they solve a problem tinct doesn't have.
`RefCell` panics are prevented by the thunk lifecycle's monotonic state
machine: a thunk transitions Unevaluated -> InProgress -> Materialized/Failed,
and the `InProgress` sentinel prevents re-entrant borrows (blackholing). The
ergonomic cost of threading branded lifetime parameters through every evaluator
function is prohibitive for no correctness benefit.

### Why u32 Is Sufficient

2^32 = ~4 billion thunks per section. A single document section producing
4 billion thunks exhausts memory long before the index overflows. `u32` (4 bytes)
is used instead of `usize` (8 bytes) to match cranelift and halve handle size.

### Environment Representation

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

Variable lookup is `env.slots[slot]` — O(1) per level instead of O(depth)
chain walk. The `(level, slot)` pair is assigned by a pre-eval variable
resolution pass (de Bruijn levels, not indices — no shifting needed under
substitution).

**Caveat — display vector required for true O(1):** `FlatEnv` has a
`parent: Option<EnvId>` chain used for stdlib/builtins root lookups. Without a
*display vector* (de Bruijn 1972 §3) — a `Vec<EnvId>` indexed by level,
prepopulated at closure creation time — resolving a variable at level `k`
still requires walking `k` parent links to reach the right `FlatEnv`, giving
O(level) lookup in the worst case. A display vector trades O(scope_size)
storage at closure creation for guaranteed O(1) slot access at lookup.
Alternatively, copy-on-capture flat closures (Nix model) copy all bindings
into a single flat frame at closure creation time — O(scope_size) creation,
O(1) lookup, but no sharing across closures. The `parent` chain on `FlatEnv`
is retained for the stdlib root only; user-scope lookups via `(level, slot)`
pairs achieve O(1) under the display-vector assumption.

The environment arena uses the same `Vec + newtype index` pattern as thunks.
Two separate arenas (`ThunkArena` + `EnvArena`) with cross-references via
their respective ID types.

### Letrec Compatibility and the Static/Computed Key Split

tinct's letrec dict scoping — where all entries share one environment so
siblings can reference each other — is compatible with de Bruijn slot
assignment, with a caveat for computed keys.

**Static keys** (the common case: `name: $x`, `enabled: true`) have names
known at parse time. A variable resolution pass assigns slot indices 0, 1, 2,
... to entries in order. Sibling mutual references (`x: $y`, `y: $x`) resolve
to `(level=0, slot=k)` — no name lookup needed. Letrec is not a blocker:
`eval_dict()` pre-allocates the `dict_env` (creating the `FlatEnv` with a
pre-sized slot vector) before creating any thunks. All thunks capture the same
`FlatEnv`, and slots are filled as thunks are created.

**Computed keys** (e.g., `[<$expr>: value]`) have names unknown at parse time.
These fall back to a `HashMap<String, ThunkId>` overflow side table on the
`FlatEnv`. This hybrid model handles both cases without special-casing the
resolution pass:

```rust
pub struct FlatEnv {
    slots: Vec<ThunkId>,                   // static keys, indexed by compile-time slot
    overflow: HashMap<String, ThunkId>,    // computed keys, name-indexed
    parent: Option<EnvId>,                 // stdlib/builtins root chain only
}
```

Most real tinct programs use only static keys; computed keys are uncommon, so
the overflow table is usually empty.

### Variable Resolution Pass Design

A single pre-eval analysis walk assigns `(level, slot)` pairs to `VarRef` nodes.
The pass maintains a scope stack and populates a `Cell<Option<(u32, u32)>>` on
each `VarRef`:

```rust
// VarRef gains a resolution cache field:
Expr::VarRef { name: String, resolved: Cell<Option<(u32, u32)>> }

// The pass walks the AST with a scope stack:
struct Resolver {
    scopes: Vec<HashMap<String, u32>>,  // name → slot per nesting level
}
impl Resolver {
    fn enter_dict(&mut self, static_keys: &[String]) { ... }
    fn resolve(&self, name: &str) -> Option<(u32, u32)> {
        for (offset, scope) in self.scopes.iter().rev().enumerate() {
            if let Some(&slot) = scope.get(name) {
                let level = (self.scopes.len() - 1 - offset) as u32;
                return Some((level, slot));
            }
        }
        None  // falls back to name lookup (computed keys, $include-introduced bindings)
    }
}
```

Unresolved references (computed keys, bindings introduced by `$include`) remain
`None` and use the overflow HashMap at runtime. The pass runs after parsing and
before evaluation.

### Contrast with Lua 5.4 Upvalues

Lua 5.4 uses explicit `UpValue` reference cells that functions close over. At
function creation time, the compiler pre-computes which outer variables are
referenced and allocates an upvalue array. Lookup is `upvalue[index]->value` —
one array index plus one pointer dereference.

tinct's letrec model is different: all dict-entry thunks are created
simultaneously capturing the same `FlatEnv`. There is no "closure binding time"
per thunk — all thunks see the same shared environment. This means tinct does
not need upvalue arrays: the `FlatEnv` is shared by reference (`EnvId`), and
slot access directly reaches the correct binding. For outer-scope free
variables, the `parent` chain (retained for stdlib only) provides the
additional lookup level, giving at most two hops for user code (current level

- stdlib root).

**Migration identity preservation** requires two translation tables
(doc/08-evaluation.md §Allocation Strategy): `HashMap<ThunkId, Rc<Thunk>>` and
`HashMap<EnvId, Rc<RefCell<Environment>>>`. Without the env table, two closures
sharing an environment in the arena would become independent copies after
migration, breaking the sharing invariant.

## Implementation

Phase 2 provides the arena infrastructure and migration framework. The arena
stores `Rc<Thunk>` entries; thunks remain `Rc<Thunk>` until Phase 3 completes
the transition to index-based handles.

### Phase 2 Implementation (current)

Thunk references remain `Rc<Thunk>`. The arena stores `Vec<Rc<Thunk>>` and
provides allocation/deallocation infrastructure. Migration between sections uses
the same translation table pattern described in §Design, but translates
`Rc<Thunk>` pointers rather than `ThunkId` indices.

### Phase 3 Implementation

The §Design section above describes the Phase 3 target: `ThunkId(u32)` handles,
`Vec<Thunk>` arena storage, zero `Rc` overhead. Phase 3 changes:

- Every `Rc<Thunk>` becomes `ThunkId(u32)`
- Sharing is via copying the `u32`
- State access is `arena.get(id).state()`
- `Value::Dict`, `Value::Seq`, `Value::Function` hold `ThunkId` and `EnvId`
- Builtins receive `&mut ThunkArena` (or `&EvalContext` containing the arena)
- `eval()` and `materialize()` receive arena access

All pattern matches on `Value` variants and every builtin function signature
will change.

### Individual Thunk Deletion

When individual thunk deletion is needed (e.g., garbage collection within
long-running REPL sessions), `ThunkArena` uses `thunderdome::Arena<Thunk>`.
thunderdome's generation counters detect stale handles, and the API is
compatible with the `ThunkId` abstraction — `ThunkId` becomes
`thunderdome::Index`, and the rest of the codebase does not change.

## References

- Tofte, M. & Talpin, J.-P. (1997). "Region-based memory management."
  *Information and Computation*, 132(2), 109--176. — Arena allocation is a
  simplified instance of region-based memory: each document section is a
  region, bulk deallocation corresponds to region exit.
- de Bruijn, N.G. (1972). "Lambda calculus notation with nameless dummies."
  *Indagationes Mathematicae*, 34, 381--392. — Flat environments use de Bruijn
  levels (not indices) for O(1) variable lookup without shifting under
  substitution.
- Yanovski, J. et al. (2021). "GhostCell: Separating Permissions from Data
  in Rust." *ICFP '21.* — Zero-cost interior mutability via branded lifetimes.
  Formally verified. Evaluated and rejected: ergonomic cost prohibitive for
  whole-evaluator adoption when RefCell is already correct.
- Launchbury, J. (1993). "A natural semantics for lazy evaluation." In
  *POPL '93*, pp. 144--154. ACM. — The thunk lifecycle (Unevaluated ->
  InProgress -> Materialized) that arena allocation must preserve. Sharing
  preservation is the key invariant: two references to the same ThunkId
  must observe the same materialized value.
- Manish Goregaokar (2021). "Arenas in Rust." Blog post. — Survey of
  typed-arena, bumpalo, and index-based patterns in the Rust ecosystem.
- matklad (2018). "Newtype Index Pattern." Blog post. — The `Vec<T>` +
  newtype `usize` pattern used by cranelift and rust-analyzer.
- cranelift `entity` module. — `PrimaryMap<K,V>`, `SecondaryMap<K,V>`,
  typed u32 index handles. Production-scale precedent for this exact pattern.
- Ierusalimschy, R., de Figueiredo, L.H. & Celes, W. (2005). "The implementation of Lua 5.0." *J. Universal Computer Science*, 11(7), pp. 1159–1176. — Flat local variable arrays with upvalue reference cells for closures. Closest precedent for tinct's slot-indexed `FlatEnv`; tinct's letrec model differs by using a shared env rather than per-closure upvalue arrays.
- Nix evaluator. — Boehm GC, flat Value arrays with de Bruijn levels,
  in-place thunk update. C++ reference implementation for lazy configuration
  language evaluation.
- Nickel evaluator. — `Rc<RefCell<Closure>>`, same model as tinct's previous
  approach. No arenas. Rust reference point for the status quo.
