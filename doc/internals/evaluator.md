# CEK Machine

The CEK machine is the iterative evaluator that materializes thunks without Rust stack recursion. It implements the classic Control-Environment-Kontinuations machine (Felleisen & Friedman, 1987) adapted for lazy evaluation: instead of recursing into sub-thunks, `force_step` pushes a continuation onto a heap-allocated stack and returns the next sub-thunk to force.

All thunk materialization flows through this machine. `materialize()` (the external entry point) and `run()` (the internal loop) both bottom out here.

---

## Entry Points

```
materialize(thunk, mat_span, ctx) -> EvalResult<Value>   [public, async]
```

The external entry point for forcing a thunk. Delegates to `run()` after constructing the initial `Action`.

```
run(initial: Action, ctx) -> EvalResult<Value>            [pub(crate), async]
```

The main loop. Runs until the continuation stack is empty and a `Continue` action arrives with no stack entry to pop.

---

## The Loop

```rust
pub(crate) async fn run(initial: Action, ctx: &Arc<EvalContext>) -> EvalResult<Value> {
    let mut stack: Vec<Cont> = Vec::new();
    let mut action = initial;

    loop {
        match action {
            Action::EvalCore { expr, env_id, ctx } => {
                // Evaluate CoreExpr → thunk, then force it
                action = ...;
            }
            Action::Materialize { thunk, mat_span } => {
                action = force_step(&thunk, mat_span, &mut stack, ctx).await;
            }
            Action::Continue(result) => match stack.pop() {
                None    => return result,
                Some(k) => action = apply_cont(k, result, &mut stack).await,
            },
        }
    }
}
```

The loop is a three-way dispatch on the current `Action`. There is no other control flow.

---

## Action

`Action` is the "current instruction" of the machine. There are three variants:

```rust
pub(crate) enum Action {
    Continue(EvalResult<Value>),
    Materialize { thunk: Arc<Thunk>, mat_span: Option<Span> },
    EvalCore { expr: Arc<Spanned<CoreExpr>>, env_id: u32, ctx: Arc<EvalContext> },
}
```

| Variant | Meaning |
|---|---|
| `Continue(Ok(v))` | A value is ready. Pop a continuation and apply it, or return. |
| `Continue(Err(e))` | An error is ready. Same: pop a continuation and apply it, or return. |
| `Materialize` | Force this thunk to a value. Calls `force_step`. |
| `EvalCore` | Evaluate a `CoreExpr` to a thunk (without forcing). Used for TypeAssert and Guarded default expressions. |

---

## force_step

```rust
pub(crate) async fn force_step(
    thunk: &Arc<Thunk>,
    mat_span: Option<Span>,
    stack: &mut Vec<Cont>,
    ctx: &Arc<EvalContext>,
) -> Action
```

Inspects one thunk and produces the next `Action`. It never forces sub-thunks directly — instead it pushes continuations and returns the sub-thunk to force.

### Dispatch table

| Thunk state | Action |
|---|---|
| `Materialized(v)` | `Continue(Ok(v))` — hot path, no stack mutation |
| `Failed(e)` | `Continue(Err(e))` — enriches error with `mat_span` if new |
| `InProgress` (or Placeholder) | `Continue(Err(CircularDependency))` |
| `PendingBuiltin` | Takes state; push `BuiltinForceArg` (if force_count > 0) or inline-dispatch builtin then push `Memoize`; return `Materialize(result_thunk)` |
| `PendingCall` | Takes state; push `PendingCallDispatch`; return `Materialize(func_thunk)` |
| `Guarded` | Takes state; push `GuardedValidate`; return `Materialize(inner_thunk)` |
| `Surface` | Lowers + evals the node; push `Memoize`; return `Materialize(result_thunk)` |
| `CoreExpr` | Calls `eval_core_expr`; push `Memoize`; return `Materialize(result_thunk)` |
| `AstNodeField` | Extracts field from AST node; produces result inline |

**Blackholing:** Every `take_*` method on a thunk atomically transitions it to `InProgress` before extracting the state. If the same thunk is re-encountered during its own evaluation (cycle), `is_in_progress()` is true and `force_step` immediately returns `CircularDependency`.

---

## Continuation Stack

```rust
pub(crate) enum Cont { ... }
const _: () = assert!(std::mem::size_of::<Cont>() <= 96);
```

All large payloads are heap-allocated (`Box<...>`) so the enum fits in 96 bytes (one cache line). The continuation stack is a plain `Vec<Cont>` that grows on the heap. There is no Rust stack recursion.

### Cont variants

