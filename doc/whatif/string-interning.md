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

String interning uses the `string-interner` crate: `Spur` type — a u32 handle. `StringInterner::get_or_intern(key) -> Spur`. Dict key becomes `Key::String(Spur)`. Comparison: `u32 == u32`, O(1). The interner is a session-scoped `Arc<StringInterner>` stored in `EvalContext.config`.

If `string-interner`'s API does not fit tinct's model, a hand-rolled `HashMap<String, u32>` + `Vec<String>` index (~50 lines, no new dependency) is the fallback. The `lasso` crate (concurrent Spur model) is not needed unless tinct adds parallel evaluation.

## What Would Change

### `src/value.rs`

**Current:** `Key::String(String)` — owned string per key.
**Proposed:** `Key::String(Spur)` — 4-byte handle. `String` accessible via `interner.resolve(spur)`.
**Impact:** Major — all Key construction (dict literals in eval.rs), all key display (error messages, JSON output), all key comparison. Every `match key { Key::String(s) => ... }` changes.

### Session lifecycle

The interner must live as long as any `Key::String` value. Most natural location: `EvalContext.config` (shared across the eval session). All key construction goes through `ctx.config.interner.get_or_intern(s)`.

## Prerequisites

- Profiling with `cargo flamegraph` or DHAT on a representative large tinct config (K8s manifest with 500+ unique dicts, each with 5–10 string keys) confirms that `String::from`, `String::clone`, or `PartialEq<String>` appear in the top-10 hotspots. Without this evidence, the optimization is not load-bearing.
- Arena migration considered before implementation: arena changes the allocation model, which affects whether interning remains beneficial after arena adoption.

## References

- crates.io: `string-interner` — `Spur` type, arena-backed string pool
- Nix: uses `string_table` / `Symbol` for attribute names — exact same use case
