# What If: Formal Verification of tinct Evaluation Semantics

**State:** Proposal

What would it take to verify that tinct's thunk lifecycle semantics are correct — both empirically (property-based testing) and theoretically (confluence proof)?

## Current State

tinct's evaluator makes three semantic adequacy claims in `doc/08-evaluation.md §Thunk Lifecycle — Adequacy`:

1. Any `PendingBuiltin` reduction produces the same final value as the equivalent `Unevaluated → materialize` path
2. `PendingCall` reduction is observationally equivalent to inlining the function body
3. Memoization: a second force of a `Materialized` thunk returns the same value without re-evaluation

And one confluence claim in `doc/08-evaluation.md §Thunk Lifecycle — Semantic Properties`:

4. For any expression in the pure subset, all maximal reduction sequences converge to the same normal form (confluence / diamond property)

These claims are documented assertions. The corpus tests provide evidence but do not systematically cover the reduction-path equivalence space, and no formal proof exists.

### What's Missing

1. Systematic empirical verification of the bisimulation claims via property-based testing
2. A formal confluence argument for the pure tinct subset

## Design

Two complementary verification strategies — testing and proof — address the same underlying correctness goal from different angles.

---

## Part A: Property-Based Testing (Empirical Verification)

**`proptest`-based random program generation + oracle comparison.** Generate a random tinct expression, evaluate it via two different reduction paths, and assert value equality.

### Generator Design

```rust
// In tests/proptest_thunk.rs
use proptest::prelude::*;

// Generate a thunk whose evaluation path can be controlled.
// For claim 1: same builtin + args, two construction paths.
fn arb_strict_builtin_call(ctx: &Rc<EvalContext>)
    -> impl Strategy<Value = (Rc<Thunk>, Rc<Thunk>)>
{
    // Path A: PendingBuiltin { func: builtin_add, args: [thunk(n1), thunk(n2)] }
    // Path B: Unevaluated { expr: [+ n1 n2] }
    // Assert: materialize(A) == materialize(B)
}
```

### Coverage Targets

| Claim | Test |
|-------|------|
| PendingBuiltin ≡ Unevaluated | 500+ generated (builtin, args) pairs across all Seq-annotated builtins |
| PendingCall ≡ inline fn body | 500+ generated (fn, args) pairs; compare PendingCall dispatch vs direct eval |
| Memoization | Force same thunk twice; assert identical result, no re-evaluation (thunk state transitions monotonically) |
| Error memoization | Force a failing thunk twice; assert same `EvalError` both times |
| Cycle detection | Generate mutual dict reference; assert `CircularDependency` error, not hang |

### Random Generators Needed

1. `arb_value` — Int, Float, String, Bool, Null, Dict (bounded depth ≤3), Seq (bounded length ≤10)
2. `arb_strict_builtin` — choose from arithmetic/comparison/string builtins + generate valid typed args
3. `arb_fn_call` — generate a simple lambda `[fn [x] expr]` with a literal body, apply to generated value
4. `arb_circular_dict` — a dict where at least two entries form a cycle (for claim 5)

---

## Part B: Confluence Proof (Theoretical Verification)

**The determinism argument.** tinct's pure subset is deterministic by construction — the state machine has no non-deterministic choice points. Determinism implies confluence trivially.

### Key Insight

Confluence (diamond property) requires: for any expression `e`, if `e →* b` and `e →* c`, then there exists `d` with `b →* d` and `c →* d`. For a **deterministic** evaluator, any two reduction sequences from the same expression are **identical** — `b = c` always. The diamond property holds with `d = b = c`.

### Determinism of the Pure Subset

| Evaluator component | Deterministic? | Reason |
|--------------------|---------------|--------|
| `Unevaluated → InProgress → Materialized` | Yes | Single applicable transition per state |
| `PendingBuiltin → call result` | Yes | Builtin functions are pure Rust functions; same args → same result |
| `PendingCall → bind args → eval body` | Yes | Function application is deterministic; no scheduling |
| `ThunkState::Failed` memoization | Yes | Error is cached; same error on re-access |
| `ThunkState::InProgress` detection | Yes | Yields `CircularDependency`; deterministic |
| `$include` | **No** | File system reads are non-deterministic (excluded from pure subset) |
| `$error` | **No** | User-raised errors may diverge from normal forms (excluded) |

