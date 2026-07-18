# Thunk Subsystem

This document is for Rust contributors working in `src/value.rs`, `src/arena.rs`, and `src/eval_materialize.rs`. Tinct developers: thunks are why unused dict entries never cause errors and why recursive definitions are safe — a thunk is only evaluated when its value is demanded, and the result is cached so it evaluates at most once.

A thunk is the runtime unit of lazy evaluation — a computation that executes at most once, with the result memoized for all subsequent readers. This document describes the `Thunk` type, its state machine, the full public API, and how thunks interact with the evaluator, materialization pipeline, cycle detection, and depth limiting.

---

## Source Locations

| Concern | File |
|---|---|
| `Thunk`, `ThunkInner`, `ThunkState`, `UnevaluatedState` | `src/value.rs` |
| `ThunkId`, `ScopeArena`, `ScopeId` | `src/arena.rs` |
| `materialize()`, `run_owned()`, `force_step()`, `dispatch_state()`, `apply_cont()` | `src/eval_materialize.rs` |
| `eval_core_expr()` | `src/eval_core.rs` |
| `eval_document_exprs_with_env()`, document pipeline | `src/eval.rs` |
| Builtin function implementations | `src/builtins*.rs` |

---

## State Machine

A thunk is always in exactly one of four observable states:

```
              try_claim()
 Unevaluated ─────────────────────────→ InProgress(task_id)
      ↑                                       │         │
      │  reset(state)                         │         │
      │  [CEK re-dispatch only]               │         │
      └───────────────────────────────────────┘         │
                                               settle(Ok(v))
                                               settle(Err(e))
                                               ┌──────┴──────┐
                                               ↓             ↓
                                         Materialized     Failed
```

The internal encoding uses two independent fields in `ThunkInner`:
- `unevaluated: Mutex<(Option<UnevaluatedState>, Option<tokio::task::Id>)>`
- `result: tokio::sync::OnceCell<Result<Value, Arc<EvalError>>>`

### State Definitions

**`Unevaluated`** — `unevaluated.0 = Some(state)`, `result` empty.
The thunk holds a pending computation. No task owns it. It can be claimed for evaluation via `try_claim()`.

**`InProgress { evaluating_task: Option<tokio::task::Id> }`** — `unevaluated.0 = None`, `result` empty.
Evaluation is in progress. `Some(task_id)` means that task owns evaluation. `None` means this is a letrec placeholder — encountering it (same-task or conservative case) produces a circular dependency error.

**`Materialized(Value)`** *(terminal)* — `result = Some(Ok(v))`.
Settled with a value. Cached forever via `OnceCell`. All readers receive the cached value immediately with no locking (OnceCell fast path).

**`Failed(Arc<EvalError>)`** *(terminal)* — `result = Some(Err(e))`.
Settled with an error. Cached forever. All readers receive this error.

`Materialized` and `Failed` are distinguished from `InProgress` (placeholder) by whether `result.get()` returns `Some`. The `state()` method checks `result` first (no lock needed for OnceCell); the mutex is only acquired when result is empty.

### Invariants

1. **Monotonicity**: `Materialized` and `Failed` are terminal — no transitions exit them.
2. **Single ownership**: `try_claim()` is the sole path into `InProgress`. The mutex ensures at most one caller wins.
3. **Notification ordering**: `settle()` writes `result` via `OnceCell::set()` first, then clears `evaluating_task`, then fires `notify_waiters()`. This ordering is critical: `state()` checks `result.get()` before the mutex, so once `result` is set, readers return a terminal state even if `evaluating_task` has not yet been cleared.
4. **Evaluate-at-most-once**: the winner of `try_claim()` MUST settle the thunk. `ThunkPanicGuard` enforces this even on task panic.
5. **`reset()` is not error recovery**: the backward transition `InProgress → Unevaluated` is used only for CEK machine re-dispatch (e.g., `FnCall` resolves to a builtin requiring arg pre-materialization). Sound in the single-threaded `LocalSet`; never use for error recovery.

---

## Internal Encoding

```rust
pub struct ThunkInner {
    /// Combined: (UnevaluatedState, evaluating_task_id).
    /// Both fields transition atomically in try_claim().
    pub unevaluated: Mutex<(Option<UnevaluatedState>, Option<tokio::task::Id>)>,

    /// Terminal result. Set exactly once (OnceCell).
    pub result: tokio::sync::OnceCell<Result<Value, Arc<EvalError>>>,

    /// Resolves when result is set. Allows tasks to await settlement.
    pub notify: Arc<tokio::sync::Notify>,
}
```

