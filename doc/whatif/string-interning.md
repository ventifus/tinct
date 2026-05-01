# What If: String Interning for Dict Keys

**State:** Proposal

What would it take to reduce dict key allocation and comparison cost via string interning?

## Current State

`Value::Dict` uses `IndexMap<Key, Rc<Thunk>>` where `Key` is `Key::String(String)` or `Key::Int(i64)`. Every dict construction allocates a new `String` per string key, even for keys like `"name"`, `"age"`, `"enabled"` that appear in every record. Dict key lookup calls `String::eq` — O(n) byte comparison.

In typical tinct config programs (K8s manifests, infrastructure templates), the same 20–50 key names appear in thousands of dict values. Without interning, each creates a separate heap allocation.

### What's Missing

1. Shared key allocations — the same key string `"name"` stored once, not per-dict
2. O(1) pointer-equality key comparison (interned strings compare by pointer, not by bytes)

## Design

**Profile first, then choose the crate.** String interning is only worthwhile if dict key allocation and comparison appear as hotspots in real profiling. The proposal identifies three candidate approaches:

### Option A: `string-interner` crate (recommended if interning is warranted)

`Spur` type — a u32 handle. `StringInterner::get_or_intern(key) -> Spur`. Dict key becomes `Key::String(Spur)`. Comparison: `u32 == u32`, O(1). The interner is a global or session-scoped `Arc<StringInterner>`.

- **Pros:** Minimal allocation for repeated keys; comparison is integer equality; well-maintained crate.
- **Cons:** Requires session-scoped interner lifecycle; strings cannot be extracted without the interner; adds a dependency.
- **When:** When `String` comparison is confirmed as a hotspot via `cargo flamegraph` or DHAT.

### Option B: `lasso` crate (concurrent)

Same Spur model but thread-safe. Only relevant if tinct adds parallel evaluation. Skip for now.

### Option C: Hand-rolled `HashMap<String, u32>` index

Maintain a `HashMap<String, u32>` + `Vec<String>` (index → string). `u32` is the interned handle. More control, no dependency. ~50 lines. Viable if `string-interner`'s API doesn't fit tinct's model.

## What Would Change

### `src/value.rs`

**Current:** `Key::String(String)` — owned string per key.
**Proposed:** `Key::String(Spur)` — 4-byte handle. `String` accessible via `interner.resolve(spur)`.
**Impact:** Major — all Key construction (dict literals in eval.rs), all key display (error messages, JSON output), all key comparison. Every `match key { Key::String(s) => ... }` changes.

### Session lifecycle

The interner must live as long as any `Key::String` value. Most natural location: `EvalContext.config` (shared across the eval session). All key construction goes through `ctx.config.interner.get_or_intern(s)`.

## Profiling Gate

**Do not implement without profiling first.** Run `cargo flamegraph` or DHAT on a large tinct config (K8s manifest with 500+ unique dicts, each with 5–10 string keys). If `String::from`, `String::clone`, or `PartialEq<String>` appear in the top-10 hotspots, proceed. If not, the optimization is not load-bearing and this proposal is superseded.

## Phased Adoption

### Phase 1: Profile

Measure `String` allocation and comparison cost on representative workloads. Use `heaptrack` or DHAT.

### Phase 2: Intern (if warranted)

If Phase 1 confirms the hotspot: add `string-interner` dependency, add `interner: StringInterner` to `EvalConfig`, change `Key::String(String)` to `Key::String(Spur)`, update all match arms and display sites.

### Prerequisites

- Phase 1: no prerequisites
- Phase 2: Phase 1 profiling confirms string interning is load-bearing; arena migration considered (arena changes the allocation model, which affects whether interning remains beneficial after arena)

### Trigger

- Phase 1: before any large config performance work
- Phase 2: when Phase 1 profiling confirms string key allocation/comparison is in the top-5 hotspots

## References

- crates.io: `string-interner` — `Spur` type, arena-backed string pool
- Nix: uses `string_table` / `Symbol` for attribute names — exact same use case
