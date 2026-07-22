# Evaluator Internals

This document is for Rust contributors working across `src/eval*.rs`. Tinct developers wanting to understand *why* certain runtime behaviors exist — why unused entries never fire, why tail calls don't stack-overflow, why cross-document `%` is lazy — will find the relevant explanations in the [Strictness Points](#strictness-points) and [Document Pipeline](#document-pipeline-and-loader) sections.

The tinct evaluator is a lazy, demand-driven evaluator built around an iterative CEK machine. All evaluation is asynchronous (tokio `LocalSet`, single-threaded). There is no Rust stack recursion for evaluation depth — all control flow is on a heap-allocated continuation stack.

---

## Module Structure

The evaluator is split across six Rust files. `eval.rs` is the façade that re-exports from the others.

| File | Responsibility |
|---|---|
| `src/eval.rs` | Top-level entry points (`eval_surface_file`, `materialize`); `EvalContext`; document pipeline; `TypeContextData`; match/type helpers |
| `src/eval_core.rs` | `eval_core_expr` — CoreExpr dispatch table; wraps AST nodes as thunks without forcing |
| `src/eval_dict.rs` | `eval_dict_core` — dict construction with letrec/FlatEnv scoping |
| `src/eval_call.rs` | `eval_call_core`, `invoke_function`, `bind_args_thunks` — function application and argument binding |
| `src/eval_materialize.rs` | `materialize`, `run`, `force_step`, `apply_cont` — the CEK machine; all continuation types; `EvalStackGuard`; profiling |
| `src/eval_access.rs` | Field/key access helpers used by builtins and the evaluator |

`eval.rs` re-exports `eval_call_core`, `invoke_function`, and `CallContext` from `eval_call.rs`. `eval_dict_core` and `core_expr_is_static_key` are re-exported into `eval.rs` via `#[path = "eval_dict.rs"] mod eval_dict_mod`.

The public API is re-exported from `src/lib.rs`:

```rust
// Evaluation entry points
pub use eval::{eval_surface_file, eval_surface_file_with_input, invoke_function, materialize,
               CallContext, EvalConfig, EvalContext, TypeContextData};

// Thunk/value types
pub use value::{string_val, ChannelInner, ClockCapInner, DirPerms, HashableValue,
                NetCapEntry, Thunk, ThunkId, Value};
```

---

## EvalContext

`EvalContext` is the immutable session handle threaded through all evaluation functions as `&Arc<EvalContext>`. It is never mutated after construction (fields that need mutation use interior mutability).

```rust
pub struct EvalContext {
    pub config: Arc<EvalConfig>,         // base_dir, no_fs, require_integrity, macro_injects_map
    pub(crate) scope_arena: Rc<RefCell<ScopeArena>>,  // all thunks and scopes
    pub env_allowed: Option<HashSet<String>>,          // env-var allowlist
    pub blame_map: Mutex<HashMap<ThunkId, String>>,    // pipeline blame for %
    pub boundary_guards: RwLock<HashMap<Span, Type>>,  // type-checker → evaluator guards
    pub do_infer_resolutions: RwLock<HashMap<String, String>>,  // [do] monad inference
    pub libdir_dir: Mutex<Option<Arc<cap_std::fs::Dir>>>,
    pub cancel: tokio_util::sync::CancellationToken,
    pub task_registry: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    pub profiling: Option<Arc<Mutex<ProfilingCollector>>>,
    pub tycon_env: OnceLock<Arc<TyConEnv>>,            // from type inference
    pub type_context: Arc<Mutex<Option<TypeContextData>>>,  // unified type-checker state
    pub scope_frames: Option<Arc<Vec<IndexMap<String, u32>>>>,  // resolver frames for lower()
}
```

**Key design decisions:**

- `scope_arena` is `Rc<RefCell<>>` (not `Arc<Mutex<>>`). The evaluator is strictly single-threaded (`LocalSet`). `Rc`-based sharing is correct and avoids locking overhead.
- Child contexts (`with_base_dir`, `with_cancel_token`, `with_explicit_cancel`, `with_timeout_ms`) share `scope_arena` via `Rc::clone`. ThunkIds created in a parent context remain valid in child contexts — the arena is the single source of truth.
- `type_context` is `Arc<Mutex<Option<TypeContextData>>>`, shared across all child contexts, because `builtin-typecheck-doc` is a side-effecting operation that accumulates type knowledge (TypeScheme, TyConDef registrations) across files. All contexts must see the same state.
- `scope_frames` carries the resolver's scope frames from the init (loader) program. It is threaded into `lower()` at eval time so that typeclass method dispatch names resolve to correct de Bruijn coordinates. Set by `with_scope_frames()` after `resolve_surface_program` runs. `None` in bootstrap contexts and tests.
- `boundary_guards` carries type-inference boundary annotations (`Span → Type`) so that `Unknown`-typed expressions crossing into concrete-typed contexts get wrapped in runtime `Guarded` thunks.

