# What If: Linear Accumulators for tinct

**State:** Accepted — 2026-05-22

**Refines:** [`runtime-v2.md`](runtime-v2.md) — the `Arc` migration makes O(n²) accumulation 10–50× more expensive in absolute terms; this proposal closes that gap.

**Informs:** [`dist-eval.md`](dist-eval.md) — `partition n seq` is on the hot path of every `dist-map` call; O(n²) preprocessing serializes distributed work before a single byte reaches a worker. `Value::Overlay` chains must flatten before wire encoding, making the deferred cost immediate and blocking at the serialization boundary.

---

## Problem

Nearly all accumulator-based stdlib functions are O(n²). Two distinct mechanisms produce the same result:

**`append`-based accumulation** (`values`, `entries`, `reindex`, `zip`, `flatten`, `uniq`, `partition`, `flat-map`):

```tinct
values-step: [fn [let xs ks i acc current-key]
    [values-impl xs ks [+ i 1]
        [append acc [get current-key xs]]]]
```

`builtin_append` calls `require_dict`, which flattens any overlay and clones the entire `IndexMap` (O(n)), then inserts one element (O(1)). Across n iterations: O(1) + O(2) + … + O(n) = **O(n²) eager Arc clones**.

**`merge`-based accumulation** (`from-entries`, `map-entries`, `remove`, `take-while`, `drop-while`, `slice`, `walk`, `transpose`, `group-by`, `deep-merge`, `collect-kv`):

```tinct
map-entries-step: [fn [let f xs ks i acc current-key]
    [map-entries-impl f xs ks [+ i 1]
        [merge acc [make-entry current-key [f ...]]]]]
```

`builtin_merge` returns `Value::Overlay(left, right)` in O(1). The n-deep chain that accumulates costs **O(n²) at flatten time** — at first access, at dict-entry scope promotion in `eval_document`, or at wire serialization in `remote-task`.

### Why It Matters More After runtime-v2

`Arc` clone costs ~10–50 ns versus ~1 ns for `Rc`. An n-element `append` accumulator that took ~50 ms with `Rc` takes 500 ms–2.5 s with `Arc`. The `Overlay` chain's deferred O(n²) becomes unavoidable at the dist-eval wire boundary — `Value::Overlay` cannot be encoded in the tinct-native wire format and must flatten to a concrete dict before encoding. The O(n²) that seemed harmless locally fires synchronously inside `remote-task` submission, blocking the coordinator.

---

## Design

### The Three Construction Mechanisms

Tinct stdlib accumulation uses exactly three mechanisms, chosen by the shape of the computation:

1. **`map` + `collect`** — for functions whose output elements are independent of each other. Each output element is a pure function of one input element; no state flows between steps. Always O(n); requires no new primitives.

2. **`build-dict`** — for functions that build a keyed `Dict` where each key-value pair is derivable from the input without consulting previously inserted entries. Accepts a lazy `Seq` of `[key: K value: V]` entries and materializes them into a pre-allocated `IndexMap` in a single pass. O(n), no `Overlay` depth.

3. **`Value::Builder`** — for functions where construction is stateful: the value inserted at step i depends on what has already been inserted. O(1) amortized per mutation; frozen to a flat `Dict` by `builder-finish`. Formal model: Launchbury & Peyton Jones (1994) ST monad — `make-builder` = `newSTRef`, `builder-set` = `writeSTRef`, `builder-finish` = `runST`.

The partition is exhaustive. Every stdlib accumulation pattern fits exactly one of the three. The O(n²) `append` and `merge` accumulation loops are eliminated from the stdlib.

### `build-dict`

```tinct
build-dict@[Fn [[Seq Entry]] Dict]
```

`build-dict` is a Rust builtin:

1. Collects the `Seq` spine to determine count (one O(n) pass, for `IndexMap::with_capacity`).
2. Inserts each entry: forces the key (required for indexing); stores the value as a thunk without forcing it.
3. Returns `Value::Dict(map)` — a flat `IndexMap`, no `Overlay` chain.