State encoding:

| State | `unevaluated.0` | `unevaluated.1` | `result` |
|---|---|---|---|
| Unevaluated | `Some(state)` | `None` | empty |
| InProgress (owned) | `None` | `Some(task_id)` | empty |
| InProgress (placeholder/letrec) | `None` | `None` | empty |
| Materialized | — | — | `Some(Ok(v))` |
| Failed | — | — | `Some(Err(e))` |

For Materialized and Failed, the mutex fields are irrelevant — `state()` returns early after seeing a non-empty `result`.

---

## Thunk Struct Fields

```rust
pub struct Thunk {
    inner: ThunkInner,
    pub(crate) span: Span,         // definition-site span (for error messages)
    pub(crate) create_parent: Option<u64>,  // profiling: parent span id
    pub(crate) create_time_us: u64,         // profiling: creation timestamp
}
```

The `span` field is set at construction time and never changes. It identifies the source location where the thunk was created and is used by `attach_materialization_context` to build error stack frames.

---

## ThunkId and Arena

Thunks are not accessed by raw `Arc<Thunk>` pointer everywhere. The arena model uses `ThunkId` — a compact 8-byte handle:

```rust
pub struct ThunkId {
    pub scope_id: u32,  // index into ScopeArena.scopes
    pub slot: u32,      // ordinal position within that Scope's slots
}
```

`ScopeArena` owns all `Arc<Thunk>` values. `EvalContext.scope_arena` is an `Rc<RefCell<ScopeArena>>` shared across the evaluation session. `ThunkId` is the stable address for the lifetime of an evaluation run.

`Dict`, `Seq`, `Variant`, `Overlay`, and `Proxy` values embed `ThunkId` for their lazy members — they reference arena slots rather than holding `Arc<Thunk>` directly. This avoids `Arc` reference-count overhead on every dict field access.

---

## UnevaluatedState Variants

Each variant carries the data needed to evaluate the thunk when first forced. The `ctx: Arc<EvalContext>` field present in most variants is the evaluation session context (arena, config, cancellation token, etc.).

```rust
pub enum UnevaluatedState {
    CoreExpr {
        expr: Arc<Spanned<CoreExpr>>,
        env_id: u32,        // index into EvalContext.scope_arena
        ctx: Arc<EvalContext>,
    },
    Surface {
        node: Arc<SurfaceNode>,
        res: Arc<ResolutionTable>,
        types: Arc<TypeAnnotationTable>,
        env_id: u32,
        ctx: Arc<EvalContext>,
    },
    AstField {
        node: Arc<SurfaceNode>,
        field: &'static str,
        ctx: Arc<EvalContext>,
    },
    BuiltinCall {
        def: BuiltinDef,
        args: Vec<ThunkId>,
        named: Option<IndexMap<String, ThunkId>>,
        call_span: Span,
        caller_env_id: u32,
        ctx: Arc<EvalContext>,
    },
    FnCall {
        func: ThunkId,
        args: Vec<ThunkId>,
        named: Option<Box<IndexMap<String, ThunkId>>>,
        call_span: Span,
        caller_env_id: u32,
        ctx: Arc<EvalContext>,
        original_call: Arc<Spanned<CoreExpr>>, // for @Expr parameter quoting (macro AST injection)
    },
    Guarded {
        inner: ThunkId,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
        blame_label: Option<BlameLabel>,
        default: Option<GuardDefault>,         // (expr, env_id) for default: fallback
    },
}
```

`initial_env_id()` returns the scope id for the CEK machine's initial environment. `AstField` and `Guarded` return 0 — they don't use an expression scope (AstField extracts a struct field; Guarded forces an inner thunk that carries its own scope).

### Laziness invariant

All `UnevaluatedState` constructors accept thunk args as `ThunkId`, not pre-materialized `Value`. The computation they describe is deferred until `force_step` claims the thunk and calls `dispatch_state`. No constructor forces any sub-thunk.

---

## Constructors

All constructors take their arguments by value, produce an `Unevaluated` thunk (or `Materialized` for `Thunk::value`, or `InProgress(None)` for `Thunk::placeholder`), and never call `materialize`.

