# What If: Value Serializer Visitor Pattern

**State:** Proposal

What would it take to unify `value_to_json` and `value_to_display_string` via a shared traversal?

## Current State

`src/lib.rs` contains two value serializers: `value_to_json()` (lines 112–160) and `value_to_display_string()` (lines 162–211). Both traverse the same `Value` enum — `Dict`, `Seq`, `Int`, `Float`, `String`, `Bool`, `Null`, `Proxy` — with identical structural logic (recursive dict/seq traversal, depth limiting) but diverging at leaf rendering (JSON escaping vs display format) and error handling.

Nickel uses a single `Display` impl on its `Term` type with a `fmt::Formatter`-based approach — no visitor trait.

### What's Missing

1. Reduction of ~60 lines of duplicated traversal logic
2. Easier addition of future output formats (YAML, TOML) without copy-pasting traversal

## Design

**Profile before refactoring.** The visitor pattern is only worthwhile if: (a) the duplication causes real maintenance burden, or (b) a third serializer is planned. For two serializers, a shared `traverse()` function with leaf callbacks may reduce code without adding abstraction overhead.

### Option A: Generic closure-based traversal (recommended if unifying)

```rust
fn traverse_value<E>(
    val: &Value,
    ctx: &Rc<EvalContext>,
    depth: usize,
    on_int: impl Fn(i64) -> Result<String, E>,
    on_string: impl Fn(&str) -> Result<String, E>,
    on_dict_begin: impl Fn() -> Result<String, E>,
    // ...etc
) -> Result<String, E>
```

This avoids the trait object overhead of a `ValueVisitor<Output>` trait while eliminating structural duplication.

### Option B: `ValueVisitor<Output>` trait

```rust
trait ValueVisitor {
    type Output;
    fn visit_int(&self, n: i64) -> Self::Output;
    fn visit_string(&self, s: &str) -> Self::Output;
    // ...
    fn visit_dict(&self, entries: impl Iterator<Item = (&Key, &Self::Output)>) -> Self::Output;
}
```

Adds ~40 lines of trait definition, requires boxing or monomorphization. For two implementations, this is more indirection than the duplication it replaces. Nickel specifically avoids this pattern.

### Verdict

If traversal is a hotspot: benchmark first. If it is not a hotspot: **defer**. The duplication is ~60 lines and the two functions have diverged intentionally (JSON requires escaping; display format uses tinct's `<builtin>` notation for functions). A visitor would force artificial unification of these rendering decisions. Nickel's approach (separate Display impl per type) is the right precedent.

## What Would Change

### `src/lib.rs`

**Proposed:** Extract `fn traverse_dict_entries(map: &IndexMap<...>, depth: usize, ctx: ..., render_entry: impl Fn(&Key, &Value) -> String)` shared helper — ~20 lines. The two serializers call this shared helper. No visitor trait.
**Impact:** Minor.

## Phased Adoption

### Phase 1: Profile

Check if `value_to_json` or `value_to_display_string` appears in profiling hotspots.

### Phase 2: Unify (if warranted and a third serializer is added)

If a third serializer (YAML, TOML) is needed, extract the shared traversal at that point. Premature extraction for two serializers is not recommended.

### Trigger

- Phase 1: during performance work
- Phase 2: when a third output format (e.g., `--format yaml`) is implemented

## References

- Nickel: single `Display` impl on `Term` — avoids visitor. Direct precedent for tinct's approach.
- Visitor pattern: GoF Design Patterns (1994) §Visitor — standard pattern but often over-applied for < 3 implementations.