Keys are forced; values stay lazy. `Entry` is `[key: K value: V]`, matching the existing `Entry` type in prelude.

`from-entries` becomes a one-line alias:

```tinct
from-entries: [fn [let pairs] [build-dict pairs]]
```

The canonical `build-dict` pattern for keyed-dict building:

```tinct
map-entries: [fn [let f xs]
    [build-dict
        [map [fn [let k] [key: k  value: [f [key: k  value: [get k xs]]]]]
             [keys xs]]]]
```

`[map ... [keys xs]]` is lazy — O(1) per element. `build-dict` materializes in one O(n) pass.

**Dual dispatch:** `build-dict` accepts `Value::Seq` or integer-keyed `Value::Dict`. For Dict input, reads `dict.len()` directly without a collection pass. Both inputs produce a flat `Value::Dict(IndexMap)` output — no Overlay.

### `Value::Builder`

```rust
pub struct Builder {
    map:    Mutex<Option<IndexMap<Key, ThunkId>>>,
    frozen: AtomicBool,
}

Value::Builder(Arc<Builder>)
```

Builtins:

```tinct
make-builder@[Fn [capacity: @Int] Builder]       # capacity: is a hint for IndexMap::with_capacity
builder-set@[Fn [b@Builder  k  v] Builder]       # O(1) amortized insert; strict on builder + key; returns same builder
builder-delete@[Fn [b@Builder  k] Builder]       # O(1) remove; strict on builder + key; returns same builder
builder-has?@[Fn [b@Builder  k] Bool]            # O(1) contains_key
builder-get@[Fn [b@Builder  k] Any]              # O(1) get; raises if key absent
builder-finish@[Fn [b@Builder] Dict]             # one-shot freeze: extracts IndexMap, returns Value::Dict
builder-snapshot@[Fn [b@Builder] Dict]           # O(n) full clone of current state; does NOT freeze
```

**One-shot invariant.** `builder-finish` takes the `IndexMap` out of `Mutex<Option<...>>` (leaving `None`) and sets `frozen = true`. Every subsequent operation — `builder-set`, `builder-delete`, `builder-has?`, `builder-get`, `builder-finish`, `builder-snapshot` — raises `EvalError::builder_already_finished`. The invariant applies to reads as well as writes: a frozen builder has no valid state to query. `AtomicBool::load(Relaxed)` provides a lock-free fast-path before mutex acquisition for the frozen check.

**Strictness.** `builder-set` and `builder-delete` are strict on the builder argument and the key. Values are stored as thunks — lazy, consistent with all other dict values. Builder operations are designed for sequential use inside a strict fold (`builtin-reduce`), not concurrent access.

**`builder-snapshot`** clones the full `IndexMap` — O(n). It is a diagnostic tool for testing and introspection; it must not appear in construction loops. Use `builder-has?` and `builder-get` (both O(1)) for all hot-path access.

**Not distributable.** `distributable?` returns `false` for any value containing `Value::Builder`. `remote-task` rejects tasks whose environment contains a builder. The pattern: construct locally, call `builder-finish` to produce a flat `Dict`, send the `Dict`. The `Dict` from `builder-finish` is a plain `Value::Dict(IndexMap)` — O(n) to serialize, no Overlay.

**`group-by` using Builder:**

```tinct
group-by: [fn [let f xs]
    [b: [make-builder]]
    [builtin-reduce
        [fn [let b x]
            [k:      [f x]]
            [bucket: [if [builder-has? b k] [builder-get b k] []]]
            [builder-set b k [cons x bucket]]]
        b
        xs]
    [build-dict
        [map [fn [let e] [key: e.key  value: [reverse [collect e.value]]]]
             [entries [builder-finish b]]]]]
```

Each bucket is a lazy `Seq` accumulated by `cons` — O(1) per element. After `builder-finish`, each bucket is reversed and collected in one O(bucket-size) pass. Total: O(n).

### Seq Rewrite for List-Building Functions