**Constructors:**

| Constructor | When to use |
|---|---|
| `EvalContext::new(base_dir, no_fs)` | Standard user evaluation |
| `EvalContext::new_empty(base_dir, no_fs)` | Bootstrap (loader.llt evaluation), tests, re-entrant macro expansion |
| `EvalContext::new_with_options(base_dir, no_fs, require_integrity, env_allowed)` | Full option control |
| `ctx.with_base_dir(dir)` | `$include` — different filesystem root, shared arena |
| `ctx.with_cancel_token()` | `with-cancel` — child cancellation scope |
| `ctx.with_explicit_cancel(token)` | `non-cancellable` / `with-context` — explicit token |
| `ctx.with_timeout_ms(ms)` | `with-timeout` / `with-deadline` — auto-cancel after delay |

All constructors pre-populate `scope_arena` with a root scope at `ScopeId(0)` containing one `Value::Builtin` thunk per entry in `core_builtins()`. Slot order in the root scope must stay in sync with `build_core_env()`'s `insert_slot_name_only` order — both call `core_builtins()` directly (a deterministic `Vec`) so the ordering is guaranteed.

---

## Scope Arena

`ScopeArena` (in `src/arena.rs`) is the authoritative store for all thunks and lexical scope frames. It is a flat `Vec<Scope>` indexed by `ScopeId(u32)`.

```rust
pub struct ScopeArena { pub(crate) scopes: Vec<Scope> }

pub struct ThunkId { pub scope_id: u32, pub slot: u32 }  // 8 bytes, Copy
pub struct ScopeId(pub u32);                               // 4 bytes, Copy
```

A `Scope` is one lexical scope frame. Variables are addressed by `(level, slot)` de Bruijn coordinates where `level = parent-chain hops from current scope` and `slot = ordinal position within that Scope`. The `ScopeArena` maintains a parent chain for O(1) display-chain lookup via `walk_parent_chain`.

**Key operations:**

- `alloc_root(slot_count)` — allocate a root scope (no parent)
- `alloc_child(parent_id, slot_count)` — allocate a child scope (inherits display chain)
- `push_slot(env_id, name, thunk)` — append a named slot
- `reserve_slot(env_id, name)` — reserve a slot for letrec phase 1 (None placeholder)
- `fill_slot(env_id, slot, src_thunk_id)` — fill a reserved slot (letrec phase 2)

`ThunkId` is a stable arena address: `scope_id` indexes into `ScopeArena.scopes` and `slot` is the ordinal position within that scope. ThunkIds are valid for the lifetime of the `EvalContext` (arenas are never compacted).

`EvalContext::alloc_thunk(env_id, thunk)` appends a thunk to the given scope and returns its ThunkId. `EvalContext::get_thunk(thunk_id)` resolves a ThunkId to `Arc<Thunk>`.

---

## Thunk Lifecycle

### ThunkInner and ThunkState

Each `Thunk` is a lazy cell with two-field separation:

```rust
pub struct Thunk {
    inner: ThunkInner,
    pub(crate) span: Span,       // definition-site source span
    pub(crate) origin: Option<Arc<str>>,  // human-readable label (e.g. "builtin-map")
    pub(crate) create_parent: Option<u64>,  // profiling parent span id
    pub(crate) create_time_us: u64,         // profiling creation timestamp
}

pub struct ThunkInner {
    pub unevaluated: Mutex<(Option<UnevaluatedState>, Option<tokio::task::Id>)>,
    pub result: tokio::sync::OnceCell<Result<Value, Arc<EvalError>>>,
    pub notify: Arc<tokio::sync::Notify>,
}
```

`ThunkState` (returned by `thunk.state()`) is derived from these two fields:

```
result set?   unevaluated state?    ThunkState
──────────────────────────────────────────────
No            Some(state)           Unevaluated
No            None (claimed)        InProgress { evaluating_task }
Yes           (irrelevant)          Materialized(v) or Failed(e)
```

**State transitions:**

```
Unevaluated ──try_claim()──► InProgress ──settle(Ok)──► Materialized
                                        ──settle(Err)──► Failed
```

`try_claim()` atomically takes the `UnevaluatedState` from the mutex and records the current `tokio::task::Id`. This is the "blackholing" step: any re-entry into the same thunk from the same task hits `InProgress` and produces a `CircularDependency` error. Re-entry from a different task causes the task to `await thunk.settled()` and then re-read the result.

**Cycle detection:** `force_step` checks `ThunkState::InProgress { evaluating_task }`. If `evaluating_task == current_task_id`, it is a genuine cycle: the `CircularDependency` error is immediately settled into the thunk (so subsequent accesses see `Failed`) and returned. The `TASK_EVAL_STACK` task-local provides the cycle path for the error message.

