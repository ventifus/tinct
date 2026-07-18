# Lower and Eval

This document is for Rust contributors working in `src/lower.rs`, `src/eval.rs`, and `src/eval_core.rs`. Tinct developers: the key architectural fact is that lowering is not a separate compiler pass — it happens per-thunk at first force. A tinct expression that is never accessed is never lowered or evaluated.

Lowering and evaluation are fused: there is no upfront lowering pass. `lower()` is called lazily inside the CEK machine when a `Surface` thunk is first forced, and eagerly at each document boundary in `eval_document_exprs_with_env`. `eval_core_expr()` then converts the resulting `CoreExpr` into thunks. Both steps happen on demand, per thunk, per first access.

---

## Two-Layer Lazy Pipeline

```
Surface thunk first forced (or document boundary)
    │
    │ lower(arc, scope_frames)          src/lower.rs
    ↓
Spanned<CoreExpr>                        + Vec<LowerDiagnostic>
    │
    │ eval_core_expr(expr, env_id, ctx)  src/eval_core.rs
    ↓
Arc<Thunk>  (unevaluated or materialized)
    │
    │ CEK machine forces on demand       src/eval_materialize.rs
    ↓
Value
```

A thunk that is never demanded is never lowered or evaluated. `SurfaceExpression::Error` nodes (from parse recovery) become `CoreExpr::Placeholder`. When a document boundary is reached, lower diagnostics are converted to `EvalError` eagerly — the placeholder only fires lazily for `Surface` thunks forced later by the CEK machine, not at document-load time.

---

## Lowering — `src/lower.rs`

### Entry Points

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

```rust
pub(crate) fn lower_inner(
    arc: &Arc<SurfaceNode>,
    diagnostics: &mut Vec<LowerDiagnostic>,
    scope_frames: Option<&[IndexMap<String, u32>]>,
) -> Spanned<CoreExpr>
```

Internal entry point that threads the diagnostics accumulator. Used by all recursive calls within `lower.rs` and by callers (e.g., quote evaluation) that hold an existing diagnostic vec.

### Diagnostics

```rust
pub enum LowerDiagnosticKind {
    Error,
    Warning,   // infrastructure only — not yet emitted
}

#[must_use]
pub struct LowerDiagnostic {
    pub kind: LowerDiagnosticKind,
    pub message: String,
    pub span: Span,
}
```

When lowering fails for a sub-expression (unresolvable variable, missing resolver coordinates, parse error node), a `LowerDiagnostic` is pushed and the expression is replaced with `CoreExpr::Placeholder`.

**Eager vs lazy error reporting:** Callers at document boundaries (e.g., `eval_document_exprs_with_env`) call `lower_errors_to_eval_error(diags)` and propagate the error immediately, preventing the document from loading. Callers that discard the diagnostic vec (e.g., recursive lowering inside `Quote`) accept lazy error discovery — the `Placeholder` only fires when that thunk is forced.

```rust
pub fn lower_errors_to_eval_error(diags: Vec<LowerDiagnostic>) -> Option<Box<EvalError>>
```

Located in `eval_materialize.rs`. Converts the first error diagnostic to `EvalError::user_error`; subsequent errors are appended to the message. Returns `None` if there are no error-severity diagnostics.

### Additional Public Functions

```rust
pub(crate) fn process_escapes(content: &str, delimiter: &str) -> String
```

Processes escape sequences in single-quoted string literals (`\n`, `\t`, `\r`, `\\`, `\<delimiter>`, unknown → pass-through). Triple-quoted strings bypass this entirely — their content is passed raw.

```rust
pub fn core_expr_to_surface_node(expr: &Spanned<CoreExpr>) -> Arc<SurfaceNode>
```

Converts a `CoreExpr` back to a `SurfaceNode` for quote/unquote evaluation. Bridges through `core_expr_to_surface_expr`. Used by `eval_core_expr`'s `Quote` arm. The round-trip is lossless for variable names because `CoreExpr::Var` preserves the original name alongside its de Bruijn coordinates.

