# Thunk Subsystem

A **thunk** is the unit of lazy evaluation in tinct. It encapsulates a computation that will be performed at most once, with the result memoized for all subsequent readers. This document describes the thunk state machine, its internal encoding, and the complete internal API.

---

## State Machine

A thunk is always in exactly one of four states:

```
                         try_claim()
   Unevaluated ──────────────────────────→  InProgress(task)
        ↑                                        │         │
        │  reset(state)                          │         │
        │  [arena migration only]                │         │
        └────────────────────────────────────────┘         │
                                                    settle(Ok(v))
                                                    settle(Err(e))
                                                    ┌──────┴──────┐
                                                    ↓             ↓
                                              Materialized     Failed
```

### State Definitions

**`Unevaluated`**
The thunk holds a pending computation. No task owns it. It can be claimed for evaluation.

**`InProgress { evaluating_task: Option<TaskId> }`**
Evaluation is in progress. `Some(t)` means task `t` owns the evaluation and will settle it. `None` means the slot was pre-allocated for letrec (placeholder) — forcing it produces a circular dependency error, which is correct.

**`Materialized(Value)`** *(terminal)*
Settled with a value. Cached forever. All readers receive this value immediately.

**`Failed(Arc<EvalError>)`** *(terminal)*
Settled with an error. Cached forever. All readers receive this error immediately.

### Invariants

1. **Monotonicity**: `Materialized` and `Failed` are terminal — no transitions exit them.
2. **Single ownership**: at most one task evaluates a thunk at any time (`evaluating_task` is set atomically with the state transition in `try_claim()`).
3. **Notification**: `settled()` resolves exactly once when entering a terminal state. It resolves before any reader can observe the terminal state.
4. **Evaluate-at-most-once**: `try_claim()` is the sole path into `InProgress`. It is atomic — concurrent callers race and at most one wins.
5. **Backward transition**: `reset()` (InProgress → Unevaluated) exists exclusively for arena migration. Never use for error recovery.

---

## Internal Encoding

```rust
pub struct ThunkInner {
    /// Combined: (UnevaluatedState, evaluating_task_id).
    /// Both fields transition atomically in try_claim().
    pub unevaluated: Mutex<(Option<UnevaluatedState>, Option<tokio::task::Id>)>,

    /// Terminal result. Set exactly once (OnceCell).
    pub result: OnceCell<Result<Value, Arc<EvalError>>>,

    /// Resolves when result is set. Allows tasks to await settlement.
    pub notify: Arc<tokio::sync::Notify>,
}
```

State encoding:

| State | `unevaluated.0` | `unevaluated.1` | `result` |
|---|---|---|---|
| Unevaluated | `Some(state)` | `None` | empty |
| InProgress (owned) | `None` | `Some(task_id)` | empty |
| InProgress (placeholder) | `None` | `None` | empty |
| Materialized | `None` | `None` | `Some(Ok(v))` |
| Failed | `None` | `None` | `Some(Err(e))` |

`Materialized`/`Failed` are distinguished from `InProgress(placeholder)` by whether `result` is set.

---

## API

### Constructors

Named after their content, not their usage. All non-special constructors produce `Unevaluated` thunks wrapping one `UnevaluatedState` variant:

```rust
Thunk::core_expr(expr: Arc<Spanned<CoreExpr>>, env_id: u32, ctx: Arc<EvalContext>, span: Span) -> Arc<Thunk>
Thunk::surface(node: Arc<SurfaceNode>, res: Arc<ResolutionTable>, types: Arc<TypeAnnotationTable>, env_id: u32, ctx: Arc<EvalContext>, span: Span) -> Arc<Thunk>
Thunk::ast_field(node: Arc<SurfaceNode>, field: &'static str, ctx: Arc<EvalContext>, span: Span) -> Arc<Thunk>
Thunk::builtin_call(def: BuiltinDef, args: Vec<ThunkId>, named: Option<IndexMap<String, ThunkId>>, call_span: Span, caller_env_id: u32, ctx: Arc<EvalContext>) -> Arc<Thunk>
Thunk::fn_call(func: ThunkId, args: Vec<ThunkId>, named: Option<Box<IndexMap<String, ThunkId>>>, call_span: Span, caller_env_id: u32, ctx: Arc<EvalContext>, original_call: Arc<Spanned<CoreExpr>>) -> Arc<Thunk>
Thunk::guarded(
    inner: ThunkId,
    expected: Type,
    field_path: Vec<String>,
    guard_span: Span,
    blame_label: Option<BlameLabel>,
    default: Option<GuardDefault>,
) -> Arc<Thunk>
// Consolidates new_guarded, new_guarded_with_blame, new_guarded_full — callers pass None for unused fields.

Thunk::value(v: Value, span: Span) -> Arc<Thunk>      // produces Materialized
Thunk::placeholder(span: Span) -> Arc<Thunk>           // produces InProgress(None) — letrec slot
```