**Memoization:** `thunk.settle(result)` writes to the `OnceCell` and notifies waiters. All subsequent `state()` calls see `Materialized` or `Failed` directly — no re-evaluation.

### UnevaluatedState Variants

```rust
pub enum UnevaluatedState {
    AstField { node, field, ctx },                    // lazy AST field access
    CoreExpr { expr, env_id, ctx },                   // lowered CoreExpr body
    BuiltinCall { def, args, named, call_span, caller_env_id, ctx },
    FnCall { func, args, named, call_span, caller_env_id, ctx, original_call },
    Guarded { inner, expected, field_path, guard_span, blame_label, default },
}
```

`AstField` thunks perform a single synchronous field extraction from a `SurfaceNode` (for `ast-of` / `quote` field lazy access). They never recurse into the CEK machine.

`Guarded` thunks wrap an inner thunk with a runtime type check. When forced, `force_step` materializes the inner thunk and then validates the result against the `expected` type via `GuardedValidate`. If the type check fails and a `default` expression is present, the default is evaluated as a fallback.

### Thunk Constructors

| Constructor | UnevaluatedState created |
|---|---|
| `Thunk::value(v, span)` | Pre-materialized — `OnceCell` set immediately |
| `Thunk::core_expr(expr, env_id, ctx, span)` | `CoreExpr` |
| `Thunk::ast_field(node, field, ctx, span)` | `AstField` |
| `Thunk::builtin_call(def, args, named, span, caller_env_id, ctx)` | `BuiltinCall` |
| `Thunk::fn_call(func, args, named, call_span, caller_env_id, ...)` | `FnCall` |
| `Thunk::placeholder(span)` | Unevaluated with empty state (for letrec pre-allocation) |

`Thunk::value` is the only constructor that does not go through the CEK machine. It is the approved fast path for constant literals and pre-computed values. All other constructors create `UnevaluatedState` entries that are evaluated on first access.

---

## Entry Points

### eval_surface_file

```rust
pub async fn eval_surface_file(
    program: &SurfaceProgram,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>>
```

Top-level entry for evaluating a `SurfaceProgram` (one or more `---`-separated documents). Returns the last document's result thunk lazily — the thunk is **not** forced.

**Preconditions:**
1. `desugar::desugar_surface_program` has run on the program (desugars `$_`, `|` pipeline, `>>`, etc.)
2. `resolve::resolve_surface_program` has run and written de Bruijn coordinates inline to AST nodes
3. If typechecking was skipped, `TypeAssert` nodes carry `Type::Unknown` (accepts all values)

Evaluates documents sequentially. Each document's result thunk becomes the `%` input for the next document. `%` is passed lazily — no materialization at `---` boundaries.

`eval_surface_file_with_input(program, ctx, Some(thunk))` binds the provided thunk as `%` in a child scope visible to all documents — used by the formatter.

### eval_document_exprs_with_env

```rust
pub(crate) async fn eval_document_exprs_with_env(
    expr_nodes: &[Arc<SurfaceNode>],
    ctx: &Arc<EvalContext>,
    parent_env_id: Option<u32>,
) -> EvalResult<(Arc<Thunk>, u32)>
```

Evaluates a sequence of surface expression nodes as a scope chain. Each expression is lowered to `CoreExpr` via `lower()` before evaluation.

- Intermediate expressions (all but the last) are materialized eagerly to extract their dict bindings. The first newly allocated FlatEnv for each intermediate dict becomes the new `current_env_id`, chaining the display vectors.
- The last expression is returned lazily as a thunk.
- Returns `(last_thunk, scope_id)` where `scope_id` is the root FlatEnv allocated for this document.

**Scope chaining invariant:** `current_env_id` is always set to the ROOT scope of each intermediate dict (the first FlatEnv allocated at `arena_len_before`), not the leaf. This keeps display chain depth equal to Env chain depth, so de Bruijn levels from the resolver map correctly.

### materialize

```rust
pub fn materialize<'a>(
    thunk: &'a Arc<Thunk>,
    mat_span: Option<&'a Span>,
    ctx: &'a Arc<EvalContext>,
) -> Pin<Box<dyn Future<Output = EvalResult<Value>> + 'a>>
```

The external entry point for forcing a thunk to a `Value`. This is the shallow-forcing interface — it forces the top thunk but not any thunks stored inside the resulting `Value::Dict`, `Value::Seq`, etc.

`materialize` loops until the thunk is settled:
- `Materialized(v)` → return `Ok(v)`
- `Failed(e)` → return `Err(e)` (enriched with `mat_span`)
- `InProgress { same_task }` → `CircularDependency` error, cached in thunk
- `InProgress { different_task }` → `await thunk.settled()`, then re-read
- `Unevaluated` → claim and dispatch via `run_owned` (which runs the CEK machine)

`mat_span` is the source span where the value is being consumed (e.g., the `+` operator site). It is attached to errors as `materialization_span` on first access and as a stack frame on subsequent accesses (deduplication prevents repeated frames for the same span).