Functions that produce integer-keyed output with independent elements use `map` + `collect`. Where element order must be preserved across a stateful fold, use `cons` + `reverse` + `collect`:

```tinct
values: [fn [let xs]
    [collect [map [fn [let k] [get k xs]] [keys xs]]]]
```

`cons` is O(1) prepend onto a lazy `Seq`. `reverse` is a single O(n) pass in Rust (pre-allocated). `collect` materializes to an integer-keyed `IndexMap` in O(n). No `IndexMap` is allocated during accumulation.

**`uniq` remains O(n²) overall.** The `contains?` check per element is O(n), dominating the algorithm. The `cons`-based rewrite improves the accumulation constant (eliminates the per-step O(n) `append` clone) but does not change the asymptotic bound. True O(n) `uniq` requires a hash-based seen set, which is a separate design concern.

---

## Complexity

| Function | Before | After |
|----------|--------|-------|
| `values`, `entries`, `reindex` | O(n²) eager | O(n) |
| `zip`, `flatten` | O(n²) eager | O(n) |
| `partition` (predicate) | O(n²) eager | O(n) |
| `uniq` | O(n²) eager | O(n²) — contains? dominates; accumulation constant improved |
| `flat-map` (Dict input) | O(n²) eager | O(n²) — see §Scope Boundary |
| `from-entries`, `collect-kv` | O(n²) deferred | O(n) |
| `map-entries`, `remove` | O(n²) deferred | O(n) |
| `take-while`, `drop-while`, `slice` | O(n²) deferred | O(n) |
| `walk`, `transpose` | O(n²) deferred | O(n) |
| `deep-merge` | O(n²) deferred | O(n) construction; O(|a|+|b|) at first access — see §Scope Boundary |
| `group-by` | O(n²) eager+deferred | O(n) |
| Wire serialization (Overlay-heavy) | O(n²) at boundary | O(n) — all stdlib output is now flat `Value::Dict` |
| `partition n seq` (dist.llt) | O(n²) before first dispatch | O(n) — authored with `build-dict` from day one |

### Scope Boundary

Two functions sit at the boundary of this proposal:

**`flat-map` (Dict input).** The Seq-input path is already O(n) (lazy `PendingBuiltin` chains). The Dict-input path uses `[builtin-concat acc [f x]]` in a reduce loop — O(n) clone per step, O(n²) total. `flat-map` underlies `ApplicativeSeq.lift2` and `MonadSeq.bind`. Fixing it requires accumulating a Seq of Seqs via `cons`, then flattening with a single `concat`. This is the same pattern as `flatten` and belongs in this design; the fix is tracked in `linear-accumulators-fixes`.

**`deep-merge`.** The current implementation ends with `[merge a [build-dict ...]]`, which produces `Value::Overlay(a, b)` rather than a flat `Dict`. The Overlay is O(|a|+|b|) to flatten at access time or at the dist-eval wire boundary. A fully flat output requires building over the union key set instead of wrapping with `merge`. This is tracked in `linear-accumulators-fixes` as a fix-later item.

---

## What Would Change

### `src/builtins_dict.rs` — `build-dict`

`builtin_build_dict`:
- One positional argument: `Value::Seq` or integer-keyed `Value::Dict` of `[key: K value: V]` entry dicts
- Seq path: collect spine to determine count, pre-allocate `IndexMap::with_capacity(n)`, iterate inserting (key forced, value as ThunkId)
- Dict path: read `dict.len()` directly, iterate in key order
- Overlay input: delegate to `require_dict` / `flatten_overlay` first
- Returns `Value::Dict(map)` — flat, no Overlay

Register as `"build-dict"` in `standard_builtins()`.

### `src/value.rs` — `Value::Builder`

```rust
pub struct Builder {
    map:    Mutex<Option<IndexMap<Key, ThunkId>>>,
    frozen: AtomicBool,
}
Value::Builder(Arc<Builder>)
```

`type_name()` → `"Builder"`. `PartialEq` → always `false`. `distributable?` → `false`. `Clone` on `Value::Builder` clones the `Arc` (shared underlying state).