```rust
pub(crate) fn annotation_name_to_type(name: &str) -> Type
```

Converts well-known type names (`"Int"`, `"String"`, `"Bool"`, `"Seq"`, etc.) to `Type` values for `TypeAssertPending` pattern lowering. `Type::Unknown` (accept-all) is the fallback for unrecognized names.

```rust
pub(crate) fn extract_dispatch_tags(arm_pattern: &SurfaceExpression) -> Vec<Option<String>>
```

Extracts concrete uppercase type annotation names from an instance arm pattern (`[let a@Int b@Float c]`). Returns one `Option<String>` per binding: `Some("Int")` for annotated concrete types, `None` for unannotated or TypeVar bindings. Used by instance binding name generation.

```rust
fn lower_pattern(pat: &Pattern) -> Pattern
```

Converts `TypeAssertPending` pattern nodes to `TypeAssert` by reading the inline `resolved` OnceLock and falling back to `annotation_name_to_type`. Recurses into `Or`, `Constructor`, and `Dict` sub-patterns. `Predicate` patterns carry a `SurfaceNode` that is left unchanged — it is lowered on demand inside `MatchDispatch` at eval time.

### `SurfaceExpression` → `CoreExpr` Dispatch Table

| Surface form | CoreExpr output | Notes |
|---|---|---|
| `Int(n)` / `U64(n)` / `Float(n)` | `Int` / `U64` / `Float` | Direct |
| `StringLiteral { prefix: "", delimiter: "\"" }` | `Str(processed)` | Single-quoted: escape sequences processed |
| `StringLiteral { prefix: "", delimiter: "\"\"\"" }` | `Str(raw)` | Triple-quoted: content passed through raw |
| `StringLiteral { prefix: "i", .. }` | *(asserts unreachable)* | Must be desugared before lowering |
| `VarRef` with `resolution = Some(Some((l, s)))` | `Var { level: l, slot: s, name, annotation }` | Resolved |
| `VarRef` with `resolution = Some(None)` | `Placeholder` + diagnostic | Explicitly unresolvable |
| `VarRef` with `resolution = None`, name `"_"` | `Var { level: 0, slot: 0, name: "_" }` | Wildcard sentinel; coords never accessed |
| `VarRef` with `resolution = None`, other name | `Placeholder` + diagnostic | Undefined variable |
| `VarRef` with `call_dispatch` set | `Var` using mangled name coords from scope_frames | Typeclass instance dispatch |
| `Field { expr: Some, field_slot: Some(s) }` | `Call(slot-get, [Int(s), target])` | O(1) typed field access; slot-get is always field-get-slot + 1 |
| `Field { expr: Some, field_slot: None }` | `Call(field-get, [Str/Int(key), target])` | Key-based lookup |
| `Field { expr: None, name }` | `Var { level, slot }` from `resolution` | Leading-dot parent-scope access |
| `Field { expr: None, Int }` | `Placeholder` + diagnostic | Leading-dot integer access; parser should reject this |
| `Pipe { lhs, rhs }` | `Call(rhs, [lhs])` | Defensive; desugar should handle first |
| `Sequential(exprs)` | `Sequential(core_exprs)` | Each expr lowered in order |
| `Dict` (no spread entries, no Decl values) | `Dict(core_entries)` | Static keys become `Str`; escaped VarRef keys computed |
| `Dict` (with `...spread`) | Nested `Call(merge, ...)` | Left-associative merge chain; `merge` Var uses `level: u32::MAX, slot: u32::MAX` (name-based fallback) |
| `Dict` with named `InstanceDecl` | `Dict` with single outer key, lowered value | Named instance binding preserved |
| `Dict` with anonymous `InstanceDecl` (multi-arm) | `Dict` with mangled-name entries | `instance_binding_name` keying for each method |
| `Dict` with `TypeAlias` | `Dict` with constructor entries | `lower_type_alias_to_constructor_dict` |
| `Dict` with `ClassDecl` (named) | `Dict([])` empty placeholder | Occupies a slot; no runtime methods |
| `Call` with `call_dispatch` on func | `Call(Var(mangled), args, named_args)` | Typeclass call-site dispatch rewrite |
| `Call` (plain) | `Call(lowered_func, args, named_args)` | Recurses into func, args, named_args |
| `Fn` | `Fn` | Params → `CoreParam` with slot index; body lowered recursively |
| `TypeAssert` | `CoreExpr::TypeAssert` with type from `resolved_type` OnceLock | Falls back to `Type::Unknown` if type check skipped or produced `Type::Error` |
| `Rest(name)` | `CoreExpr::Rest(name)` | Only valid in type expressions |
| `Match` | `Match` with lowered scrutinee and arms | Pattern lowering converts `TypeAssertPending` → `TypeAssert`; multi-body arms wrapped in `Sequential` |
| `Quote` | `Quote` | Inner lowered with a fresh throwaway diagnostic vec; `scope_frames: None` inside |
| `Unquote` / `UnquoteSplice` | `Unquote` / `UnquoteSplice` | Diagnostics and scope_frames threaded |
| `LetDecl` | `LetDecl` | Even-index bindings lowered as `Str` (name extraction); odd-index lowered normally |
| `PatternDecl` | `PatternDecl` | |
| `CaseArm` | `CaseArm` | Used by match evaluation; not a standalone expression |
| `Decl(InstanceDecl)` in expression position | `Dict` with mangled-name entries, or `Placeholder` if empty | Multi-arm InstanceDecl outside a dict entry |
| `Decl(TypeAlias)` in expression position | `Dict([])` | No name to bind constructors under; empty dict |
| `Decl(other)` | `Placeholder` | Other declarations not valid as expressions |
| `Error(span)` | `Placeholder` + diagnostic | Parse recovery node |
| `Placeholder` | `Placeholder` | |