| Variant | Pushed by | Effect when applied |
|---|---|---|
| `Memoize` | PendingBuiltin, PendingCall, Surface, CoreExpr, Guarded (default fallback) | Cache result into parent thunk; forward value or error |
| `PendingCallDispatch` | force_step(PendingCall) | Inspect forced function value; invoke it with captured args; push `Memoize` for result |
| `GuardedValidate` | force_step(Guarded) | Type-check the forced inner value; wrap record fields if record type; memoize or fallback to default |
| `BuiltinForceArg` | force_step(PendingBuiltin) when `force_count > 0` | Accumulate forced args; when all forced, dispatch builtin; push `Memoize` |
| `TypeAssertCheck` | force_step(Surface/CoreExpr) for `TypeAssert` nodes | Validate forced value against annotation type; check `is:` predicate if present |
| `SequentialStep` | force_step, or itself | Evaluate next expression in a `Sequential` body; thread intermediate dict bindings into scope |
| `VariantUnpackForSeq` | `SequentialStep` when result is a `Variant` | Unpack Variant payload dict; add its fields to scope; continue with `SequentialStep` |
| `MatchDispatch` | force_step(PendingCall for match) | Try each match arm pattern; on match evaluate body; on exhaustion error |
| `MatchGuardCheck` | `MatchDispatch` | Check truthiness of guard expression; advance arm or fall through |
| `MatchPredicateCheck` | `MatchDispatch` | Invoke predicate on scrutinee; check `Bool(true)`; advance arm or fall through |
| `PredicateCheck` | `TypeAssertCheck` for `is:` predicates | Check truthiness of predicate result; return value or evaluate default |

---

## apply_cont

```rust
pub(crate) async fn apply_cont(
    cont: Cont,
    result: EvalResult<Value>,
    stack: &mut Vec<Cont>,
) -> Action
```

Pops and applies one continuation. Each handler receives the `EvalResult<Value>` produced by the just-forced thunk and returns the next `Action`. Most handlers either:

- Push another continuation (e.g., `Memoize`) and return `Materialize` for the next sub-thunk, or
- Return `Continue` with the final value.

---

## Memoize

`Cont::Memoize` is the most common continuation. It is the mechanism by which all thunk results are cached:

```
force_step(T):
    push Memoize { parent_thunk: T, ... }
    return Materialize(sub_thunk)

apply_cont(Memoize):
    T.set_materialized(value)     // or T.cache_failure_once(error)
    return Continue(value)
```

`set_materialized` writes to the `OnceCell` in `ThunkInner`. All other `Arc<Thunk>` handles pointing to the same thunk observe the result immediately. Subsequent `force_step` calls for the same thunk hit the `Materialized` fast path.

---

## TCO

When `PendingCallDispatch` has `tail_hint = true`, the `Memoize` continuation is skipped and the function body is returned directly as `EvalCore`:

```
Normal call:   push Memoize → push PendingCallDispatch → Materialize(func)
Tail call:     push PendingCallDispatch → Materialize(func)
               → apply_cont returns EvalCore(body) instead of pushing Memoize
```

The tail call body evaluates in the current continuation frame. The result propagates to whatever continuation was already on the stack before the call, keeping the stack O(depth of non-tail frames).

---

## EvalStackGuard

`EvalStackGuard` is a RAII helper that keeps `EvalContext.state.eval_stack` (the human-readable call trace used for cycle path reconstruction) in sync with the continuation stack.

```rust
struct EvalStackGuard { state: Arc<Mutex<EvalState>>, armed: bool }
```

| Constructor | Effect on `eval_stack` |
|---|---|
| `EvalStackGuard::push(state, entry)` | Pushes `entry`; drops pop it |
| `EvalStackGuard::inherited(state)` | No push; drop pops |
| `.disarm()` | Prevents pop on drop; transfers pop responsibility to a continuation |

The protocol:
1. `force_step` creates a `push` guard when transitioning a thunk to `InProgress`.
2. When delegating to `Memoize`, `BuiltinForceArg`, or `PendingCallDispatch`, the guard is **disarmed** — the continuation inherits pop responsibility.
3. The inheriting continuation creates an `inherited` guard, which pops when the continuation completes or errors.

This ensures `eval_stack` is always consistent with the continuation stack without manual pop calls at every exit path.

---

## Error Decoration

`attach_materialization_context` is called at every error site in the machine to enrich errors with location context:

```rust
pub(crate) fn attach_materialization_context(
    err: Box<EvalError>,
    mat_span: Option<&Span>,    // where the error was observed
    origin: Option<&str>,       // human-readable thunk label
    thunk_span: Span,           // where the thunk was defined
) -> Box<EvalError>
```

It sets `err.materialization_span` on first encounter and adds stack frames for subsequent observation sites (using deduplication to avoid repeated frames for the same span).