`EvalError::builder_already_finished` — new variant for all post-freeze access. User programming error; must not use `EvalError::internal`.

### `src/builtins_dict.rs` — Builder builtins

`make-builder`: optional `capacity:` named arg → `IndexMap::with_capacity`. `builder-set`/`builder-delete`: strict on builder + key; check `frozen` via `AtomicBool::load(Relaxed)` before acquiring mutex; raise `builder_already_finished` if true. `builder-has?`/`builder-get`: same frozen check. `builder-finish`: swap `Some(map)` → `None`, set `frozen = true`, return `Value::Dict(map)`. `builder-snapshot`: clones map while holding mutex lock; returns `Value::Dict`; raises if frozen.

### `stdlib/prelude.llt` — stdlib rewrites

Delete all `-impl`/`-step` recursive helper chains for affected functions. Public signatures unchanged.

**Seq rewrite:** `values`, `entries`, `reindex`, `zip`, `flatten`, `uniq`, predicate `partition` — `map`+`collect` or `cons`+`reverse`+`collect`.

**`build-dict` rewrite:** `from-entries`, `map-entries`, `remove`, `take-while`, `drop-while`, `slice`, `walk`, `transpose`, `collect-kv`, `deep-merge` — `map`/`filter` over keys producing `[key: k value: v]` entries, fed to `build-dict`.

**Builder rewrite:** `group-by` — `make-builder` + `builtin-reduce` + `builder-has?`/`builder-get`/`builder-set` + `builder-finish` + `build-dict`.

Private helpers (`-impl`, `-step`, `-seq-impl`, `reverse-seq`, `contains-seq?`, etc.) live exclusively in the private first dict. Public dict contains only user-facing functions.

### `stdlib/dist.llt`

Not yet written (blocked on `dist-eval.md`). When authored, `partition n seq` uses `build-dict` from the start — no O(n²) pattern introduced. The `dist-map` hot path is linear from day one.

### Tests

Large-input tests (n ≥ 1000) for all rewritten functions verify linear behavior. Builder corpus tests cover: normal construction, `builder-has?` on present/absent keys, double-finish error, all post-freeze access raises `builder_already_finished`.

---

## Prerequisites

- **runtime-v2 Part E (Rc→Arc) complete** — `build-dict` and `Value::Builder` are `Arc`-native; implementing on `Rc` would be premature
- **`cons` is O(1)** — `builtin_cons` prepends without cloning the existing `Seq`
- **`reverse` pre-allocates** — `builtin_reverse` uses `IndexMap::with_capacity(n)`

---

## References

- Okasaki, C. (1998). *Purely Functional Data Structures.* Cambridge University Press. §6.1 — Amortized functional queues via two-list representation; direct antecedent of the `cons`+`reverse` accumulator pattern.
- Launchbury, J. & Peyton Jones, S.L. (1994). "Lazy functional state threads." In *PLDI '94*, pp. 24–35. ACM. doi:10.1145/178243.178246 — The ST monad as a safely-encapsulated mutable region that freezes to a pure value via rank-2 types (`forall s. ST s a`). Formal model for `Value::Builder`: `make-builder` = `newSTRef`, `builder-set` = `writeSTRef`, `builder-finish` = `runST`. Tinct enforces the use-once invariant dynamically (frozen flag) where Haskell enforces it statically.
- Hickey, R. (2009). "Are We There Yet?" JVM Language Summit keynote. — Clojure's transient-then-persistent bulk-construction pattern; direct model for `Value::Builder`.
- Bagwell, P. (2001). "Ideal Hash Trees." EPFL Technical Report. — HAMT persistent data structure; background for why transients outperform persistent tries for purely-constructive single-use accumulation.
- Marlow, S. (ed.) (2010). *Haskell 2010 Language Report.* `Data.List` — Strict left fold (`foldl'`) as the canonical accumulator for list construction; the pattern `builtin-reduce` with a builder adapts for tinct.
