# Proof Sketch: Thunk Lifecycle Bisimulation

**Status:** Sketch (not mechanized)
**Proof assistant:** Coq (planned)
**Property:** Lazy evaluation in Tinct is observationally equivalent to call-by-value
evaluation — any value that is ultimately forced produces the same result under both
strategies.

## Informal Statement

Let `e` be a closed LLT expression and `E` an environment in which `e` is well-typed.
Let `v` be the value obtained by fully materializing `e` under lazy evaluation (i.e.,
creating a `Thunk` and then calling `force()`). Let `v'` be the value obtained by
eagerly evaluating `e` under the same environment.

**Claim:** `v ≅ v'` under observable equality (both are the same LLT value, or both
diverge, or both raise the same error kind).

This is the standard thunk bisimulation / Plotkin adequacy theorem, stated for
Tinct's `Thunk`-based evaluator.

## Tinct Thunk Lifecycle

A `Thunk` in Tinct (`src/value.rs`) moves through the following states:

```
Unevaluated(Expr, Env)
    │
    │  force() called
    ▼
Evaluating                  ← cycle detection guard (raises E041 if re-entered)
    │
    │  eval() completes
    ▼
Evaluated(Value)            ← memoized; subsequent force() calls return cached Value
```

Key invariant: once a `Thunk` reaches `Evaluated(v)`, all future calls to `force()`
return the same `v`. This is the **memoization invariant**.

## Bisimulation Strategy

The proof would proceed as a simulation relation `R` between thunk-based (lazy) and
direct (eager) evaluation:

```
R(thunk t, value v)  iff  force(t) ≅ v
```

**Base cases:**

- `R(Thunk(Int n, _), Int n)` — forcing a literal thunk yields the integer directly.
- `R(Thunk(Bool b, _), Bool b)` — same for booleans.
- `R(Thunk(Str s, _), Str s)` — same for strings.

**Inductive cases (selection):**

- `R(Thunk(Dict {k: t_i}, env), Dict {k: v_i})` if `∀i. R(t_i, v_i)` — dict thunk
  bisimulates dict value when all entry thunks bisimulate their values.
- `R(Thunk(Call(f, args), env), v)` if eager application of `f` to `args` in `env`
  produces `v` — function call thunk bisimulates the result of the call.

**Cycle case:**

When `force(t)` re-enters `t` while it is in state `Evaluating`, the evaluator raises
`E041 CircularDependency`. The eager evaluator would diverge (infinite loop). These
are bisimilar under an extended relation that equates "detectable cycle → E041" with
"divergence".

## Memoization Correctness Lemma

**Lemma (Idempotence):** For any thunk `t` in state `Evaluated(v)`,
`force(t) = v` for all subsequent calls.

**Proof sketch:** By inspection of the `RefCell<ThunkState>` match in `src/value.rs`:
the `Evaluated` arm returns `Rc::clone(&v)` without modifying state. Since the state
is never written from `Evaluated`, the result is stable.

## Open Questions for Mechanization

1. **Environment aliasing:** Tinct environments use `Rc<RefCell<Environment>>` with
   shared parent chains. The proof must show that environment mutation (binding new
   keys) is safe under sharing — i.e., that no thunk observes a modified environment
   after creation. This requires an ownership discipline argument.

2. **Letrec dicts:** Dict entries are bound in a mutually recursive environment (letrec
   semantics). The bisimulation must handle the fixed-point construction for mutually
   recursive bindings.

3. **Builtins:** Builtin functions (Rust-native) are opaque from the proof's
   perspective. They must be axiomatized as deterministic total (or error-raising)
   functions.

4. **Depth limit:** The `MAX_EVAL_DEPTH` guard (`src/eval.rs`) means the evaluator
   is partial — it raises `E043 ResourceLimitExceeded` on deeply nested structures.
   The bisimulation holds only for evaluation trees shallower than `MAX_EVAL_DEPTH`.

## References

- Launchbury, J. (1993). "A natural semantics for lazy evaluation." POPL 1993.
  (Section 3 gives the standard thunk-based lazy semantics this proof mirrors.)
- Plotkin, G. (1977). "LCF considered as a programming language." TCS 5(3).
  (Adequacy theorem for lazy PCF — the template for this bisimulation.)
- Kiselyov, O. (2013). "Eff Directly in OCaml." (Levels in type inference, cited
  in `src/typecheck.rs` for `RowTail::RowVar` level semantics.)
