# Circular Dependency Error Path Reconstruction

## Overview

When tinct detects a circular dependency — a thunk is forced while it is
already `InProgress` — the evaluator reports the full A→B→A cycle chain instead
of only the span of the blackholed thunk.

Before this feature, the error reported only the endpoint of the cycle:

```
[E040] circular dependency detected while evaluating x at 3:5
```

Now the error includes each step in the chain:

```
[E040] circular dependency detected while evaluating x at 3:5
  cycle: a (1:1) → b (2:3) → x (3:5) → [cycle back to a]
```

Implemented in the `error-context` sprint (completed 2026-05-05).

## Design

**A call stack alongside `include_guard` in `EvalState`.** The evaluation state
already has `include_guard: HashSet<(u64, u64)>` to detect `include` cycles
(using `(device, inode)` file identity to avoid TOCTOU races). The same pattern
applies to thunk evaluation cycles: a `Vec<(String, Span)>` call stack records
each thunk being evaluated.

```rust
struct EvalState {
    include_guard: HashSet<(u64, u64)>,          // (device, inode) file identity
    include_cache: HashMap<(u64, u64), Rc<Thunk>>,  // keyed by (device, inode)
    eval_stack: Vec<(String, Span)>,             // (thunk origin label, span) for cycle reporting
}
```

**File identity via `(device, inode)`**: Using `PathBuf` directly for cycle
detection is vulnerable to TOCTOU (time-of-check-time-of-use) races where a
file is replaced between the guard check and the actual read. Using the
`(st_dev, st_ino)` tuple from `stat(2)` as the identity key ensures that the
same physical file is being tracked, even if accessed via different paths
(symlinks, relative vs absolute paths). This matches the approach used by Make,
Git, and other build tools that need to track file identity reliably.

**Push on enter, pop on exit.** When `materialize()` transitions a thunk from
`Unevaluated` to `InProgress`:

1. Push `(thunk.origin.to_string(), thunk.span)` onto `eval_stack`
2. On successful materialization: pop
3. On `InProgress` detection (cycle): the current `eval_stack` contains the
   full cycle chain — a `CircularDependency` error is constructed with the chain

**Precedent survey:**

- **Nix** (`callPackage` cycle errors): Nix's evaluator tracks a `call_stack`
  (vector of attribute paths) and reports the full chain: `infinite recursion
  encountered ... callPackage at a.nix:5 → callPackage at b.nix:12 → ...`
- **GHC** (mutual recursion detection): GHC's typechecker tracks
  `recursion_stack` for `{-# NOINLINE -#}` cycles; the pattern is identical.

### Memory cost

`eval_stack` is a `Vec<(String, Span)>` — at most `MAX_EVAL_DEPTH = 256`
entries at peak. Each entry: `String` (origin label, typically 5–30 chars) +
`Span` (48 bytes). Upper bound: 256 × ~80 bytes ≈ 20 KB. Acceptable for a
per-session mutable state structure.

### Performance cost

Push/pop on every thunk force. At `materialize()`'s call frequency (hot path),
this adds one `Vec::push` + `Vec::pop` per thunk. At the common case where
there's no cycle, the push/pop is ~4 ns each. This is measurable on tight inner
loops. Gated behind `EvalConfig.track_cycle_path: bool` (default true for
interactive/REPL, optional for `--no-cycle-track` in performance-critical batch
mode).

## Implementation

### `src/eval.rs` / `src/eval_materialize.rs`

Push/pop `(origin, span)` in the `Unevaluated → InProgress` transition.
Construct cycle chain in `CircularDependency` error. The hot path of
`materialize()` gains two Vec operations.

### `src/error.rs`

`ErrorKind::CircularDependency` gains `cycle_chain: Vec<(String, Span)>` field.
`Display` renders the chain.

## References

- Nix evaluator source: `EvalState.call_stack` in `src/libexpr/eval.cc` — direct precedent for eval_stack in a lazy language evaluator.
