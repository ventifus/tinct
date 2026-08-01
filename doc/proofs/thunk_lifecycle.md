# Proof Sketch: Thunk Settlement Monotonicity and Sharing

**Status:** Sketch (not mechanized)
**Proof assistant:** Coq (planned)
**Property:** Every thunk is evaluated at most once (Launchbury sharing) and,
once settled, returns the same result on every subsequent access (monotonicity).

## Informal Statement

Let `t` be a `Thunk` created in the Tinct evaluator. Then:

1. **Evaluate-at-most-once (sharing):** The body of `t` is evaluated at most once
   across all concurrent tasks that demand `t`.
2. **Monotonicity:** Once `t` is settled with result `r`, every subsequent read of
   `t` yields `r`. No future operation can change or retract `r`.
3. **Settlement guarantee:** If any task begins evaluating `t` (wins `try_claim`),
   then `t` will be settled on all exit paths, including panics.

## Architecture

A `Thunk` (`src/value.rs`) has two fields inside `ThunkInner`:

```text
ThunkInner {
    unevaluated: Mutex<(Option<UnevaluatedState>, Option<task::Id>)>,
    result:      tokio::sync::OnceCell<Result<Value, Arc<EvalError>>>,
    notify:      std::sync::OnceLock<Arc<tokio::sync::Notify>>,
}
```

- `unevaluated` holds the expression to evaluate plus the claiming task's id.
- `result` is a write-once cell for the terminal value or error.
- `notify` wakes tasks awaiting settlement. The `OnceLock` provides lazy initialization — the `Notify` is created on first await, not at thunk construction time.

## State Machine

```text
Unevaluated(state)          unevaluated = (Some(state), None)
    │                       result      = empty
    │  try_claim() succeeds
    ▼
InProgress                  unevaluated = (None, Some(task_id))
    │                       result      = empty
    │  settle(r) called
    ▼
Settled(r)                  unevaluated = (None, None)
                            result      = r  (OnceCell set)
```

Transitions are strictly forward. There is no path from Settled back to any
earlier state.

## Property 1: Evaluate-at-Most-Once (Sharing)

**Claim:** For any thunk `t`, at most one task executes `t`'s body.

**Proof sketch:**

`try_claim` acquires the `unevaluated` mutex and calls `state.take()` on the
`Option<UnevaluatedState>`. `Option::take` replaces the contents with `None`
and returns the previous value. If the option was already `None`, `take`
returns `None` and the caller does not proceed to evaluate.

Since the mutex serializes all `try_claim` calls, exactly one caller observes
`Some(state)` — the first to acquire the lock while the option is populated.
All subsequent callers observe `None` and fall through to `settled().await`,
waiting on the `notify` signal.

Therefore the body is dispatched to exactly one task. QED.

## Property 2: Monotonicity

**Claim:** Once `result` contains a value, it never changes.

**Proof sketch:**

`result` is a `tokio::sync::OnceCell`. Its `set` method succeeds exactly once;
all subsequent calls to `set` are no-ops (returning `Err`). The `settle`
method calls `self.inner.result.set(result)` and discards the return value.
`get` on a populated `OnceCell` always returns the same `&T`.

Since `OnceCell` provides no `take`, `swap`, or mutable access to the stored
value, the result is immutable once written. QED.

## Property 3: Settlement Guarantee (ThunkPanicGuard)

**Claim:** If `try_claim` succeeds for thunk `t`, then `t` is settled on all
exit paths.

**Proof sketch:**

After `try_claim` succeeds, the evaluator creates a `ThunkPanicGuard`:

```text
ThunkPanicGuard(Option<Arc<Thunk>>)
```

The guard holds `Some(Arc::clone(&thunk))`. Two exit paths exist:

1. **Normal path:** The evaluator calls `guard.settle(result)`, which calls
   `self.0.take().unwrap()` (disarming the guard) followed by
   `thunk.settle(result)`. When `Drop` runs, `self.0` is `None` — no-op.

2. **Panic path:** The guard is dropped without `settle` being called.
   `Drop::drop` observes `Some(thunk)` via `self.0.take()` and calls
   `thunk.settle(Err(EvalError::internal("thunk evaluation task panicked")))`.

In both cases, `thunk.settle` is called exactly once, which sets the
`OnceCell` and calls `notify.notify_waiters()`. Therefore all tasks
awaiting `t` via `settled()` will be unblocked. QED.

**Known limitation:** If the process aborts (stack overflow on platforms that
abort rather than unwind, allocator OOM, SIGKILL), destructors do not run
and the guarantee does not hold. These are OS-level exits outside the
application's control.

## Corollary: Bisimulation with Eager Evaluation

Properties 1 and 2 together establish that Tinct's lazy evaluator is
observationally equivalent to a call-by-value evaluator for any expression
whose value is ultimately demanded — the standard Launchbury (1993) adequacy
result. The thunk's `OnceCell` memoization ensures that sharing does not
change the observable result.

## References

- Launchbury, J. (1993). "A natural semantics for lazy evaluation." POPL 1993.
  Section 3: thunk-based lazy semantics and the sharing property this proof mirrors.
- Plotkin, G. (1977). "LCF considered as a programming language." TCS 5(3).
  Adequacy theorem for lazy PCF — the template for the bisimulation corollary.