### Type Guard Wrapping

After lowering the expression, `lower_inner` checks `SurfaceNode.type_guard`. If the type checker set a guard type (via the `TypeAnnotation` OnceLock), the output is wrapped:

```rust
CoreExpr::TypeAssert {
    annotation: Annotation::Simple("__guard__"),
    expr: Arc::new(Spanned::new(core_expr, span)),
    resolved_type: guard_type,
    pipeline_blame: None,
}
```

This produces a runtime type check injected by the type checker, not written by the user. The annotation name `"__guard__"` distinguishes injected guards from user-written `[@Type expr]` annotations.

### TypeAlias Lowering

`[type Color Red Green Blue]` as a dict-entry value (`Color: [type Red Green Blue]`) lowers to a constructor dict:

```
Color: {
    Red:   UnitVariant { tycon: "Color", ctor: "Red" }
    Green: UnitVariant { tycon: "Color", ctor: "Green" }
    Blue:  UnitVariant { tycon: "Color", ctor: "Blue" }
}
```

`CoreExpr::UnitVariant` — not `CoreExpr::Variant { payload: None }` — is emitted for unit constructors. The two differ in how `eval_core_expr` handles them.

Payload constructors (`Circle: [r: Float]`) lower to functions:
```
Circle: Fn { params: [r@slot0], body: Variant { tag: "Color.Circle", payload: Dict { r: Var(level=1, slot=0) } } }
```

The body uses `level=1` to skip the function body's own letrec env and reach the params. The function's `return_ann` is set to `Annotation::Simple("Color.Circle")` so pattern matching can identify the constructor without a special runtime type.

Unit constructors with `@[...]` annotations (e.g., `Red@[category: "primary"]`) are wrapped in a `Call(builtin-make-annotated, [UnitVariant, ann_dict])` call.

A `TypeAlias` in standalone expression position (not a dict-entry value) lowers to `CoreExpr::Dict([])` — an empty dict. There is no name to bind constructors under.

### Spread Dict Lowering

`[a: 1  ...rest  b: 2]` desugars to a left-associative merge chain:

```
Call(merge, [Dict([a: 1]), rest])
→ Call(merge, [previous, Dict([b: 2])])
```

