# What If: Circular Dependency Error Path Reconstruction

**State:** Accepted — 2026-05-04

What would it take to show the full A→B→A cycle chain when `ThunkState::InProgress` is detected, instead of reporting only the blackholed thunk's span?

## Current State

When tinct detects a circular dependency (a thunk is forced while it is already `InProgress`), it reports a `CircularDependency` error with the span of the blackholed thunk. This gives the user only the endpoint of the cycle — not the path that led there.

```
[E040] circular dependency detected while evaluating x at 3:5
```

The user sees where `x` is defined but not the chain `a → b → x → a` that caused the cycle.

### What's Missing

1. The full cycle chain ("evaluating x requires y, which requires z, which requires x")
2. The spans of each step in the chain

## Design

**Carry a call stack alongside `include_guard` in `EvalState`.** The evaluation state already has `include_guard: HashSet<PathBuf>` to detect `include` cycles. The same pattern applies to thunk evaluation cycles: maintain a `Vec<(String, Span)>` call stack that records each thunk being evaluated.

```rust
struct EvalState {
    include_guard: HashSet<PathBuf>,
    include_cache: HashMap<PathBuf, Rc<Thunk>>,
    eval_stack: Vec<(String, Span)>,  // NEW: (thunk origin label, span) for cycle reporting
}
```

**Push on enter, pop on exit.** When `materialize()` transitions a thunk from `Unevaluated` to `InProgress`:
1. Push `(thunk.origin.to_string(), thunk.span)` onto `eval_stack`
2. On successful materialization: pop
3. On `InProgress` detection (cycle): the current `eval_stack` contains the full cycle chain — construct a `CircularDependency` error with the chain

**Precedent survey:**
- **Nix** (`callPackage` cycle errors): Nix's evaluator tracks a `call_stack` (vector of attribute paths) and reports the full chain: `infinite recursion encountered ... callPackage at a.nix:5 → callPackage at b.nix:12 → ...`
- **GHC** (mutual recursion detection): GHC's typechecker tracks `recursion_stack` for `{-# NOINLINE -#}` cycles; the pattern is identical.

**Error format:**
```
[E040] circular dependency detected while evaluating x at 3:5
  cycle: a (1:1) → b (2:3) → x (3:5) → [cycle back to a]
```

### Memory cost

`eval_stack` is a `Vec<(String, Span)>` — at most `MAX_EVAL_DEPTH = 256` entries at peak. Each entry: `String` (origin label, typically 5–30 chars) + `Span` (48 bytes). Upper bound: 256 × ~80 bytes ≈ 20 KB. This is acceptable for a per-session mutable state structure.

### Performance cost

Push/pop on every thunk force. At `materialize()`'s call frequency (hot path), this adds one `Vec::push` + `Vec::pop` per thunk. At the common case where there's no cycle, the push/pop is ~4 ns each. This is measurable on tight inner loops. **Gate behind `EvalConfig.track_cycle_path: bool`** (default true for interactive/REPL, optional for `--no-cycle-track` in performance-critical batch mode).

## What Would Change

### `src/eval.rs` / `src/eval_materialize.rs`

**Proposed:** Push/pop `(origin, span)` in the `Unevaluated → InProgress` transition. Construct cycle chain in `CircularDependency` error.
**Impact:** Moderate — the hot path of `materialize()` gains two Vec operations.

### `src/error.rs`

**Proposed:** `ErrorKind::CircularDependency` gains `cycle_chain: Vec<(String, Span)>` field. `Display` renders the chain.
**Impact:** Minor — ErrorKind variant extends, Display updates.

## Phased Adoption

### Phase 1: Cycle chain in EvalState

Add `eval_stack: Vec<(String, Span)>` to `EvalState`. Push/pop in `materialize()`. Construct chain on `InProgress` detection. Render in error Display.

### Phase 2: Performance gate

If Phase 1 shows measurable overhead on benchmarks, add `EvalConfig.track_cycle_path: bool` and skip push/pop when false.

### Prerequisites

- Phase 1: no prerequisites; `EvalState` is already mutable, `include_guard` is the model to follow

### Trigger

- Phase 1: when users report confusing circular dependency errors without chain context (already the case for non-trivial mutual recursion)

## References

- Nix evaluator source: `EvalState.call_stack` in `src/libexpr/eval.cc` — direct precedent for eval_stack in a lazy language evaluator.