```rust
// Wrap a CoreExpr for lazy evaluation (most common path from eval_core_expr)
Thunk::core_expr(expr: Arc<Spanned<CoreExpr>>, env_id: u32, ctx: Arc<EvalContext>, span: Span) -> Self

// Wrap a SurfaceNode for lazy evaluation (pre-lowering path, used by `eval` builtin)
Thunk::surface(node: Arc<SurfaceNode>, res: Arc<ResolutionTable>, types: Arc<TypeAnnotationTable>,
               env_id: u32, ctx: Arc<EvalContext>, span: Span) -> Self

// Wrap a lazy AST field access
Thunk::ast_field(node: Arc<SurfaceNode>, field: &'static str, ctx: Arc<EvalContext>, span: Span) -> Self

// Wrap a deferred builtin call — args stay as ThunkIds
Thunk::builtin_call(def: BuiltinDef, args: Vec<ThunkId>, named: Option<IndexMap<String, ThunkId>>,
                    span: Span, caller_env_id: u32, ctx: Arc<EvalContext>) -> Self

// Wrap a deferred function call — func and args stay as ThunkIds
Thunk::fn_call(func: ThunkId, args: Vec<ThunkId>, named: IndexMap<String, ThunkId>,
               call_span: Span, caller_env_id: u32, span: Span, ctx: Arc<EvalContext>,
               original_call: Arc<Spanned<CoreExpr>>) -> Self

// Wrap a type guard around an inner thunk
Thunk::guarded(inner: ThunkId, expected: Type, field_path: Vec<String>, guard_span: Span,
               blame_label: Option<BlameLabel>, default: Option<GuardDefault>) -> Self

// Produce a Materialized thunk directly (for already-known values)
Thunk::value(v: Value, span: Span) -> Self

// Produce an InProgress(None) thunk — letrec placeholder, forces a circular dependency error
Thunk::placeholder(span: Span) -> Self
```

`Thunk::value` is the only approved fast path. It skips the `Unevaluated` state entirely and places the value directly into `result` via `OnceCell::set`. This is correct because the value is already fully evaluated — no computation needs deferring.

`Thunk::placeholder` starts in `InProgress(None)` state. Its purpose is letrec: dict entries pre-allocate placeholder slots. These are filled in before any thunk that references them is forced, so a correctly constructed dict never actually encounters a live placeholder. Encountering a placeholder during evaluation (cycle detection conservative arm) produces a `CircularDependency` error.

**Profiling fields**: `core_expr`, `surface`, `ast_field`, `builtin_call`, and `fn_call` record `create_parent` and `create_time_us` from `ctx.profiling` at construction time. `value`, `placeholder`, and `guarded` set both to zero/None — they don't represent user-visible computation that needs profiling attribution.

---

## State Observation

```rust
pub fn state(&self) -> ThunkState
```

The only way to observe thunk state. Never reads `result`, `unevaluated`, or `notify` directly. Returns a cloned snapshot — callers receive a `ThunkState` value, not a borrow into the thunk.

```rust
pub enum ThunkState {
    Unevaluated,
    InProgress { evaluating_task: Option<tokio::task::Id> },
    Materialized(Value),
    Failed(Arc<EvalError>),
}
```

`state()` checks `result.get()` first (no lock), then acquires the mutex only if result is empty. This makes the common (terminal) case lock-free.

### Convenience accessors

```rust
pub fn try_get_materialized(&self) -> Option<Value>     // Some(v) iff Materialized
pub fn get_cached_error(&self) -> Option<Box<EvalError>>// Some(e) iff Failed
pub fn is_materialized(&self) -> bool                   // true iff Materialized
pub fn definition_span(&self) -> Span                   // the span field
```

### Non-destructive introspection

These peek at the `UnevaluatedState` without claiming the thunk. Used by the CEK machine to check for fast-path conditions before forcing.

```rust
pub fn peek_builtin_def(&self) -> Option<BuiltinDef>    // Some if BuiltinCall state
pub fn is_guarded(&self) -> bool                        // true if Guarded state
pub fn is_pending_call(&self) -> bool                   // true if FnCall state
pub fn peek_surface_node(&self) -> Option<Arc<SurfaceNode>>
pub fn peek_ast_node_field(&self) -> Option<(Arc<SurfaceNode>, &'static str)>
```

---