The `merge` Var uses `level: u32::MAX, slot: u32::MAX` — a sentinel that triggers the name-based fallback in `eval_core.rs` (special-cased for `"merge"` and `"field-get"`) rather than slot-based lookup. This is a deliberate exception to the strict de Bruijn discipline, used because `merge` is always a root builtin and the spread-desugar site has no env context.

---

## Evaluation — `src/eval.rs` + `src/eval_core.rs`

### EvalContext

`EvalContext` is the evaluation infrastructure bundle. Key fields:

| Field | Type | Purpose |
|---|---|---|
| `config` | `Arc<EvalConfig>` | Immutable: base_dir, no_fs, require_integrity, macro_injects_map, source_file |
| `scope_arena` | `Rc<RefCell<ScopeArena>>` | All FlatEnv scopes and thunk slots; single-threaded (LocalSet) |
| `program_store` | `Rc<RefCell<Vec<SurfaceProgram>>>` | Append-only store for `Value::Program` payloads; shared via Rc::clone |
| `env_allowed` | `Option<HashSet<String>>` | OS env var allowlist; None = all allowed |
| `blame_map` | `Mutex<HashMap<ThunkId, String>>` | Pipeline blame: maps `%` ThunkId → producing stage label |
| `boundary_guards` | `RwLock<HashMap<Span, Type>>` | Type inference boundary guards: span → expected type |
| `do_infer_resolutions` | `RwLock<HashMap<String, String>>` | `[do]` monad infer resolutions: sentinel name → resolved monad name |
| `libdir_dir` | `Mutex<Option<Arc<Dir>>>` | Open libdir Dir, shared across nested includes |
| `cancel` | `CancellationToken` | Cooperative cancellation for async builtins |
| `task_registry` | `Arc<Mutex<Vec<JoinHandle<()>>>>` | Background task handles; `drain` aborts all |
| `profiling` | `Option<Arc<Mutex<ProfilingCollector>>>` | Per-span timing; None when disabled |
| `tycon_env` | `OnceLock<Arc<TyConEnv>>` | Type constructor environment from inference; set once after typecheck |
| `type_context` | `Arc<Mutex<Option<TypeContextData>>>` | Unified type environment; None until `builtin-make-type-ctx` |
| `scope_frames` | `Option<Arc<Vec<IndexMap<String, u32>>>>` | Resolver scope frames for `call_dispatch` resolution in `lower()` |

`EvalContext` is `Arc`-shared across all thunks and tasks in a session. Child contexts (created by `with_base_dir`, `with_cancel_token`, etc.) clone `Arc<EvalConfig>` but share the same `scope_arena` via `Rc::clone`. All child contexts share the same `type_context` and `tycon_env`.

**No `EvalState` or `eval_stack` field.** The evaluation stack for cycle path reconstruction is a `tokio::task_local!` (`TASK_EVAL_STACK` in `eval_materialize.rs`) — it is per-async-task, not shared across thunks.

### `eval_surface_file`

```rust
pub async fn eval_surface_file(
    program: &SurfaceProgram,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>>
```

The primary entry point for evaluating a complete `SurfaceProgram`. Iterates over documents and returns the last document's output thunk. **It does not thread `%` between documents** — that is the loader.llt's responsibility via `builtin-eval` and `builtin-scope-new`.

```rust
pub async fn eval_surface_file_with_input(
    program: &SurfaceProgram,
    ctx: &Arc<EvalContext>,
    initial_input: Option<Arc<Thunk>>,
) -> EvalResult<Arc<Thunk>>
```

Variant that binds an initial `%` value in a child FlatEnv scope before evaluation. Used by the formatter.

**Precondition:** `desugar_surface_program` and `resolve_surface_program` must have run on the program before this is called. Failure to do so produces either incorrect de Bruijn coordinates or placeholder errors at runtime.

### Document Scope-Chain Semantics

Each document is a sequence of expression items. `eval_document_exprs_with_env` evaluates them as a scope chain using the FlatEnv/ScopeArena:

```
For each expression item except the last:
    1. lower(node) → check lower diagnostics → error on first Error diagnostic
    2. eval_core_expr → returns Arc<Thunk>
    3. materialize (forces the value)
    4. Detect which FlatEnv was newly allocated (via arena length delta)
    5. If a new FlatEnv was allocated, advance current_env_id to it so subsequent
       dicts are children, forming the scope chain.

For the last expression:
    1. lower(node) → check lower diagnostics → error on first Error diagnostic
    2. eval_core_expr → return thunk lazily (no materialization)
```

Scope chaining works via `ScopeId`/`alloc_child`: each dict allocates a FlatEnv as a child of the previous dict's FlatEnv, making all ancestor entries visible to inner expressions via the parent chain. De Bruijn `level` counts hops from innermost to outermost.

The last expression's thunk is returned to the caller without forcing. Only expressions that are actually accessed by the pipeline output end up being evaluated.

**Non-dict intermediate results:** If an intermediate expression produces a non-dict value (or no new FlatEnv is allocated), the scope chain is not extended — the result is silently discarded.

### `eval_core_expr`

```rust
pub(crate) fn eval_core_expr<'a>(
    expr: &'a Spanned<CoreExpr>,
    env_id: u32,
    ctx: &'a Arc<EvalContext>,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + 'a>>
```

Converts one `CoreExpr` node into a thunk. Most variants create a deferred thunk wrapping the sub-expression; only literals produce immediately-materialized thunks. Returns a boxed future to permit recursion in async contexts.

Key dispatch:

| CoreExpr | Produces |
|---|---|
| `Int` / `Float` / `Str` / `U64` | Materialized `Value::Int` / `Float` / `Value::String` / `Value::U64` thunk |
| `Var { level, slot }` | Walks parent chain `level` hops via `scope_arena.walk_parent_chain`; returns thunk at `slot` |
| `Dict(entries)` | Calls `eval_dict_core` — allocates a FlatEnv child, creates literal thunks for literal entries and `UnevaluatedState::CoreExpr` thunks for non-literal entries (letrec) |
| `Fn` | Materialized `Value::Function` capturing `env_id` as `closure_env_id`; FnAnnotation populated from `return_ann` |
| `Call { func, args }` | Calls `eval_call_core` — creates a `UnevaluatedState::FnCall` or `UnevaluatedState::BuiltinCall` thunk |
| `TypeAssert { .. }` | `UnevaluatedState::CoreExpr` thunk — CEK machine handles `TypeAssertCheck` continuation |
| `Sequential` | `UnevaluatedState::CoreExpr` thunk — CEK machine handles `SequentialStep` continuation |
| `Match` | `UnevaluatedState::CoreExpr` thunk — CEK machine handles `MatchDispatch` continuation |
| `Variant { payload: None }` | Materialized `Value::Variant { tycon, ctor, payload: None }` |
| `Variant { payload: Some(expr) }` | Materializes the payload dict immediately, stores as ThunkId; `Value::Variant { payload: Some(id) }` |
| `UnitVariant { tycon, ctor }` | Materialized `Value::Variant { tycon, ctor, payload: None }` |
| `Quote(inner)` | `core_expr_to_surface_node(inner)` → `eval_quote_walk(...)` |
| `Rest` | Immediate error ("rest marker only valid in type expressions") |
| `Unquote` / `UnquoteSplice` | Immediate error ("only valid inside quote") |
| `PatternDecl` | Immediate error ("only valid in instance match arms") |
| `LetDecl` | Evaluates binding pairs into a `Value::Dict`; dict values are lazy thunks |
| `CaseArm` | Immediate error ("case arms are not expressions") |
| `Placeholder` | Immediate `Err(EvalError)` — error propagates to caller of `eval_core_expr` |

**Letrec dict construction (via `eval_dict_core`):** `CoreExpr::Dict` implements letrec in two phases via `ScopeArena`:
1. **Phase 1 (reserve):** `alloc_child(parent_env_id, entries.len())` — allocates a FlatEnv with all slots reserved.
2. **Phase 2 (fill):** For each entry, create a literal thunk or `Thunk::core_expr(value_expr, dict_env_id, ...)`. The thunk captures `dict_env_id` so when forced, it evaluates in the dict's own scope — enabling mutual recursion.

