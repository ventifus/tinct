# Lower and Eval

Lowering and evaluation are fused: there is no upfront lowering pass. `lower()` is called lazily inside the CEK machine when a `Surface` thunk is first forced. `eval_core_expr()` then converts the resulting `CoreExpr` into thunks. Both steps happen on demand, per thunk, per first access.

---

## Two-Layer Lazy Pipeline

```
Surface thunk first forced
    │
    │ lower(arc, scope_frames)          src/lower.rs
    ↓
Spanned<CoreExpr>                        + Vec<LowerDiagnostic>
    │
    │ eval_core_expr(expr, ctx)          src/eval_core.rs
    ↓
Arc<Thunk>  (unevaluated or already materialized)
    │
    │ CEK machine forces on demand       src/eval_materialize.rs
    ↓
Value
```

A thunk that is never demanded is never lowered or evaluated. `SurfaceExpression::Error` nodes (from parse recovery) become `CoreExpr::Placeholder`, which only errors when forced.

---

## Lowering — `src/lower.rs`

### Entry Point

```rust
pub fn lower(
    arc: &Arc<SurfaceNode>,
    scope_frames: Option<&[IndexMap<String, u32>]>,
) -> (Spanned<CoreExpr>, Vec<LowerDiagnostic>)
```

Pure function of the `SurfaceNode` — no external tables are consulted. All cross-phase data is read from inline OnceLock fields on the node:
- `Resolution` → de Bruijn `(level, slot)` for `VarRef` and `Field`
- `TypeAnnotation` (on `TypeAssert.resolved_type`) → the resolved `Type`
- `TypeAnnotation` (on `SurfaceNode.type_guard`) → wraps the output in `CoreExpr::TypeAssert`
- `CallDispatch` → mangled instance binding name for typeclass method calls
- `SlotAnnotation` (on `Field.field_slot`) → O(1) slot access vs key-based lookup

`scope_frames` — when `Some`, provides the accumulated resolver scope frames from the init program. Used to resolve `call_dispatch` mangled names to de Bruijn coordinates. Pass `None` in test contexts and bootstrap paths.

### Diagnostics

```rust
pub struct LowerDiagnostic {
    pub kind: LowerDiagnosticKind,  // Error or Warning
    pub message: String,
    pub span: Span,
}
```

When lowering fails for a sub-expression (unresolvable variable, missing resolver coordinates, parse error node), a `LowerDiagnostic` is pushed and the expression is replaced with `CoreExpr::Placeholder`. The placeholder only produces an error at runtime if it is forced.

Callers that need eager error reporting (document loading) inspect the returned diagnostic vec. Callers that discard it accept lazy error discovery.

### `SurfaceExpression` → `CoreExpr` Dispatch Table