## State Transitions

### `try_claim() -> Option<UnevaluatedState>`

Atomically transitions `Unevaluated → InProgress(current_task)`. Returns the `UnevaluatedState` to evaluate, or `None` if the thunk is already `InProgress` or terminal (the mutex `state.take()` returns `None` when `state.0` is `None`).

The mutex lock ensures atomicity: `state.take()` removes the `UnevaluatedState` and `*task_id = tokio::task::try_id()` sets the owner in a single critical section.

The caller that wins `try_claim()` MUST settle the thunk. `ThunkPanicGuard` enforces this.

### `settle(result: Result<Value, Arc<EvalError>>)`

Transitions `InProgress → Materialized` or `InProgress → Failed`. Order:
1. `OnceCell::set(result)` — makes the terminal state visible to `state()` immediately.
2. Lock mutex, clear `evaluating_task` to `None`.
3. `notify_waiters()` — unblocks tasks awaiting `settled()`.

`OnceCell::set` is idempotent: concurrent losers' `Memoize` continuations that call `settle` on an already-settled thunk are silently no-ops. The first caller wins.

**Critical ordering**: step 1 must precede step 2. If `evaluating_task` were cleared first, a concurrent `state()` call would see `(None, None)` with an empty `result` — i.e., `InProgress(None)` — triggering false cycle detection.

### `reset(state: UnevaluatedState)`

Transitions `InProgress → Unevaluated`. Used by the CEK machine when a `FnCall` is claimed and then needs to be re-queued (e.g., function body resolves to a builtin that requires pre-materialized args — the `reset` puts the state back so `dispatch_state` can re-enter the `BuiltinCall` path after claiming again). Does NOT fire `notify`. Only valid in the single-threaded `LocalSet`; a backward transition under concurrent evaluation would be unsound.

### `settled()` — async notification

```rust
pub async fn settled(&self)
```

Resolves when the thunk enters a terminal state. Uses subscribe-before-check to avoid TOCTOU:

```rust
loop {
    let notified = self.inner.notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();            // register before checking result
    if self.inner.result.get().is_some() { return; }
    notified.await;
    if self.inner.result.get().is_some() { return; }
}
```

The `enable()` call registers the waker before reading `result.get()`. This ensures that if `settle()` fires between the `enable()` call and the `await`, the notification is not lost.

---

## Panic Safety

`ThunkPanicGuard` ensures a claimed thunk always reaches a terminal state, even if the evaluation task panics:

```rust
struct ThunkPanicGuard(Option<Arc<Thunk>>);

impl ThunkPanicGuard {
    fn settle(mut self, result: Result<Value, Arc<EvalError>>) {
        let thunk = self.0.take().unwrap();
        thunk.settle(result);
        // Drop runs with self.0 == None — no-op
    }
}

impl Drop for ThunkPanicGuard {
    fn drop(&mut self) {
        if let Some(thunk) = self.0.take() {
            thunk.settle(Err(Arc::new(EvalError::internal(
                "thunk evaluation task panicked".to_string(),
                thunk.span.clone(),
            ))));
        }
    }
}
```

The guard is armed at construction (`Some(thunk)`). Calling `guard.settle(result)` disarms it (takes the `Option` to `None`) before calling `thunk.settle`. On task panic during an `await` point, Rust unwinds into `drop`, which fires the panic error path.

---

## Context Migration: `with_replaced_ctx`

```rust
pub(crate) fn with_replaced_ctx(&self, new_ctx: Arc<EvalContext>) -> Option<Arc<Thunk>>
```

Creates a new `Arc<Thunk>` identical to `self` but with the `ctx` field in its `UnevaluatedState` replaced. Returns `None` if the thunk is already `InProgress` or terminal (non-Unevaluated). Used by arena migration in `src/arena.rs` to transplant unevaluated thunks from one evaluation context to another.

---

## Materialization: The External Entry Point

```rust
pub fn materialize<'a>(
    thunk: &'a Arc<Thunk>,
    mat_span: Option<&'a Span>,
    ctx: &'a Arc<EvalContext>,
) -> Pin<Box<dyn Future<Output = EvalResult<Value>> + 'a>>
```

`materialize` is the public-facing forcing function. It runs the same protocol as `force_step` but without pushing continuations — it is the entry point used from outside the CEK machine (e.g., `eval_document_exprs_with_env`, `match_pattern`, `primitive_eq`):