This two-phase protocol allows entries to reference each other by slot before any of them are evaluated. There is no separate `reserve_slot`/`fill_slot` API — the `ScopeArena` slice is pre-sized at allocation and slots are written by index.

---

## Invariants

1. **Lowering is a pure function.** `lower()` reads inline OnceLocks but never writes them. The same `Arc<SurfaceNode>` can be lowered multiple times (in practice each document-level node is lowered exactly once; `Surface` thunks are lowered once when first forced).
2. **`CoreExpr::Placeholder` is the error sentinel.** Every lowering error replaces the sub-expression with `Placeholder`. At document boundaries, lower diagnostics are converted to errors eagerly. Inside `Surface` thunks forced by the CEK machine, the placeholder fires lazily when forced.
3. **`Type::Unknown` is the accept-all fallback.** A `TypeAssert` node whose `resolved_type` OnceLock is empty (typecheck skipped or failed) gets `Type::Unknown`, which passes for any value at runtime.
4. **`Quote` suppresses diagnostics.** Lowering inside `Quote` uses a fresh diagnostic vec that is discarded. `VarRef` nodes inside a quote are symbols, not variable references, so undefined-variable errors must not be reported.
5. **`eval_surface_file` precondition.** Desugaring and resolution must run before evaluation. The evaluator does not check or re-run them.
6. **Letrec correctness.** The FlatEnv for a dict scope is allocated before any entry value thunks are created. Each value thunk captures the dict's `env_id`. When any value thunk is forced, it can look up sibling entries by slot — the slots are already allocated, even if not yet evaluated.
7. **`%` threading is loader.llt's responsibility.** `eval_surface_file` does not thread `%` between documents. The loader (via `builtin-eval` and `builtin-scope-new`) is responsible for making the prior document's output available as `%` in the next document's scope.
8. **Eval stack is per-async-task.** `TASK_EVAL_STACK` (in `eval_materialize.rs`) is a `tokio::task_local!` — it exists per Tokio task, not per `EvalContext`. It is used for cycle path reconstruction when a `Var` lookup encounters an in-progress thunk.
9. **Spread `merge` Var uses sentinel coordinates.** `level: u32::MAX, slot: u32::MAX` is the name-based fallback sentinel for `"merge"` in spread-dict lowering. This is the only approved exception to strict de Bruijn coordinates in lowering.

---

## Layering Notes

### What lowering does vs desugaring

Desugaring (`src/desugar.rs`) is an AST-to-AST transformation: it removes `Pipe` nodes and `$_` placeholders by rewriting them as `Call` and `Fn` nodes in the `SurfaceExpression` layer. Lowering (`src/lower.rs`) is the `SurfaceExpression`-to-`CoreExpr` translation: it reads inline OnceLock fields (set by the resolver and type checker) and produces the internal representation the CEK machine evaluates. The two passes are distinct: desugar runs once before name resolution; lowering runs per-thunk at eval time.

### What lowering does vs name resolution

Name resolution (`src/resolve.rs`) walks the surface AST and writes de Bruijn `(level, slot)` coordinates into inline `OnceLock<Option<(u32, u32)>>` fields on `VarRef` and `Field` nodes. Lowering reads those fields. The resolver does not produce `CoreExpr`; lowering does not perform name lookup. If the resolver did not run on a node, `resolution.get()` returns `None` and lowering emits a diagnostic + `Placeholder`.

### `Source` thunks vs direct lowering

`UnevaluatedState::Surface` thunks are created by the `eval` builtin (re-evaluation of runtime-constructed AST). When forced, `force_step` calls `lower()` then `eval_core_expr()`. Direct lowering (at document boundaries and inside `eval_quote_preprocess`) calls `lower()` and `eval_core_expr()` in the same call path without creating an intermediate `Surface` thunk.