---

## CEK Machine

The CEK machine (in `src/eval_materialize.rs`) is the iterative force loop. It eliminates Rust stack recursion for all thunk forcing. Named after Felleisen & Friedman 1987 (Control-Environment-Kontinuations).

### Entry: run / run_owned

```rust
pub(crate) async fn run(initial: Action, ctx: &Arc<EvalContext>) -> EvalResult<Value>
```

The main loop: a three-way dispatch on the current `Action`.

```rust
pub(crate) enum Action {
    Continue(EvalResult<Value>),
    Materialize { thunk: Arc<Thunk>, mat_span: Option<Span> },
    EvalCore { expr: Arc<Spanned<CoreExpr>>, env_id: u32, ctx: Arc<EvalContext> },
}
```

| Action | Effect |
|---|---|
| `Continue(Ok(v))` | Pop top continuation and apply, or return `v` if stack empty |
| `Continue(Err(e))` | Same — error propagates through the continuation stack |
| `Materialize { thunk }` | Call `force_step` to dispatch the thunk |
| `EvalCore { expr, env_id }` | Call `eval_core_expr` to wrap the expr as a thunk, then force it |

`EvalCore` is used for TypeAssert and Guarded default expression fallbacks — cases where a `CoreExpr` must be evaluated without first creating an intermediate thunk.

### force_step

```rust
pub(crate) async fn force_step(
    thunk: &Arc<Thunk>,
    mat_span: Option<Span>,
    stack: &mut Vec<Cont>,
    ctx: &Arc<EvalContext>,
) -> Action
```

Inspects one thunk's current state and produces the next `Action`. Never forces sub-thunks directly — pushes continuations instead.

**Dispatch table:**

| State | Action produced |
|---|---|
| `Materialized(v)` | `Continue(Ok(v))` — hot path, no stack mutation |
| `Failed(e)` | `Continue(Err(e))` — enriches error with `mat_span` |
| `InProgress { same_task }` | `Continue(Err(CircularDependency))` — cached in thunk |
| `InProgress { different_task }` | Await `thunk.settled()`, re-read (async cooperative wait) |
| `Unevaluated` | `try_claim()` → `dispatch_state()` → see below |

**dispatch_state dispatches by UnevaluatedState:**

| UnevaluatedState | Continuations pushed | Next Action |
|---|---|---|
| `BuiltinCall` | `BuiltinForceArg` (if force_count > 0) or `Memoize` | `Materialize(result_thunk)` |
| `FnCall` | `PendingCallDispatch` | `Materialize(func_thunk)` |
| `Guarded` | `GuardedValidate` | `Materialize(inner_thunk)` |
| `CoreExpr` | if TypeAssert: `Memoize + TypeAssertCheck`; if Sequential: `Memoize + LetrecChainStep`; if Match: `Memoize + MatchDispatch`; else `Memoize` | `Materialize(result_thunk)` or `EvalCore` |
| `AstField` | (none) | `Continue(Ok(value))` — synchronous |

**TypeAssert and Sequential/Match are handled inline** in `dispatch_state` (not via eval_core_expr round-trip) to avoid creating redundant CoreExpr thunks that would loop back into the same branch.

### Continuation Stack

```rust
pub(crate) enum Cont { ... }
const _: () = assert!(std::mem::size_of::<Cont>() <= 96);
```

All large payloads are boxed so `Cont` fits in 96 bytes (one cache line). The stack is a plain `Vec<Cont>`.

| Cont variant | Pushed by | Effect when applied |
|---|---|---|
| `Memoize` | All deferred paths | Cache result into parent thunk via `thunk.settle()`; forward value or error |
| `PendingCallDispatch` | `FnCall` dispatch | Inspect forced function; invoke with args; push `Memoize` for result |
| `GuardedValidate` | `Guarded` dispatch | Type-check forced inner value; wrap record fields if record type; memoize or fallback to default |
| `BuiltinForceArg` | `BuiltinCall` dispatch when `force_count > 0` or `Strictness::Seq` arg | Pre-materialize argument; when all forced args done, dispatch builtin |
| `TypeAssertCheck` | TypeAssert inline path | Validate forced value against annotation type; check `is:` predicate if present |
| `LetrecChainStep` | `Sequential` inline path | Evaluate next expression in body; thread dict bindings into scope |
| `VariantUnpackForLetrecChain` | `LetrecChainStep` when result is `Variant` | Unpack payload dict; add fields to scope; continue LetrecChainStep |
| `MatchDispatch` | `Match` inline path | Try each arm pattern; on match evaluate body; on exhaustion error |
| `MatchGuardCheck` | `MatchDispatch` | Check guard expression truthiness; advance arm or fall through |
| `MatchPredicateCheck` | `MatchDispatch` | Invoke predicate on scrutinee; check `Bool(true)` |
| `PredicateCheck` | `TypeAssertCheck` for `is:` predicates | Check predicate result; return value or evaluate default |

