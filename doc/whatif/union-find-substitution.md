# What If: Path-Compressed Union-Find for `Substitution::apply()`

**State:** Proposal

What would it take to replace the current linear TypeVar chain walk in `Substitution::apply()` with O(α(n)) union-find?

## Current State

`Substitution` is a `HashMap<String, Type>` mapping TypeVar names to types. `apply_inner()` follows TypeVar chains: if `t0 → t1` and `t1 → Int`, applying to `t0` requires two lookups. For longer chains (e.g., from chained unification in large record types), this is O(chain_depth) per application. `apply()` is called on every type during inference, so O(chain_depth) compounds across the full inference run.

`MAX_SUBST_SIZE = 50,000` caps substitution growth to prevent O(n²) DoS; chains beyond this cause a type inference error.

### What's Missing

1. O(α(n)) amortized chain walk via path compression
2. Union-by-rank to keep chains short
3. Reduced allocation: path-compressed union-find can avoid the intermediate HashMap representation

## Design

### Approach

Replace `HashMap<String, Type>` in `Substitution` with a union-find structure:

```rust
struct UnionFind {
    // Map TypeVar name → node ID
    name_to_id: HashMap<String, u32>,
    // Node storage: parent[i] == i means root; else follow parent
    parent: Vec<u32>,
    // Node value: Some(Type) at roots (concrete types), None at non-roots
    value: Vec<Option<Type>>,
}
```

- **Path compression:** when following `parent` chain, update all nodes to point directly to root
- **Union-by-rank:** bind the lower-rank TypeVar to the higher-rank root, keeping trees shallow
- **Migration cost:** all `Substitution::apply()`, `Substitution::compose()`, and unification callers change

### Candidate crates

- `union-find` crate: simple, stable, no deps. `QuickFindUf<Type>` with custom rank.
- `petgraph::unionfind`: bundled in `petgraph` (already a transitive dep?). Simpler if already present.
- Hand-rolled: ~80 lines; full control; no new dependency.

## What Would Change

### `src/types.rs`

**Current:** `Substitution { type_map: HashMap<String, Type>, row_map: HashMap<String, RowTail> }`.
**Proposed:** Two union-find structures (one for type vars, one for row vars), or a combined structure.
**Impact:** Major — all `Substitution` construction, `apply`, `compose`, `extend`, unification callers.

**Note:** Row variables (`row_map`) have different semantics (they bind to `RowTail`, not `Type`). Would require either a separate union-find or a tagged value type. This complexity may outweigh the benefit for row vars specifically — consider applying union-find only to `type_map`.

## Prerequisites

- Chain-depth instrumentation added to `apply_inner()` (counts hops per chain; emits max/average on test exit behind `--debug-types`). Profiling on large real-world tinct configs confirms average chain depth ≥4. If chains are consistently depth ≤2, the current HashMap is already O(1) and union-find adds overhead with no benefit.
- `MAX_SUBST_SIZE` guard may be removed or reframed as union-find node count once the structure is replaced.

## References

- Tarjan, R.E. & van Leeuwen, J. (1984). "Worst-case analysis of set union algorithms." *JACM*, 31(2), pp. 245–281. — Path compression + union-by-rank gives O(α(n)) amortized, where α is the inverse Ackermann function.
- crates.io: `union-find` — simple Rust union-find
- crates.io: `petgraph` (UnionFind) — bundled union-find in the graph library