**Claim:** the pure subset (no `$include`, no `$error`) is deterministic, and therefore confluent.

### Extension for PendingBuiltin / PendingCall

These states add new reduction paths beyond the Ariola & Felleisen (1997) call-by-need calculus. The relevant Ariola-Felleisen lemmas to extend:

| Lemma | Extension for tinct |
|-------|---------------------|
| L1: Unique decomposition | `PendingBuiltin` and `PendingCall` are canonical deferred forms — each has a unique evaluation context. No ambiguity about which step to take next. |
| L2: Subject reduction | The type of the materialized value is determined by the builtin's return type and the argument values. Builtin purity guarantees same-args → same-result. |
| L3: Confluence via standardization | Follows from determinism — identical reduction sequences converge. |

### Proof Sketch (to add to doc/08-evaluation.md)

```
Theorem (Confluence of pure tinct):
  For any closed expression e in the pure subset (no $include, no $error),
  all maximal reduction sequences from e converge to the same normal form
  or all diverge.

Proof sketch: By determinism of the evaluation state machine.
The transition function δ(ThunkState, expr, env, ctx) → ThunkState' is a
function (not a relation): each ThunkState has exactly one applicable
transition. Therefore any two reduction sequences are identical, and
confluence follows trivially.

PendingBuiltin and PendingCall are deferred normal forms, not choice
points — they reduce to exactly one outcome when materialized, determined by
the builtin function and the values of their arguments (builtin purity).
The Ariola & Felleisen (1997) lemmas extend to cover these states by
unique decomposition (L1) and subject reduction (L2). □
```

---

## What Would Change

**Status 2026-05-07:** Confluence proof sketch added to `doc/08-evaluation.md §Thunk Lifecycle — Semantic Properties` (Part B complete).

**Open:** `tests/proptest_thunk.rs` covering all five claims:

| Claim | Coverage |
|-------|----------|
| PendingBuiltin ≡ Unevaluated | 500+ generated (builtin, args) pairs across all Seq-annotated builtins |
| PendingCall ≡ inline fn body | 500+ generated (fn, args) pairs; compare PendingCall dispatch vs direct eval |
| Memoization | Force same thunk twice; assert identical result, no re-evaluation |
| Error memoization | Force a failing thunk twice; assert same `EvalError` both times |
| Cycle detection | Generate mutual dict reference; assert `CircularDependency` error, not hang |

`proptest = "1"` added to `[dev-dependencies]` in `Cargo.toml`. The Ariola-Felleisen lemma extension is documented in `doc/08-evaluation.md` alongside the proof sketch.

A Coq or Isabelle formalization of the state machine is a stretch goal, contingent on the proptest suite finding zero failures and a formal semantics specification complete in `doc/08-evaluation.md`.

## Prerequisites

No prerequisites. The existing evaluator is the subject of verification — no upstream features are needed.

## References

- Ariola, Z.M. & Felleisen, M. (1997). "The call-by-need lambda calculus." *J. Functional Programming*, 7(3), pp. 265–301. — The diamond property for call-by-need. tinct extends this with PendingBuiltin/PendingCall states.
- Launchbury, J. (1993). "A natural semantics for lazy evaluation." In *POPL '93*, pp. 144–154. — Heap-based operational semantics for lazy evaluation; the model tinct's thunk lifecycle corresponds to.
- Claessen, K. & Hughes, J. (2000). "QuickCheck: a lightweight tool for random testing of Haskell programs." In *ICFP '00*, pp. 268–279. ACM. — Foundational property-based testing paper; `proptest` is the Rust equivalent.
- Reynolds, J.C. (1972). "Definitional interpreters for higher-order programming languages." In *ACM Annual Conference*, pp. 717–740. — Defunctionalization; the theoretical basis for tinct's PendingCall/PendingBuiltin as defunctionalized continuations.