### State Observation

```rust
pub fn state(&self) -> ThunkState
```

This is the **only** way to observe thunk state. No other code reads `result`, `unevaluated`, or `notify` directly for state-checking purposes. All access is through `state()`.

```rust
pub enum ThunkState {
    Unevaluated,
    InProgress { evaluating_task: Option<tokio::task::Id> },
    Materialized(Value),
    Failed(Arc<EvalError>),
}
```

There are no convenience aliases. Callers pattern-match on `state()`:

```rust
match thunk.state() {
    ThunkState::Materialized(v) => return Ok(v),
    ThunkState::Failed(e)       => return Err(Box::new((*e).clone())),
    ThunkState::InProgress { evaluating_task } => { /* cycle detection or wait */ }
    ThunkState::Unevaluated     => { /* claim and spawn */ }
}
```

### Transitions

**`try_claim() -> Option<UnevaluatedState>`** — `try_` prefix follows Rust convention for fallible operations. Atomically transitions Unevaluated → InProgress(current_task), returning the state to evaluate. Returns `None` if the thunk is already InProgress or terminal (concurrent race). The caller that wins `try_claim()` MUST settle the thunk (enforced by `ThunkPanicGuard`).

```rust
pub fn try_claim(&self) -> Option<UnevaluatedState>
```

**`settle(result: Result<Value, Arc<EvalError>>)`** — Finalizes the thunk. One function, one Result argument — matches Rust's `Ok`/`Err` idiom. Transitions InProgress → Materialized or Failed. Sets `result` first, then clears `evaluating_task`, then fires `notify`. Idempotent: `OnceCell::set()` silently discards duplicate calls (concurrent losers' Memoize continuations).

**Critical ordering:** `result` must be set before `evaluating_task` is cleared. If cleared first, a concurrent `state()` call would see `(None, None)` in the mutex with an empty `result` — i.e. `InProgress { evaluating_task: None }`. The conservative cycle detection arm (`_ => true`) treats this as same-task InProgress and raises a false cycle error. Since `state()` checks `result.get()` before the mutex, once `result` is set all readers return Materialized or Failed immediately regardless of the task_id state.

```rust
pub fn settle(&self, result: Result<Value, Arc<EvalError>>)
```

**`reset(state: UnevaluatedState)`** — **[Arena migration only — `src/arena.rs`]** Transitions InProgress → Unevaluated. Clears `evaluating_task`. Does NOT fire `notify`. Never use for error recovery; `is_cacheable()` is gone and all errors are settled via `settle(Err(...))`.

```rust
pub fn reset(&self, state: UnevaluatedState)
```

### Async Settlement Notification

```rust
pub async fn settled(&self)
```

Returns a future that resolves when the thunk enters a terminal state (Materialized or Failed). Uses subscribe-before-check to avoid the TOCTOU window:

```rust
pub async fn settled(&self) {
    loop {
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();       // register before checking
        if self.inner.result.get().is_some() { return; }
        notified.await;
        if self.inner.result.get().is_some() { return; }
    }
}
```

### Panic Safety

`ThunkPanicGuard` ensures a claimed thunk always reaches a terminal state, even if the evaluating task panics. It wraps `settle()` with mandatory disarming:

```rust
struct ThunkPanicGuard(Option<Arc<Thunk>>);

impl ThunkPanicGuard {
    fn settle(mut self, result: Result<Value, Arc<EvalError>>) {
        // Disarm by clearing the Option, then settle
        let thunk = self.0.take().unwrap();
        thunk.settle(result);
        // Drop runs with self.0 == None — no-op
    }
}

impl Drop for ThunkPanicGuard {
    fn drop(&mut self) {
        if let Some(thunk) = self.0.take() {
            // Panic path: force thunk to Failed so waiters unblock with an error
            thunk.settle(Err(Arc::new(EvalError::internal(
                "thunk evaluation task panicked".to_string(),
                thunk.span.clone(),
            ))));
        }
    }
}
```