```
loop:
  Materialized(v)          → return Ok(v)
  Failed(e)                → attach mat_span, return Err(e)
  InProgress(same task)    → CircularDependency error, settle(Err), return Err
  InProgress(other task)   → await settled(), loop
  Unevaluated              → try_claim(); if won, call run_owned(state, thunk, ctx).await
                             (run_owned settles the thunk as a side effect); loop
```

When `try_claim()` is won, `run_owned` is called inline in the same task — no `spawn_local`. The CEK machine runs until the thunk is settled, then `materialize` loops and reads the now-terminal state.

When `try_claim()` is lost (concurrent race), the loop reads `InProgress(other task)` and awaits `settled()`.

### `run_owned`

```rust
pub(crate) async fn run_owned(
    state: UnevaluatedState,
    thunk: &Arc<Thunk>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Value>
```

Entry point for inline thunk evaluation. Called by `materialize()` after winning `try_claim()`. Pushes an initial `Cont::Memoize` for the thunk, then calls `dispatch_state` to get the first `Action`, then runs the CEK loop until the stack is empty.

---

## CEK Machine Integration

`force_step` is the per-thunk dispatch function of the CEK machine (in `eval_materialize.rs`). It inspects one thunk and returns the next `Action`:

```rust
pub(crate) async fn force_step(
    thunk: &Arc<Thunk>,
    mat_span: Option<Span>,
    stack: &mut Vec<Cont>,
    ctx: &Arc<EvalContext>,
) -> Action
```

The dispatch table:

| Thunk state | Behavior |
|---|---|
| `Materialized(v)` | `Continue(Ok(v))` — hot path, no stack mutation |
| `Failed(e)` | `Continue(Err(e))` — enriches error with `mat_span` if new |
| `InProgress(same task or None)` | `Continue(Err(CircularDependency))` — settles thunk with error |
| `InProgress(different task)` | `await thunk.settled()`, loop |
| `Unevaluated` | Win `try_claim()` → `dispatch_state()`; lose → loop |

`dispatch_state` converts a pre-claimed `UnevaluatedState` to the initial `Action` for the CEK machine, setting up continuations:

| State variant | Continuation pushed | Action returned |
|---|---|---|
| `BuiltinCall` (force_count > 0 or Seq/Spine args) | `BuiltinForceArg` | `Materialize(arg_thunk)` |
| `BuiltinCall` (no pre-materialization needed) | `Memoize` (slow path) or none (fast path) | `Continue(Ok(v))` or `Materialize(result_thunk)` |
| `FnCall` | `PendingCallDispatch` | `Materialize(func_thunk)` |
| `Guarded` | `GuardedValidate` | `Materialize(inner_thunk)` |
| `Surface` | `Memoize` | `Materialize(result_thunk)` |
| `CoreExpr` | `Memoize` | `Materialize(result_thunk)` |
| `AstField` | none | `Continue(Ok(field_value))` (synchronous) |

### Memoize

`Cont::Memoize` is the mechanism by which thunk results are cached. When applied with a result `r`:
- calls `thunk.settle(r.map(Ok).unwrap_or_else(Err))` — writes to `OnceCell`
- returns `Continue(r)`

All other `Arc<Thunk>` handles to the same thunk see the cached result immediately on their next `state()` call.

### BuiltinForceArg and Strictness

`BuiltinDef` carries two strictness controls:
- `force_count: usize` — unconditionally pre-materialize the first N args before dispatch
- `pos_strictness: &'static [Strictness]` — per-position demand: `Id` (never force), `Seq` (force to WHNF), `Spine` (force structural layer)

The CEK machine handles both controls iteratively via `BuiltinForceArg` continuations, avoiding Rust stack growth on chains like `$- → $- → ...` where each builtin materializes its first argument.

### TCO (Tail Call Optimization)

When `PendingCallDispatch` has `tail_hint = true` (set when `Arc::strong_count(thunk) == 1`), the `Memoize` continuation for the outer thunk is skipped. The function body evaluates in the current continuation frame, propagating its result directly to whatever was already on the stack. This achieves O(1) tail-call frames.

---

## Cycle Detection

The cycle detection mechanism is the `InProgress` state. The protocol in both `force_step` and `materialize`:

```rust
ThunkState::InProgress { evaluating_task } => {
    let same = match (evaluating_task, tokio::task::try_id()) {
        (Some(e), Some(c)) => e == c,
        _                  => true,  // conservative: None = placeholder
    };
    if same {
        // CircularDependency: this task is forcing a thunk it already owns
        let cycle_path = TASK_EVAL_STACK.try_with(...).unwrap_or_default();
        let err = EvalError::circular_dependency(label, thunk.span.clone(), cycle_path);
        thunk.settle(Err(Arc::new(err.clone())));
        return error;
    }
    thunk.settled().await;   // diamond: a different task owns this thunk, wait
}
```

The conservative arm (`_ => true`) treats any case where task IDs cannot be compared as same-task. This handles letrec placeholders (`InProgress(None)` — no owner) and the window between `settle()` clearing `result` and clearing `evaluating_task` (which cannot arise in practice given the correct ordering in `settle()`).

`TASK_EVAL_STACK` is a `tokio::task_local!` `Vec<(Arc<str>, Span)>` that records the sequence of thunk names being forced in the current task. It is populated by `EvalStackGuard::push` in `dispatch_state` and popped by `EvalStackGuard::drop` (or by the inheriting continuation). This forms the cycle path included in `EvalError::circular_dependency`.

Cross-task cycles (task A waits for task B, task B waits for task A) are not detected by this mechanism — they deadlock. Detection for the distributed-eval architecture is future work.

---

## Error Decoration

```rust
pub(crate) fn attach_materialization_context(
    err: Box<EvalError>,
    mat_span: Option<&Span>,   // where the error was observed
    origin: Option<&str>,      // human-readable thunk label
    thunk_span: Span,          // where the thunk was defined
) -> Box<EvalError>
```

Called at every error site in the CEK machine. Sets `err.materialization_span` on first encounter; adds a deduplicated stack frame on subsequent observation of the same error at different spans. Builtins receive errors from inner thunks and propagate them with their own span attached as a stack frame.

---

## Letrec Dict Scoping

Dict entries in the letrec model are created as follows (in `eval_dict_core`):

1. Allocate a `ScopeArena` scope for the dict's bindings (via `alloc_root` or `alloc_child`).
2. For each dict key-value pair:
   - Evaluate the key expression to a materialized `HashableValue` (keys must be strict).
   - Reserve a slot in the scope (`reserve_slot`).
3. For each dict value:
   - Wrap the value expression as an `Unevaluated` thunk (via `Thunk::core_expr`) pointing into the shared dict scope.
   - Fill the reserved slot with this thunk (`fill_slot`).

All value thunks are created before any is forced. Each value thunk's `env_id` points to the shared dict scope, so every entry can see every other entry's thunk — including entries that appear later in source order. This is the "tie the knot" pattern that enables mutual recursion between dict entries.

Unused entries are never forced. An entry that references another entry via a cycle produces a `CircularDependency` error only when both are forced.

---

## Document Pipeline and `%`

At `---` document boundaries, the output of document N is bound to `%` in the scope of document N+1 as an `Unevaluated` thunk — specifically, the `Arc<Thunk>` returned by `eval_document_exprs_with_env` for document N is placed directly into the child scope as a slot. No materialization occurs at the boundary. Document N+1 forces `%` only if it actually uses `%`.

This is implemented in `eval.rs`'s `eval_surface_file_from_env`: the thunk for each document becomes the `last` variable that is passed (lazily) into the next document's scope.

---

## Depth Limiting

There is no `MAX_EVAL_DEPTH` counter or depth-exceeded error in the current implementation. The CEK machine is iterative — it uses a heap-allocated continuation stack (`Vec<Cont>`) instead of the Rust call stack, so there is no risk of Rust stack overflow from deeply nested tinct expressions.

The only depth-related limit is Rust's default stack size for async tasks. Deeply recursive tinct programs may exhaust the Rust stack if they create deeply nested synchronous call chains in Rust code, but this is an implementation artifact, not a semantic limit.

**If your notes reference `MAX_EVAL_DEPTH` (256) or `DepthExceeded` errors from a non-cacheable depth limit:** these were present in an earlier (non-CEK, recursive) implementation. They no longer exist.

---

## Strictness Annotations

`BuiltinDef` carries strictness metadata (Wadler & Hughes 1987):