### Memoize

`Cont::Memoize` is the most common continuation. It writes the CEK machine's result back to the parent thunk:

```
force_step(T):
    push Memoize { parent_thunk: T }
    return Materialize(sub_thunk)

apply_cont(Memoize):
    T.settle(result)         // OnceCell write, notifies waiters
    return Continue(result)
```

All `Arc<Thunk>` handles pointing to the same thunk see the result immediately after `settle`. Subsequent `force_step` calls for the same thunk hit `Materialized` directly.

### Builtin Strictness

Builtins declare argument strictness via `BuiltinDef`:

```rust
pub struct BuiltinDef {
    pub func: BuiltinFn,
    pub name: &'static str,
    pub pos_strictness: &'static [Strictness],  // per-position W&H strictness
    pub force_count: usize,                      // unconditional pre-force count
}
```

`force_count` arguments are pre-materialized unconditionally before builtin dispatch. `pos_strictness` is scanned for the first `Strictness::Seq` or `Strictness::Spine` position that has an un-materialized thunk, and that argument is pre-materialized iteratively via `BuiltinForceArg`.

This keeps force-chains off the Rust stack: `$-` → materialize → `$-` → ... stays on the continuation stack.

### TCO

When `PendingCallDispatch.tail_hint == true`, the `Memoize` continuation is skipped and `apply_cont` returns `EvalCore(body)` directly. `tail_hint` is set when `Arc::strong_count(thunk) == 1` at dispatch time, meaning nobody else holds a reference to the result thunk — memoization is pointless. This achieves O(1) tail-call elimination.

### EvalStackGuard

`EvalStackGuard` is a RAII guard that maintains `TASK_EVAL_STACK` (a task-local `Vec<(Arc<str>, Span)>`) in sync with the continuation stack. It is used to reconstruct the cycle path when `CircularDependency` is detected.

```rust
EvalStackGuard::push(entry)      // push and arm; drop pops
EvalStackGuard::inherited()      // no push; drop pops (continuation inherits pop)
guard.disarm()                   // prevent pop on drop; continuation takes responsibility
```

### Error Decoration

`attach_materialization_context` (in `eval_materialize.rs`) enriches errors with location context. It is called at every error site in the CEK machine:

- Sets `err.materialization_span` on first access (where the value was consumed)
- Adds stack frames for subsequent observation sites (deduplication prevents repeated frames)
- Adds an origin frame when `thunk.origin` is set (e.g., builtin name)

---

## CoreExpr Evaluation

`eval_core_expr` (in `src/eval_core.rs`) maps a `Spanned<CoreExpr>` to an `Arc<Thunk>`. It **wraps without forcing**: no materialization happens here unless a sub-expression must be evaluated to produce a thunk (e.g., dict keys, variant payloads).

```rust
pub(crate) fn eval_core_expr<'a>(
    expr: &'a Spanned<CoreExpr>,
    env_id: u32,
    ctx: &'a Arc<EvalContext>,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + 'a>>
```

**Dispatch table:**

| CoreExpr variant | Result |
|---|---|
| `Int`, `Float`, `Str`, `U64` | `Thunk::value(...)` — pre-materialized literal |
| `Var { level, slot }` | Walk parent chain `level` hops; return thunk at `slot` |
| `Dict(entries)` | `eval_dict_core(entries, env_id, ctx)` |
| `Call { func, args, named_args }` | `eval_call_core(...)` → `Thunk::fn_call` (PendingCall thunk) |
| `Fn { params, body }` | `Thunk::value(Value::Function { ... })` — materialized at definition |
| `Sequential(_)` | `Thunk::core_expr(...)` — CEK handles via LetrecChainStep |
| `Match { .. }` | `Thunk::core_expr(...)` — CEK handles via MatchDispatch |
| `TypeAssert { .. }` | `Thunk::core_expr(...)` — CEK handles via TypeAssertCheck |
| `Variant { tag, payload: None }` | `Thunk::value(Value::Variant { payload: None })` |
| `Variant { tag, payload: Some(e) }` | Eval + materialize payload, then `Thunk::value(...)` |
| `UnitVariant { tycon, ctor }` | `Thunk::value(Value::Variant { payload: None })` |
| `Quote(inner)` | `eval_quote_walk` — walk AST, handle unquotes |
| `LetDecl { bindings }` | Return `Value::Dict` of `(name → lazy-thunk)` pairs |

**Variable lookup** is O(arena chain depth). `arena.walk_parent_chain(env_id, level)` walks the parent pointer chain `level` hops from `env_id`, then indexes `slot` in that scope. A miss is a compiler invariant violation (logged as `EvalError::internal`).