Usage in the spawned task:
```rust
spawn_local(TASK_EVAL_STACK.scope(RefCell::new(vec![]), async move {
    let guard = ThunkPanicGuard(Some(Arc::clone(&thunk_for_task)));
    let result = run_owned(state, &thunk_for_task, &ctx).await;
    guard.settle(result.map_err(|e| Arc::new(*e)));
}));
```

---

## Concurrency Model

### Evaluation Protocol

`materialize(thunk, ctx)` is the only external entry point for forcing a thunk. It follows this protocol:

```rust
loop {
    match thunk.state() {
        ThunkState::Materialized(v)  => return Ok(v),
        ThunkState::Failed(e)        => return Err(Box::new((*e).clone())),
        ThunkState::InProgress { evaluating_task } => {
            let same = match (evaluating_task, tokio::task::try_id()) {
                (Some(e), Some(c)) => e == c,
                _                  => true,  // conservative
            };
            if same {
                // Genuine cycle: T's evaluating task IS the current task
                let cycle_path = TASK_EVAL_STACK.with(|s| s.borrow().clone());
                let err = EvalError::circular_dependency(...);
                thunk.settle(Err(Arc::new(err)));
                return Err(...);
            }
            thunk.settled().await;  // diamond: different task owns T, wait
        }
        ThunkState::Unevaluated => {
            // Try to claim. If we win, spawn evaluation and wait.
            // If we lose (concurrent race), another task claimed it — wait for them.
            if let Some(state) = thunk.try_claim() {
                let t = Arc::clone(thunk);
                let c = Arc::clone(ctx);
                spawn_local(TASK_EVAL_STACK.scope(RefCell::new(vec![]), async move {
                    let guard = ThunkPanicGuard(Some(Arc::clone(&t)));
                    let result = run_owned(state, &t, &c).await;
                    guard.settle(result.map_err(|e| Arc::new(*e)));
                }));
            }
            thunk.settled().await;
        }
    }
}
```

Multiple concurrent `materialize()` callers all spawn tasks. Only one wins `try_claim()`; losers find InProgress with a different task ID and wait. Losers' Memoize continuations call `settle(Ok(v))` on an already-settled thunk — `OnceCell` discards the duplicate silently. Correct.

### Diamond Pattern

Two evaluation paths independently demand thunk T:
- Path A: spawns task X. X wins `try_claim()`. X evaluates T. X calls `settle(Ok(v))`. `notify` fires.
- Path B: spawns task Y. Y loses `try_claim()` (X won). Y finds `InProgress { evaluating_task: X }`. X ≠ Y → waits on `settled()`. X settles T → Y resumes, reads the cached result.

### Cycle Detection

Genuine cycle: the same task that owns T's evaluation demands T again. `evaluating_task == current_task`. → `settle(Err(cycle_error))`.

Cross-task cycles (A waits for B, B waits for A) deadlock. Detection is deferred to the distributed-eval architecture.

---

## UnevaluatedState Variants

Each variant carries the data needed to evaluate a thunk. `env_id` fields are indices into `EvalContext.scope_arena`. `ctx` fields are `Arc<EvalContext>` sharing the session's scope arena.

```rust
pub enum UnevaluatedState {
    CoreExpr   { expr: Arc<Spanned<CoreExpr>>, env_id: u32, ctx: Arc<EvalContext> },
    Surface    { node: Arc<SurfaceNode>, res: Arc<ResolutionTable>,
                 types: Arc<TypeAnnotationTable>, env_id: u32, ctx: Arc<EvalContext> },
    AstField   { node: Arc<SurfaceNode>, field: &'static str, ctx: Arc<EvalContext> },
    BuiltinCall { def: BuiltinDef, args: Vec<ThunkId>, named: Option<IndexMap<String, ThunkId>>,
                  call_span: Span, caller_env_id: u32, ctx: Arc<EvalContext> },
    FnCall     { func: ThunkId, args: Vec<ThunkId>, named: Option<Box<IndexMap<String, ThunkId>>>,
                 call_span: Span, caller_env_id: u32, ctx: Arc<EvalContext>,
                 original_call: Arc<Spanned<CoreExpr>> },
    Guarded    { inner: ThunkId, expected: Type, field_path: Vec<String>,
                 guard_span: Span, blame_label: Option<BlameLabel>,
                 default: Option<GuardDefault> },
}
```