```rust
pub enum Strictness {
    Id,    // "identity projection" — argument never forced at dispatch site
    Seq,   // force to WHNF before builtin is called
    Spine, // force structural layer (e.g., Dict spine) without element values
}

pub struct BuiltinDef {
    pub func: BuiltinFn,
    pub name: &'static str,
    pub pos_strictness: &'static [Strictness],
    pub force_count: usize,  // unconditionally force first N args before dispatch
}
```

`force_count` is checked before `pos_strictness` scanning. Both are applied iteratively by the CEK machine via `BuiltinForceArg` continuations. Builtins with `Strictness::Id` arguments receive those arguments as `ThunkId` values and choose when (and whether) to force them.

---

## Concurrency Model

The runtime uses `tokio::task::LocalSet` with a `current_thread` runtime. All evaluation runs on one OS thread. The `Mutex` in `ThunkInner.unevaluated` guards against concurrent access by multiple `tokio` tasks (not OS threads), which may interleave at `.await` points.

Multiple concurrent `materialize()` calls on the same thunk are correct:
- One wins `try_claim()` and drives evaluation via `run_owned`.
- Others enter the `InProgress(other task)` arm and `await settled()`.
- When settlement fires, all waiters loop, read `Materialized` or `Failed`, and return.

`OnceCell::set()` in `settle()` is idempotent — concurrent `Memoize` continuations that all try to settle the same thunk (diamond pattern in the dependency graph) are handled correctly: the first write wins, subsequent writes are silently discarded.

`Arc` is used for thunks that need to be shared across task boundaries. `Rc` is not used for thunks themselves (only for within-task session-local data like `EvalContext.scope_arena`).

---

## Known Issues and Layering Notes

### `origin` field

`eval_materialize.rs` accesses `thunk.origin` in several places (e.g., `thunk.origin.clone()` at lines 67, 664, 711, 1330), but the `Thunk` struct in `src/value.rs` as of the current uncommitted working tree does not include an `origin: Option<Arc<str>>` field. This is a **code inconsistency** that will produce a compile error. One of the two files (`value.rs` or `eval_materialize.rs`) is out of sync with the other due to in-progress changes (`src/value.rs` and `src/eval.rs` are both modified in the working tree). The `origin` field was used for human-readable thunk labels in error messages and profiling; its disposition needs to be resolved.

### `Environment` struct (legacy)

`src/value.rs` contains an `Environment` struct with `bindings: IndexMap<String, Arc<Thunk>>` and a parent chain `Option<Arc<RwLock<Environment>>>`. This struct is **no longer the primary evaluation scope mechanism** — `ScopeArena` / `FlatEnv` is. The `Environment` struct is retained for transitional match-dispatch code (B-515 tracks full `FlatEnv` migration). It should not be used for new code.

### Intermediate expression forcing in `eval_document_exprs_with_env`

Intermediate expressions in a document scope chain (all but the last) are eagerly materialized via `materialize(&thunk, Some(&node_span), ctx).await?` to extract their dict bindings for the scope chain. This is architecturally correct — the scope chain semantics require knowing the dict keys before the next expression can be evaluated. However, it means intermediate dicts are fully forced at evaluation time even if their values are never used by the final expression. This is a necessary strictness point for scope construction, not a laziness violation.

---

## Debug Implementation

`impl fmt::Debug for Thunk` uses `try_lock()` (non-blocking). If the mutex is contended, it displays `<locked>` rather than the actual state. This is safe — debug output is best-effort and must not block. Values are shown as their type name only (`Materialized(Int)`), never forced for display.

`impl fmt::Debug for Value` shows `Dict` keys but not values (`<thunk>`), and `Seq(...)` without forcing the spine. Neither `Debug` nor `Display` on `Value` forces any thunk — display is always state-reporting, never computing.

---

## Test Coverage

`src/value.rs` includes unit tests for:
- `Thunk::core_expr` → `state()` reports `Unevaluated`
- `Thunk::value` → `state()` reports `Materialized`
- `settle(Ok(...))` → `state()` reports `Materialized`
- `settle(Err(...))` → `state()` reports `Failed`
- `settle` is idempotent (second call is silent no-op)
- `try_claim()` succeeds on Unevaluated, transitions to `InProgress`
- `try_claim()` returns `None` on already-InProgress
- `reset()` restores `Unevaluated` from `InProgress`
- `try_get_materialized()` and `get_cached_error()` convenience accessors