**Function definition** is eager: `CoreExpr::Fn` produces a `Value::Function` with the body stored as `Arc<Spanned<CoreExpr>>` and `closure_env_id` set to the current `env_id`. The function annotation's extra fields are evaluated at definition time via `extract_fn_annotation_extra`.

**Variant payload** is the one case where `eval_core_expr` calls `materialize` — the payload expression is evaluated and materialized immediately so that `Value::Variant.payload` can be stored as a `ThunkId` pointing to the materialized payload dict.

---

## Dict Construction: Letrec Scoping

`eval_dict_core` (in `src/eval_dict.rs`) implements letrec-scoped dict construction.

```rust
pub(crate) async fn eval_dict_core(
    entries: &[Spanned<CoreEntry>],
    parent_env_id: u32,
    ctx: &Arc<EvalContext>,
    dict_span: &Span,
) -> EvalResult<Arc<Thunk>>
```

**Semantics:**

1. A child `FlatEnv` is allocated unconditionally (`alloc_child(parent_env_id, entries.len())`). This happens even for literal-only dicts because the resolver assigns `(level, slot)` coordinates to ALL entries; skipping allocation would shorten the display chain and cause wrong-scope lookups from nested scopes.

2. Keys are evaluated in `parent_env_id` (Key Isolation Invariant) — they cannot reference sibling dict entries.