| Surface form | CoreExpr output | Notes |
|---|---|---|
| `Int(n)` / `U64(n)` / `Float(n)` | `Int` / `U64` / `Float` | Direct |
| `StringLiteral { prefix: "", delimiter: "\"" }` | `Str(processed)` | Single-quoted: escape sequences processed |
| `StringLiteral { prefix: "", delimiter: "\"\"\"" }` | `Str(raw)` | Triple-quoted: content passed through raw |
| `StringLiteral { prefix: "i", .. }` | *(asserts unreachable)* | Must be desugared before lowering |
| `VarRef` with `resolution = Some(Some((l, s)))` | `Var { level: l, slot: s }` | Resolved |
| `VarRef` with `resolution = Some(None)` | `Placeholder` + diagnostic | Explicitly unresolvable |
| `VarRef` with `resolution = None`, name `"_"` | `Var { level: 0, slot: 0 }` | Wildcard sentinel; coords never accessed |
| `VarRef` with `resolution = None`, other name | `Placeholder` + diagnostic | Undefined variable |
| `VarRef` with `call_dispatch` set | `Var` using mangled name coords | Typeclass instance dispatch |
| `Field { expr: Some, field_slot: Some(s) }` | `Call(slot-get, [Int(s), target])` | O(1) typed field access |
| `Field { expr: Some, field_slot: None }` | `Call(field-get, [Str/Int(key), target])` | Key-based lookup |
| `Field { expr: None, name }` | `Var { level, slot }` from `resolution` | Leading-dot parent-scope access |
| `Pipe { lhs, rhs }` | `Call(rhs, [lhs])` | Defensive; desugar should handle first |
| `Dict` (no spread entries) | `Dict(core_entries)` | Static keys become `Str`; non-escaped VarRef keys too |
| `Dict` (with `...spread`) | Nested `Call(merge, ...)` | Left-associative merge chain |
| `Dict` with `InstanceDecl` (multi-arm) | `Dict` with mangled-name entries | `instance_binding_name` keying |
| `Dict` with `TypeAlias` | `Dict` with constructor entries | `lower_type_alias_to_constructor_dict` |
| `Dict` with `ClassDecl` (named) | `Dict([])` empty placeholder | Occupies a slot; no runtime methods |
| `Call` | `Call` | Recurses into func, args, named_args |
| `Fn` | `Fn` | Params → `CoreParam`; body lowered recursively |
| `TypeAssert` | `CoreExpr::TypeAssert` with type from `resolved_type` OnceLock | Falls back to `Type::Unknown` if not set |
| `Sequential` | `Sequential` | Each expr lowered in order |
| `Match` | `Match` with lowered scrutinee and arms | Pattern lowering converts `TypeAssertPending` → `TypeAssert` |
| `Quote` | `Quote` | Inner lowered with diagnostics suppressed; `scope_frames: None` inside |
| `Unquote` / `UnquoteSplice` | `Unquote` / `UnquoteSplice` | |
| `LetDecl` | `LetDecl` | Binding names lowered as `Str`; values lowered normally |
| `PatternDecl` | `PatternDecl` | |
| `CaseArm` | `CaseArm` | |
| `Error(span)` | `Placeholder` + diagnostic | Parse recovery node |
| `Placeholder` | `Placeholder` | |

### Type Guard Wrapping

After lowering the expression, `lower_inner` checks `SurfaceNode.type_guard`. If the type checker set a guard type, the output is wrapped:

```rust
CoreExpr::TypeAssert {
    annotation: Simple("__guard__"),
    expr: Arc::new(Spanned::new(core_expr, span)),
    resolved_type: guard_type,
    pipeline_blame: None,
}
```

This produces a runtime type check injected by the type checker, not written by the user.

### TypeAlias Lowering

`[type Color Red Green Blue]` as a dict-entry value (`Color: [type Red Green Blue]`) lowers to a constructor dict:

```
Color: {
    Red:   Variant { tycon: "Color", ctor: "Red",   payload: None }
    Green: Variant { tycon: "Color", ctor: "Green", payload: None }
    Blue:  Variant { tycon: "Color", ctor: "Blue",  payload: None }
}
```

Payload constructors (`Circle: [r: Float]`) lower to functions:
```
Circle: [fn [let r] Variant { tycon: "Color", ctor: "Circle", payload: { r: $r } }]
```

A `TypeAlias` in standalone expression position (not a dict-entry value) lowers to `CoreExpr::Dict([])` — an empty dict. There is no name to bind constructors under.

---

## Evaluation — `src/eval.rs` + `src/eval_core.rs`

### EvalContext

`EvalContext` is the evaluation infrastructure bundle. Key fields:

| Field | Type | Purpose |
|---|---|---|
| `config` | `Arc<EvalConfig>` | Immutable: base dir, no_fs flag, integrity checking, etc. |
| `state` | `Arc<Mutex<EvalState>>` | Mutable: `eval_stack` for cycle path reconstruction |
| `scope_arena` | `Rc<RefCell<ScopeArena>>` | All scope frames and thunk slots |
| `current_env_id` | `u32` | `ScopeId` for new anonymous slot allocations |
| `scope_frames` | `Option<Arc<Vec<IndexMap<String, u32>>>>` | Resolver scope frames for `call_dispatch` resolution in `lower()` |
| `type_context` | `Arc<Mutex<Option<TypeContextData>>>` | Unified type environment; `None` until initialized |
| `cancel` | `CancellationToken` | Cooperative cancellation for async operations |

`EvalContext` is `Arc`-shared across all thunks and tasks in a session. Child contexts (e.g., `with_eval_scope(env_id)`) clone the `Arc` and change only `current_env_id` — all other fields are shared.