`force_step()` dispatches on the variant after `try_claim()` succeeds.

### `initial_env_id()`

Returns the evaluation scope index for the CEK machine. Called by `run_owned()` and `force_step()`.

```rust
impl UnevaluatedState {
    pub fn initial_env_id(&self) -> u32 {
        match self {
            UnevaluatedState::CoreExpr    { env_id, .. }        => *env_id,
            UnevaluatedState::Surface     { env_id, .. }        => *env_id,
            UnevaluatedState::BuiltinCall { caller_env_id, .. } => *caller_env_id,
            UnevaluatedState::FnCall      { caller_env_id, .. } => *caller_env_id,
            // AstField accesses a struct field — no expression scope needed.
            // Guarded forces an inner thunk — the inner thunk carries its own scope.
            UnevaluatedState::AstField { .. } => 0,
            UnevaluatedState::Guarded  { .. } => 0,
        }
    }
}
```

---

## Internal CEK Functions

These functions live in `eval_materialize.rs` alongside `force_step()`. They are not part of the public Thunk API.

### `dispatch_state()`

Converts a pre-claimed `UnevaluatedState` to the initial `Action` for the CEK machine — without calling `try_claim()`. Called by `run_owned()` (spawned task entry) and by `force_step()` when it wins `try_claim()` inline for a dependency thunk.

```rust
async fn dispatch_state(
    state: UnevaluatedState,
    thunk: &Arc<Thunk>,
    stack: &mut Vec<Cont>,
    ctx: &Arc<EvalContext>,
    env_id: u32,
) -> Action
```

Pushes `Cont::Memoize` for `thunk` onto `stack`, then maps each variant to the corresponding initial `Action`:

| Variant | Initial Action |
|---|---|
| `CoreExpr { expr, env_id }` | `Action::Eval { expr, env_id }` |
| `Surface { node, env_id, .. }` | `Action::EvalSurface { node, env_id }` |
| `AstField { node, field }` | `Action::EvalAstField { node, field }` |
| `BuiltinCall { def, args, .. }` | `Action::CallBuiltin { def, args, .. }` |
| `FnCall { func, args, .. }` | `Action::CallFn { func, args, .. }` |
| `Guarded { inner, .. }` | `Action::Materialize { thunk: inner }` |

### `force_step()` — dependency thunk handling

When the CEK machine encounters `Action::Materialize { dep_thunk }` for a dependency (not the task's own thunk), `force_step()` uses `try_claim()` and the full InProgress protocol. This replaces all variant-specific `take_*()` dispatch:

```rust
// force_step() inner loop for dependency thunk dep_thunk:
loop {
    match dep_thunk.state() {
        ThunkState::Materialized(v) => return Action::Return(v),
        ThunkState::Failed(e) => return Action::Err(Box::new((*e).clone())),
        ThunkState::InProgress { evaluating_task } => {
            let same = match (evaluating_task, tokio::task::try_id()) {
                (Some(e), Some(c)) => e == c,
                _                  => true,  // conservative: None = placeholder/letrec
            };
            if same {
                let cycle_path = TASK_EVAL_STACK.with(|s| s.borrow().clone());
                let err = EvalError::circular_dependency(..., cycle_path);
                dep_thunk.settle(Err(Arc::new(err.clone())));
                return Action::Err(Box::new(err));
            }
            dep_thunk.settled().await;  // diamond: different task owns dep_thunk, wait
            // loop and re-read state after notification
        }
        ThunkState::Unevaluated => {
            if let Some(state) = dep_thunk.try_claim() {
                // Won the race — evaluate dep_thunk INLINE (same task, no spawn)
                let env_id = state.initial_env_id();
                return dispatch_state(state, dep_thunk, stack, ctx, env_id).await;
            }
            // Lost the race (another task claimed between state() and try_claim()) — loop
        }
    }
}
```

The diff-task InProgress case (diamond) now waits on `settled()` instead of reporting a false cycle. Inline evaluation (won `try_claim()`) reuses the current task and stack — no spawn.