3. Values are wrapped lazily:
   - Literal values (`Int`, `Float`, `Str`, `U64`) get `Thunk::value(...)` directly — pre-materialized fast path (analogous to Nix's `maybeThunk`).
   - Non-literal values get `Thunk::core_expr(expr, dict_env_id, ctx)` — they evaluate in the child `dict_env_id` and can reference sibling entries via de Bruijn slots (enabling mutual recursion).

4. Static keys (bare-word strings or annotated Var keys) are registered in the FlatEnv via `letrec_slots`. This is the "tie the knot" step: the resolver pre-assigned slot indices; the evaluator fills those exact slots so that VarRef lookups from sibling values find each other.

5. Dynamic keys (computed at runtime) are not registered in the FlatEnv. They do not participate in letrec scoping.

**Invariant:** `core_expr_is_static_key` must return the same set as `resolve.rs`'s `surface_dict_static_keys`. Both must agree on which entries get slot assignments.

---

## Function Application

### eval_call_core

```rust
pub(crate) async fn eval_call_core(
    func_expr: &Spanned<CoreExpr>,
    args: &[Arc<Spanned<CoreExpr>>],
    named_args: &[Spanned<CoreNamedArg>],
    caller_env_id: u32,
    ctx: &Arc<EvalContext>,
    call_span: &Span,
    original_call: Arc<Spanned<CoreExpr>>,
) -> EvalResult<Arc<Thunk>>
```

Creates a `PendingCall` (`FnCall`) thunk without forcing anything. All arguments are wrapped as `Thunk::core_expr` thunks — lazy, not materialized. The function expression itself is also wrapped lazily via `eval_core_expr`. Returns a `Thunk::fn_call` (i.e., `UnevaluatedState::FnCall`).

Dispatch (materializing the function value and dispatching to `Value::Function` or `Value::Builtin`) happens in the CEK machine via `PendingCallDispatch`.

### invoke_function

```rust
pub async fn invoke_function(ctx: &CallContext<'_>) -> EvalResult<Arc<Thunk>>
```

Binds positional and named arguments to function parameters, then wraps the body as an `UnevaluatedState::CoreExpr` thunk. The body is **not** forced — it is returned as a lazy thunk.

`invoke_function_tco` is the tail-call variant: it returns `(body_expr, call_env_id)` directly without creating a thunk, allowing the caller to reuse the current continuation frame.

### bind_args_thunks

`bind_args_thunks` handles the full parameter binding protocol:

1. **BIND-SPLIT**: Classify params into `regular_params`, `typed_variadic_params` (annotated `...xs@T`), and `rest_param` (unannotated `...xs`).
2. **BIND-ARITY**: Check that every required parameter has coverage (positional or named). Raise `MissingRequiredParam` if not.
3. **BIND-NAMED-VALIDATE**: Reject named args that target positionally-bound params (C-NO-OVERLAP) or unknown params (C-NAMED-VALID). System-injected names (containing `∷`) bypass validation.
4. Allocate a child FlatEnv for the call frame (`alloc_child(closure_env_id, params.len())`).
5. **Phase 1**: `reserve_slot` for each param in declaration order (establishes correct de Bruijn slot indices).
6. **BIND-POSITIONAL + BIND-NAMED**: Fill each regular param slot from positional args, named args, or default expressions. Defaults are wrapped as `Thunk::core_expr` (lazy evaluation at access time).
7. **BIND-VARIADIC**: Route excess positional args and unmatched named args into typed buckets or the rest bucket. **Typed variadic params are a strictness point**: materializing is necessary to inspect the runtime type for bucket dispatch. Untyped rest (`...args`) is lazy — args flow in without forcing.

**CoreParam.resolved_type and slot:** The type checker populates `Param.resolved_type` and `Param.slot` (threaded via `CoreParam` fields). `resolved_type` is used at BIND-SPLIT time to classify variadic params into typed buckets vs. the untyped rest. `slot` is the resolver-assigned de Bruijn slot index used by `fill_slot` to place the arg thunk at the correct arena position. These fields were added in S-938 (commit `T-1691/B-533/B-534`) to enable the type checker to communicate type information to the evaluator without a runtime annotation lookup.

---

## Document Pipeline and Loader

### Document Pipeline

Multiple `---`-separated documents are evaluated in sequence by `eval_surface_file_from_env`. The last thunk of each document is passed as the `%` input for the next document, lazily — no materialization at `---` boundaries.

The tinct-side `builtin-eval` (inside `loader.llt`) is responsible for threading the per-document scope-id forward via `builtin-scope-new`, making each document's scope a child of the previous.

### run_loader_pipeline

`run_loader_pipeline` (in `src/lib.rs`) is the shared evaluation core used by the CLI. It orchestrates:

1. **Parse** the init program (`loader.llt`).
2. **Desugar** the parsed program.
3. **Resolve** the program to de Bruijn coordinates, starting from the root FlatEnv's slot names. Captures scope frames and threads them into `eval_ctx_with_frames` via `with_scope_frames()`.
4. **Type-stage pass**: evaluate documents with `stage: "type"` header first. This materializes type-stage values (TypeNode leaves → `TypeStageEntry::Resolved`, functions → `TypeStageEntry::Function`). The result is passed to the type checker as `type_stage_map`.
5. **Typecheck** the full program via `builtin-typecheck-doc` side effects.
6. **Evaluate** via `eval_surface_file`.

The scope frames from step 3 are essential for production path typeclass method dispatch (B-513). Without them, `lower()` at eval time cannot resolve `call_dispatch`-mangled instance binding names to correct de Bruijn coordinates.

---

## Type Checker Integration

### TypeContextData

```rust
pub struct TypeContextData {
    pub type_stage_scope_id: Option<u32>,   // scope for type-stage function thunks
    pub inference_env: Arc<RwLock<Env>>,    // accumulated HM inference environment
    pub tycon_env: HashMap<String, Arc<TyConDef>>,  // accumulated TyCon definitions
    pub type_errors: Vec<TypeError>,        // accumulated type errors
}
```

Wrapped in `Arc<Mutex<Option<TypeContextData>>>` on `EvalContext`. `None` until initialized by `builtin-make-type-ctx`. Shared across all child contexts so that `builtin-typecheck-doc` side effects are visible everywhere.

### boundary_guards

`EvalContext.boundary_guards` (`RwLock<HashMap<Span, Type>>`) carries type-inference boundary annotations. When a `Span` in the map is encountered at thunk creation time in `eval_core_expr`, the thunk is wrapped in a `Guarded` thunk with the expected type. This is how `Unknown`-typed expressions crossing into concrete-typed contexts get runtime checks without requiring explicit `[@Type ...]` annotations in user code.

### CoreParam.resolved_type and slot

Type checker → evaluator data threading (S-938):

- `SurfaceParam.resolved_annotation_type` (a `TypeAnnotation` OnceLock) is populated by `infer_fn_push_cont` during type inference.
- `lower.rs` reads `resolved_annotation_type` and writes it into `CoreParam.resolved_type` when lowering `CoreExpr::Fn` params.
- `eval_core.rs`'s `Fn` arm converts `CoreParam` to `Param`, preserving `resolved_type` and `slot`.
- `bind_args_thunks` reads `Param.resolved_type` at BIND-SPLIT time.

This pipeline eliminates the need for runtime annotation lookup during variadic dispatch — the type is known at force time without re-parsing the annotation.

---

## Strictness Points

The following operations unconditionally force evaluation (necessary strictness):

| Site | Reason |
|---|---|
| `TypeAssert` (`[@T expr]`) | Type must be known at annotation site |
| `Guarded` thunk dispatch | Type check on crossing a type boundary |
| Intermediate sequential dicts | Scope bindings must be extracted before the next expression |
| `match` scrutinee | Pattern matching requires WHNF |
| Typed variadic dispatch (`...xs@T`) | Runtime type determines which bucket receives the arg |
| `builtin-if` condition | Branch selection requires Bool value |
| Arithmetic/comparison builtins | Operations require scalar values |
| `emit` / `write` / I/O builtins | Output requires materialized value |

The following must **not** force evaluation (laziness invariants):

| Site | What stays lazy |
|---|---|
| Dict construction | Values in `Value::Dict` are `Arc<Thunk>`s, never materialized |
| Function argument passing | Args are `Thunk::core_expr` until the callee forces them |
| `%` at `---` boundaries | Last thunk passed as-is, no materialization |
| `Value::Seq { head, tail }` | Both `head` and `tail` are `ThunkId`s |
| `Value::Overlay(l, r)` | Neither side is materialized until the Overlay is flattened |
| `Value::Function` body | Body is `Arc<Spanned<CoreExpr>>`, forced only on call |

---

## Interaction with Other Subsystems

### Parser

The evaluator receives a `SurfaceProgram` (from `parse()`) after it has been processed by `desugar` and `resolve`. The evaluator does **not** parse source text. It does invoke `lower::lower()` internally at thunk force time (for `Surface` thunks and inside `eval_document_exprs_with_env`), converting `SurfaceNode` to `CoreExpr`.

### Type Checker

The type checker communicates with the evaluator through several channels:

1. **Inline AST annotations**: `SurfaceExpression::TypeAssert.resolved_type` (a `TypeAnnotation` OnceLock) is set by the type checker and read by `lower.rs` to populate `CoreExpr::TypeAssert.resolved_type`. Used at force time by `TypeAssertCheck`.
2. **`EvalContext.boundary_guards`**: set by `set_boundary_guards()` after type checking; wraps expressions in `Guarded` thunks at eval time.
3. **`EvalContext.do_infer_resolutions`**: set by `set_do_infer_resolutions()` for `[do]` form monad inference.
4. **`EvalContext.tycon_env`**: set by `set_tycon_env()` after type checking; used by `is_subtype` for user-defined type constructors.
5. **`CoreParam.resolved_type` and `slot`**: written by the lowerer from `SurfaceParam.resolved_annotation_type`; used by `bind_args_thunks` for typed variadic dispatch.
6. **`TypeContextData`**: accumulated inference env and TyCon definitions; shared via `type_context: Arc<Mutex<Option<TypeContextData>>>`.

### Scope Arena

The scope arena (`src/arena.rs`) is the authoritative store for all thunks and scope frames. It is shared across all child `EvalContext`s via `Rc::clone`. The type checker's scope arena (used for type-stage function lookup) uses `type_stage_scope_id` stored in `TypeContextData.type_stage_scope_id`. The eval-stage arena and type-stage scope references are separate and do not alias.

### Builtins

Builtins are registered in `core_builtins()` (`src/builtins_core.rs`) and pre-populated into the root scope at `EvalContext` construction time. A builtin function has type `BuiltinFn = fn(BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>>`. Builtins receive `BuiltinArgs { args: Vec<ThunkId>, named: Option<IndexMap<String, ThunkId>>, call_span, ctx, caller_env_id }`. They return `Arc<Thunk>` — a builtin may return a lazy thunk or a pre-materialized `Thunk::value(...)`. Builtins are **not** supposed to call `materialize` on arguments they don't need.

The `BuiltinDef.force_count` and `pos_strictness` fields allow the CEK machine to pre-materialize arguments iteratively (via `BuiltinForceArg`) before dispatching the builtin, preventing Rust stack growth for strict-argument chains.

### Import / Include

The `include` builtin (`builtin_include` in `builtins_meta.rs`) uses `ctx.with_base_dir()` to create a child context for the included file. Included files receive a fresh scope but share the same arena (ThunkIds from the parent remain valid). The `%libdir` capability is threaded via `ctx.libdir_dir`.

---

## Layering Notes

**`eval.rs` as a hub module:** `eval.rs` re-exports from all other eval submodules and defines `EvalContext`. This is intentional — `EvalContext` must be defined exactly once and all eval files need it. The circular dependency (`eval.rs` imports from `eval_call.rs`; `eval_call.rs` imports `materialize` and `EvalContext` from `eval.rs`) is safe because neither module's initialization depends on the other (all symbols are function-level references).

**Legacy `Env` chain still present in match dispatch:** `MatchDispatchData`, `MatchGuardCheckData`, and `MatchPredicateCheckData` carry `env: Arc<RwLock<Env>>` alongside `env_id: u32`. This is a transitional state (B-515): pattern matching is migrating from the legacy Env chain to the FlatEnv arena. The `env` field is passed to `match_pattern` and `apply_predicate_to_subject` but is not the authoritative scope — `env_id` is. This dual-path is a known debt item.

**Standard evaluation uses `CoreExpr` thunks exclusively.** `UnevaluatedState::Surface` was deleted in T-1770. The `eval` builtin now calls `lower()` followed by `eval_core_expr()` inline — no intermediate `Surface` thunk is created. All evaluation paths go through `CoreExpr` thunks after the `builtin-lower` step.

**Annotation evaluation in `eval_dict.rs`:** `eval_annotation_property_dict` supports only literal annotation values (string, int, float). Non-literal annotation values (e.g., a VarRef type name in `@[return: Dict doc: "..."]`) are silently skipped with `continue`. Full expression evaluation for annotation property dict values is tracked as T-1620.