### `eval_surface_file`

```rust
pub async fn eval_surface_file(
    program: &SurfaceProgram,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>>
```

The primary entry point for evaluating a complete `SurfaceProgram`. Iterates over documents, evaluates each, and threads `%` between them: the output of document N is bound as `%` in document N+1's scope.

**Precondition:** `desugar_surface_program` and `resolve_surface_program` must have run on the program before this is called. Failure to do so produces either incorrect de Bruijn coordinates or placeholder errors at runtime.

### Document Scope-Chain Semantics

Each document is a sequence of expression items. `eval_document_exprs` evaluates them as a scope chain:

```
For each expression item except the last:
    1. lower → eval → materialize (forces the value)
    2. If the result is a Dict or Overlay:
       For each string-keyed entry, allocate a child env and inject the entry
       as a lazy thunk into that env for subsequent expressions to see.
    3. If the result is not a Dict/Overlay, it is silently discarded.

For the last expression:
    1. lower → eval → return thunk (lazy, not forced)
```

The last expression's thunk is returned to the caller without forcing. This is the "value" of the document. Only expressions that are actually accessed by the pipeline output end up being evaluated.

### `eval_core_expr`

```rust
pub(crate) async fn eval_core_expr(
    expr: &Arc<Spanned<CoreExpr>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>>
```

Converts one `CoreExpr` node into a thunk. Most variants create an `Unevaluated` thunk wrapping the sub-expression; only literals produce immediately-materialized thunks.

Key dispatch:

| CoreExpr | Produces |
|---|---|
| `Int` / `Float` / `Str` / `U64` | Materialized `Value::Int` / `Float` / `Str` / `U64` thunk |
| `Var { level, slot }` | Walks parent chain `level` hops, returns thunk at `slot` |
| `Dict` | Evaluates letrec: all entries allocated as thunks first (reserve), then filled |
| `Fn` | Materialized `Value::Function` capturing `current_env_id` as `closure_env_id` |
| `Call { func, args }` | `UnevaluatedState::Call` thunk — deferred to CEK machine |
| `TypeAssert { expr, resolved_type }` | `UnevaluatedState::Guarded` thunk wrapping `expr` |
| `Sequential` | `CoreExpr::Sequential` thunk — `SequentialStep` continuation in CEK machine |
| `Match` | `UnevaluatedState::CoreExpr` thunk; `MatchDispatch` continuation when forced |
| `Variant` | Materialized `Value::Variant` |
| `Placeholder` | `Failed` thunk with "undefined variable" error |

**Letrec dict construction:** `CoreExpr::Dict` implements letrec in two phases via `ScopeArena`:
1. **Phase 1 (reserve):** allocate all entry slots with `reserve_slot()` to establish indices
2. **Phase 2 (fill):** lower and eval each entry's value expression, then `fill_slot()` with the resulting thunk

This two-phase protocol allows entries to reference each other by slot before any of them are evaluated.

---

## Invariants

1. **Lowering is a pure function.** `lower()` reads inline OnceLocks but never writes them. The same `Arc<SurfaceNode>` can be lowered multiple times (though in practice each node is lowered at most once, when its thunk is first forced).
2. **`CoreExpr::Placeholder` is the error sentinel.** Every lowering error replaces the sub-expression with `Placeholder`, which carries the error lazily — it only fires when forced.
3. **`Type::Unknown` is the accept-all fallback.** A `TypeAssert` node whose `resolved_type` OnceLock is empty (typecheck skipped or failed) gets `Type::Unknown`, which passes for any value at runtime.
4. **`Quote` suppresses diagnostics.** Lowering inside `Quote` uses a fresh diagnostic vec that is discarded. `VarRef` nodes inside a quote are symbols, not variable references, so undefined-variable errors must not be reported.
5. **`eval_surface_file` precondition.** Desugaring and resolution must run before evaluation. The evaluator does not check or re-run them.
6. **Letrec correctness.** All slots in a dict scope are reserved before any are filled. This ensures that during fill-phase evaluation, any entry that references a sibling gets a valid (possibly unevaluated) thunk — not a use-after-free.
