# Runtime v2 — Implementation Plan

**Whatif:** [`doc/whatif/runtime-v2.md`](../runtime-v2.md)
**Branch:** `runtime-v2` (long-lived; no intermediate commit requirement)
**Test command:** `just test` (full suite)

Three sprints after the prerequisite chain completes. **Parts A–E of Sprint 1 do not independently compile — treat them as one atomic change. The first `cargo check` checkpoint is after Part E; `just test` checkpoint is after Part G.**

---

## Prerequisites (complete in this order)

| Sprint | Notes |
|--------|-------|
| `exhaustiveness-multi-field-nominal` | Fix `coverage.rs` — touches `type_def.rs`; must land first |
| `runtime-v2-type-prereqs` | Full task list below; also touches `type_def.rs`; land after above |
| `include-decomp-eval-primitives` | Deletes `builtin_include`; implements expand/eval/eval-types with interim dict_to_file approach |
| `include-decomp-prelude` | Self-hosted pipeline in prelude; `expand` signature is `Dict→Dict` at this point |
| `include-decomp-review` | `/review-whatif include-decomposition` |

### `runtime-v2-type-prereqs` task list

This sprint is a prerequisite for runtime-v2 but is not in TODO.md until runtime-v2 is accepted.

- Add `Type::Task(Box<Type>)` to `src/type_def.rs`; add arms to `unify`, `is_subtype`, `apply`, `occurs_in`, `collect_type_vars`, `display` — mirrors `Type::Seq` exactly (`src/type_def.rs`, `src/type_unify.rs`, `src/type_env.rs`)
- Add `Type::Channel(Box<Type>)` with same full handling (`src/type_def.rs` et al.)
- Add `Type::Context` — opaque, no type parameter; same files
- Add inference rules in `src/typecheck.rs`: `task` infers `Task(body_type)` from body; `await` unifies `Task(?t)` → `?t`; `send`/`recv` unify channel element type; `select-once` sources require fresh `TypeVar`s for `t` and `r` per call site (not `Unknown`)
- Declare `Signal`, `Action`, `CancelHandle`, `SelectSource` as prelude type aliases/declarations (`stdlib/prelude.llt`)
- Corpus tests for `task`/`await` type inference; `channel`/`send`/`recv` element-type checking (`tests/corpus/`)
- Verify `just test` passes

---

## Sprint 1 — Foundation: AST, Evaluator, Value Types, Rc→Arc

**Goal:** The entire non-async transformation. `just test` passes at the end.

⚠️ **Parts A–E form a single compilation unit.** Deleting `Expr` in Part A immediately breaks every file that imports it. No intermediate `cargo check` is possible until Part E is complete. Work through A→E without expecting partial builds to succeed.

---

### Part A — New type hierarchy (alongside existing types for now; existing deleted in Part E)

Add to `src/ast.rs`:

- `SurfaceExpression` enum — all variants use `Arc<SurfaceNode>` at recursive positions (not `Box` or `Rc`)
- `SurfaceNode { pub expr: SurfaceExpression, pub span: Span }` — dedicated wrapper; no `id` field (identity = `Arc::as_ptr() as usize`)
- `SurfaceProgram { pub documents: Vec<Spanned<SurfaceDocument>> }`
- `SurfaceDocument { pub stage: Option<Stage>, pub name: Option<String>, pub items: Vec<SurfaceItem>, pub output_type: Option<Spanned<Annotation>>, pub expects: Option<Spanned<Annotation>>, pub caps: Option<Spanned<Vec<(String, Annotation)>>> }`
- `SurfaceItem { Expr(Arc<SurfaceNode>), Decl(Spanned<SurfaceDeclaration>) }`
- `SurfaceDeclaration` enum — TypeAlias, ClassDecl, InstanceDecl, DefMacro, MacroDecl, SyntaxClass, Splice
- `CoreExpr` enum — de Bruijn coordinates as plain `u32` fields, all recursive positions use `Arc<Spanned<CoreExpr>>`
- `NodeId(usize)` and `fn node_id(arc: &Arc<SurfaceNode>) -> NodeId { NodeId(Arc::as_ptr(arc) as usize) }`
- `ResolutionTable(HashMap<NodeId, (u32, u32)>)` — level + slot
- `TypeAnnotationTable(HashMap<NodeId, Type>)` — for TypeAssert nodes

Do NOT delete `Expr`, `Document`, `File` yet — kept until Part E.

---

### Part B — Parser, resolver, typechecker, expander migration

**Parser** (`src/parser.rs`):

- Change `parse()` return type from `File` to `SurfaceProgram`
- Every `Rc::new(spanned_expr)` at recursive positions → `Arc::new(SurfaceNode { expr, span })`
- Every `Box::new(spanned_expr)` at recursive positions → `Arc::new(SurfaceNode { expr, span })`
- Every `Expr::Variant { ... }` construction → `SurfaceExpression::Variant { ... }`
- Declaration forms (`Expr::DefMacro`, `Expr::MacroDecl`, `Expr::SyntaxClass`, `Expr::Splice`, `Expr::TypeAlias`, `Expr::ClassDecl`, `Expr::InstanceDecl`) → route to `SurfaceDeclaration` and wrap in `SurfaceItem::Decl`; expression forms → `SurfaceItem::Expr`
- NOTE: `NodeId` is NOT stored in nodes; it is computed on-demand by callers via `Arc::as_ptr()`

**Resolver** (`src/resolve.rs`):

- New entry point: `pub fn resolve_program(program: &SurfaceProgram) -> ResolutionTable`
- Walks `SurfaceProgram` → `SurfaceDocument.items` → `SurfaceItem::Expr` → `SurfaceNode.expr`
- For `SurfaceExpression::VarRef { name, escaped }`: compute de Bruijn coordinates; insert `node_id(&arc) → (level, slot)` into `ResolutionTable`
- **New:** `SurfaceExpression::Pipe` now exists during resolution (was previously eliminated by `desugar.rs` before resolution) — resolver must walk into `Pipe.lhs` and `Pipe.rhs`
- `SurfaceDeclaration` nodes in `SurfaceItem::Decl`: walk their expression children for name binding only; do NOT insert into resolution table
- Return type is `ResolutionTable` — the `SurfaceProgram` is unchanged (immutable by design)
- Remove all `VarRef.resolved: RefCell<...>` mutations from the old resolver

**Typechecker** (`src/typecheck.rs`):

- Walk `SurfaceProgram` instead of `File`
- Produce `TypeAnnotationTable` instead of mutating `TypeAssert.resolved_type: RefCell<Option<Type>>`
- `type_stage_env`: build by calling `build_type_stage_env()` at startup; store in `EvalConfig.type_stage_env: Arc<RwLock<Environment>>` (populated here in Part B; the field itself is added in Part E)

**Expander** (`src/expand.rs`):

- Change all `Expr::Variant` match arms to `SurfaceExpression::Variant`
- `expand_document()`: walks `SurfaceDocument.items`; processes `SurfaceItem::Decl(SurfaceDeclaration::DefMacro/MacroDecl/SyntaxClass)` to register macros; processes `SurfaceItem::Decl(SurfaceDeclaration::Splice)` by flattening into `SurfaceItem::Expr` entries inline
- Macro round-trip replacement: `ast_to_dict_expr` → `surface_expr_tag` + `surface_node_get_field` (these functions are added in Part D); `dict_to_ast` → a new `surface_node_from_value(v: &Value, ctx: &Arc<EvalContext>) -> Result<Arc<SurfaceNode>, MacroError>` that converts macro output back to `SurfaceNode`. Add this function to `src/surface_fields.rs`
- All `Rc::new(...)` / `Box::new(...)` at expression positions → `Arc::new(SurfaceNode { expr, span })`

---

### Part C — Lowering pass

Create `src/lower.rs`:

```rust
pub fn lower(
    node: &Arc<SurfaceNode>,
    res: &ResolutionTable,
    types: &TypeAnnotationTable,
) -> Spanned<CoreExpr>
```

**Structural lowering** for all `SurfaceExpression` variants → corresponding `CoreExpr` variants:

- Container type changes: recursive `Arc<SurfaceNode>` → `Arc<Spanned<CoreExpr>>` via recursive `lower()` calls; `Vec<Arc<SurfaceNode>>` → `Vec<Arc<Spanned<CoreExpr>>>`
- `PatternDecl`/`LetDecl` `bindings`: `Vec<Arc<SurfaceNode>>` → `Vec<Spanned<CoreExpr>>` (unwrap Arc, recurse)

**Special cases:**

- `SurfaceExpression::VarRef { name, escaped }`:
  - `if let Some(&(level, slot)) = res.get(&node_id(arc))` → `CoreExpr::Var { name, level, slot }`
  - else → `CoreExpr::FreeVar(name)` — runtime name-based env lookup (same as current `VarRef` slow path for `Some(None)` resolved)
- `SurfaceExpression::Pipe { lhs, rhs }` → `CoreExpr::Call { func: lower(rhs), args: vec![lower(lhs)], implied: true }` — pipe elimination
- `SurfaceExpression::TypeAssert { annotation, expr }`:
  - `if let Some(ty) = types.get(&node_id(arc))` → `CoreExpr::TypeAssert { annotation, expr: lower(expr), resolved_type: ty.clone() }`
  - else → `CoreExpr::RuntimeTypeCheck { annotation, expr: lower(expr), default: None }` — macro-synthesized node; evaluator performs dynamic check, falls back to `default:` annotation if present, raises error otherwise

**`lower()` handles `SurfaceExpression` only** — it takes an `Arc<SurfaceNode>` which always contains a `SurfaceExpression`. `SurfaceDeclaration` nodes are in `SurfaceDocument.items` as `SurfaceItem::Decl` and are not lowered by this function. The evaluator skips `SurfaceItem::Decl` entries (they were processed by the expander).

---

### Part D — Surface fields and match dispatch

Create `src/surface_fields.rs`:

```rust
// Tag extraction — O(1), used by match evaluator
pub fn surface_expr_tag(expr: &SurfaceExpression) -> &'static str

// Field extraction — used by AstNodeField thunk and dot-access
// Returns:
//   - Value::Expression(Arc<SurfaceNode>) for expression-typed child fields
//   - Value::Str/Bool/Int for primitive fields (name, escaped, implied, desugared)
//   - Value::Seq of Value::Expression for sequence-of-expression fields
//   - Value::Variant (matching tinct type declaration) for Annotation, DotKey,
//     Parameter, Entry, MatchArm fields
pub fn surface_node_get_field(node: &Arc<SurfaceNode>, field: &str) -> Value

// Match payload — used by match evaluator for [Program _], [Document _]
// Returns full payload dict; [Program _] wildcard discards it entirely
pub fn surface_doc_match_view(doc: &Arc<SurfaceDocument>) -> (&'static str, Value)
pub fn surface_program_match_view(prog: &Arc<SurfaceProgram>) -> (&'static str, Value)

// For macro output reconstruction (expander needs this)
pub fn surface_node_from_value(v: &Value, ctx: &Arc<EvalContext>)
    -> Result<Arc<SurfaceNode>, MacroError>
```

---

### Part E — Evaluator uses CoreExpr; old types deleted; Rc→Arc

**Delete from `src/ast.rs`:** `Expr`, `Document`, `File`, `VarRef.resolved: RefCell<...>`, `TypeAssert.resolved_type: RefCell<Option<Type>>`

**All 7 eval files must be updated** (not just `src/eval.rs`):

- `src/eval.rs` — pattern-match on `CoreExpr`; all arms updated
- `src/eval_materialize.rs` — match on new `UnevaluatedState` variants
- `src/eval_dict.rs` — `eval_dict` now uses `SurfaceDocument.items`; skips `SurfaceItem::Decl`
- `src/eval_call.rs` — update `Expr`/`NamedArg`/`Param` imports
- `src/eval_access.rs` — update `Expr` imports
- `src/eval_pipeline.rs` — **deleted** (`eval_file_with_input`, `eval_document`, `run_eval` gone)
- `src/eval_deep.rs` — **deleted** (`deep_materialize` removed)

**New `UnevaluatedState` variants:**

```rust
enum UnevaluatedState {
    Surface {  // pre-lowering; first force triggers lower() then evaluates
        node:  Arc<SurfaceNode>,
        res:   Arc<ResolutionTable>,
        types: Arc<TypeAnnotationTable>,
        env:   Arc<RwLock<Environment>>,
        ctx:   Arc<EvalContext>,
    },
    Expr { expr: Spanned<CoreExpr>, env: Arc<RwLock<Environment>>, ctx: Arc<EvalContext> },
    Builtin { ... },
    Call { ... },
    AstNodeField { node: Arc<SurfaceNode>, field: &'static str },
}
```

**`AstNodeField` evaluation** (in `src/eval_materialize.rs`):

- Takes `node` and `field`; calls `surface_node_get_field(node, field)`; returns the resulting `Value` as `Materialized`

**`FreeVar` evaluation** (in `src/eval.rs` `CoreExpr::FreeVar` arm):

- Name-based env lookup: `env.get(name)` — same as current `VarRef` slow path for `Some(None)` case

**`RuntimeTypeCheck` evaluation** (new `CoreExpr` arm):

1. Force `expr` (materialization point)
2. Validate result against `annotation` using the same structural check as existing `TypeAssert` guard path (`check_type_assert` / `Guarded` thunk mechanism in `src/eval_materialize.rs`)
3. Pass → return materialized value; Fail with `default` → return `default` as lazy thunk; Fail without default → raise `EvalError`
4. Result cached in `OnceCell`

**Update `IncludeCacheEntry::Cached`** to carry the tables:

```rust
Cached(Arc<Thunk>, Arc<ResolutionTable>, Arc<TypeAnnotationTable>)
```

The `eval` builtin retrieves `res` and `types` from the `IncludeCacheEntry` for the file that produced the `Expression` nodes being passed in `[Seq Expression]`.

**RAII drop guard** — add to every `tokio::spawn` call site (prep for async in Sprint 2):

```rust
struct ResultGuard { cell: Arc<OnceCell<Result<Value, Arc<EvalError>>>> }
impl Drop for ResultGuard {
    fn drop(&mut self) { self.cell.set(Err(EvalError::cancelled())).ok(); }
}
```

Including every `tokio::spawn` inside `eval_dict` for parallel dict evaluation.

**Rc→Arc migration** (same pass — same files already open):

- `Rc<Thunk>` → `Arc<Thunk>` throughout
- `Rc<RefCell<Environment>>` → `Arc<RwLock<Environment>>`
- `Rc<EvalConfig>` → `Arc<EvalConfig>`
- `Rc<RefCell<EvalState>>` → `Arc<Mutex<EvalState>>`
- `ThunkState` enum → `(Mutex<Option<UnevaluatedState>>, tokio::sync::OnceCell<Result<Value, Arc<EvalError>>>)` pair
- `EvalError` → `Arc<EvalError>` (was `Box`)
- `EvalConfig` gains `type_stage_env: Arc<RwLock<Environment>>` (built by `build_type_stage_env()` at startup)
- `Cargo.toml`: add `tokio` (rt-multi-thread, time, signal, sync, macros), `tokio-util`, `dashmap`

**Delete:**

- `src/ast_dict.rs` entirely
- `src/desugar.rs` — three responsibilities confirmed by code inspection: (1) Pipe→Call → `lower()`; (2) `$_` desugaring → `stdlib/desugar.llt` Sprint 3; (3) `desugar_annotation`/`desugar_param_annotation` → moved into `lower()` which already traverses annotations structurally
- `src/eval_deep.rs`
- `src/eval_pipeline.rs`

**First `cargo check` checkpoint** — after Part E. All files now use `SurfaceExpression`/`CoreExpr`. The `Expr` enum is gone.

---

### Part F — Native AST value types

Add to `src/value.rs`:

- `Value::Program(Arc<SurfaceProgram>)`
- `Value::Document(Arc<SurfaceDocument>)`
- `Value::Expression(Arc<SurfaceNode>)`

Update builtins (`src/builtins_meta.rs`):

- `load`: parse → `SurfaceProgram` → `Value::Program`
- `expand`: unwrap `Value::Program` → `&SurfaceProgram`; run expansion; wrap result
- `eval`: iterate `[Seq Expression]`; for each `Value::Expression(node)`: get `(res, types)` from `IncludeCacheEntry` for this file's program; create `UnevaluatedState::Surface { node, res, types, env, ctx }` where env = `stdlib_env + env: entries + %: binding`; return lazy thunks
- `eval-types`: same as `eval` but base env = `ctx.config.type_stage_env`
- `ast-of`: receives thunk without forcing (`Strictness::Id`); if thunk is `Surface { node, ... }` return `Value::Expression(Arc::clone(node))`; if thunk is `Materialized(Value::Builtin/Task/Channel/Context/...)` return `Value::Expression(Arc::new(SurfaceNode { expr: SurfaceExpression::Placeholder, span: Span::origin() }))`
- Delete `eval-ast` and `builtin_eval_ast`

Update `src/eval.rs` — add arms to match evaluator, dot-access evaluator, `get`, `has?`, `type-of`, `dict?`:

- **Match evaluator**: `Value::Expression(node)` → `surface_expr_tag(&node.expr)` for tag; create one `AstNodeField` thunk per pattern-bound variable
- **Dot-access**: `Value::Expression(node)` → `surface_node_get_field(node, field)`; `Value::Document` → `surface_doc_get_field`; `Value::Program` → `surface_program_get_field`
- **CRITICAL**: `Value::Variant` dot-access arm must remain unchanged and reachable — new arms must not intercept `Value::Variant` dispatch
- `type-of`: returns `"Expression"` / `"Document"` / `"Program"`
- `dict?`: returns `false` for all three

---

### Part G — Tinct type declarations and prelude update

**Order within Part G: type declarations first, then codecs.**

Add to `stdlib/prelude.llt` (AST types — must be added before codecs/json.llt):
`Expression`, `Document`, `Program`, `Parameter`, `Entry`, `Annotation`, `AnnotationEntry`, `MatchArm`, `InstanceArm`, `DotKey`, `NamedArg`, `Span`, `Declaration`, `Pattern`, `DocumentName`

Update include-decomp tinct pipeline in `stdlib/prelude.llt` (3 steps):

1. `eval-file`: change `ast@Dict` → `ast@Program` type annotation
2. `eval-document-runtime`: change `doc.name` string check → `DocumentName` match (`[match doc.name [Named n]: ... Unnamed: ...]`); `doc.expressions` now `[Seq Expression]` passed directly to `eval` builtin (was positional Dict from old schema)
3. Confirm `dict_to_file` deletion forced these changes (it was the bridge between old Dict-based eval and new Expression-based eval)

Create `stdlib/codecs/json.llt` (after type declarations above are in prelude):

- `to-json` — full tinct implementation via match dispatch on `Expression`/`Document`/`Program`
- `from-json` — Rust primitive re-exported (interim; full tinct impl blocked on `str-at`/`str-slice`/`str-length`)

Update `stdlib/cli/out/json.llt` — delegate to `codecs/json.llt`

**Test checkpoint: `just test`** — all existing tests pass. Highest-risk part: corpus tests for `load`/`expand`/`eval` may fail due to type changes; fix as found.

---

## Sprint 2 — Async: Evaluation + All Primitives

**Goal:** `eval`/`materialize` become `async fn`. All async builtins working. `just test` passes.

### Part A — Async evaluation core

**`eval`, `materialize` → `async fn`:**

- Every recursive call site gains `.await`
- `eval_dict` fans out independent entries via `tokio::task::JoinSet`; each entry's thunk wrapped in `ResultGuard` (including eval_dict spawns — not just the `task` builtin)

**Deadlock detection:**

- Task-local `HashSet<*const Thunk>` (intra-task, fast path) — check before entering `result.get_or_init().await`; if thunk is in set, raise `EvalError::Cycle`
- Process-global `WAIT_FOR: DashMap<tokio::task::Id, usize>` where value is `Arc::as_ptr() as usize` (NOT `*const Thunk` — raw pointers are `!Send`, use `usize` cast instead); DFS cycle check from current task before suspending; remove entry on exit (either result received, cycle detected, or cancelled)

**Current `BuiltinFn` type** (confirmed by code inspection of `src/value.rs:29-39`):

```rust
pub type BuiltinFn = fn(BuiltinArgs) -> EvalResult<Rc<Thunk>>;
// where BuiltinArgs has: args: &[Rc<Thunk>], named: Option<&IndexMap<String, Rc<Thunk>>>,
//                        call_span: Span, ctx: Rc<EvalContext>
```

**New `BuiltinFn` type after Sprint 2A** (args are owned — moved into future to satisfy `'static` bound for `tokio::spawn`):

```rust
type BuiltinFn = fn(BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send + 'static>>;

struct BuiltinArgs {
    args:       Vec<Arc<Thunk>>,
    named:      Option<IndexMap<String, Arc<Thunk>>>,  // Option preserved from current
    call_span:  Span,
    ctx:        Arc<EvalContext>,
    // Note: depth removed — tracked via EvalContext or call stack instead
}
```

Confirmed: `~190 builtins` (not ~180 — test at `src/builtins.rs:6652` asserts `count == 190`).
All ~180 builtins gain async wrapper; I/O builtins replace `block_on(fut)` with `fut.await`.

**`async_rt.rs`:** `run_program(fut)` with multi-thread Tokio runtime; `spawn_task(fut)` for Arc-based tasks; `block_on` bridge removed.

**LSP** (`src/lsp/`): Uses synchronous `lsp-server` library; analysis functions become `async fn`; the synchronous message dispatch loop retains `block_on(analysis_fn(...))` at the outermost call boundary only.

**`EvalContext` gains `cancel: CancellationToken`** (tokio_util).

**Test suite:** All tests → `#[tokio::test(flavor = "current_thread")]`; `run_eval(source)` wraps in `run_program(...)`.

**`Cargo.toml`:** add `notify`, `num_cpus`; remove `hyper`, `reqwest`.

### Part B — Async primitives

Register in `standard_builtins()` (implement in `src/builtins_async.rs`):

`task`, `await`, `await-any`, `channel`, `send`, `recv`, `select-once`, `par`, `context`, `with-cancel`, `with-timeout`, `with-deadline`, `cancelled?`, `with-context`, `timeout`, `cancel-task`, `cancel-root`, `drain`, `exit-now`, `signal-channel`, `timer-channel`, `watch-channel`

Add to `src/value.rs`:

- `Value::Task(Arc<Mutex<TaskState>>)`
- `Value::Channel(Arc<ChannelInner>)`
- `Value::Context(tokio_util::sync::CancellationToken)`

Add to `src/type_def.rs` with full handling (unify, apply, occurs_in, collect_type_vars, display):

- `Type::Task(Box<Type>)` — follows `Type::Seq` pattern exactly
- `Type::Channel(Box<Type>)` — follows `Type::Seq` pattern exactly
- `Type::Context` — opaque; no type parameter

Inference rules in `src/typecheck.rs`:

- `task` body type → `Task(body_type)`
- `await` unifies `Task(?t)` → `?t`
- `send`/`recv` unify channel element type
- `select-once` sources `[Seq [SelectSource t r]]` → `t` and `r` are fresh `TypeVar`s instantiated per call site (not `Unknown`)

Add to `stdlib/prelude.llt` (after AST types from Sprint 1G):
`Signal`, `Action`, `CancelHandle`, `SelectSource`, `Context` (opaque — registered as primitive type, no tinct declaration)

Add `stdlib/async.llt`:

- `cancel: [fn [c@CancelHandle] [c.cancel]]`
- `await-all` (channels + `with-cancel` + `recv-all` via `reduce` — see whatif for full implementation; uses `collect` to force spawning)
- `recv-all` uses `reduce` over `[range 0 n]` (Rust builtin loop — no tinct recursion depth consumed)
- `par-map`, `par-filter`, `exit`, `graceful-exit`, `finally`, `loop-select`, `retry`

**Test corpus** (sprint runner writes these):

1. Single task spawn + await
2. Channel send/recv ordering
3. `await-all` with one failing task — verify cancellation of siblings
4. `cancel-task` with a waiter — verify `Err(Cancelled)` propagation (not hang)
5. Cross-task cycle detection — verify `EvalError::Cycle` (not deadlock)

**Test checkpoint: `just test` + new corpus tests.** First sprint where async programs work end-to-end.

---

## Sprint 3 — Stdlib and Cleanup

**Goal:** All stdlib migrated. `$_` desugaring in tinct. Full suite green.

### `$_` desugaring surface pass

Create `stdlib/desugar.llt`:

- `desugar-program: [fn [p@Program] ...]` — walks `Expression` tree via match dispatch; finds `[Var name: "_" escaped: true]` in non-parameter positions; wraps containing expression in `[Fn params: [[Parameter name: "_" ...]] body: ... desugared: true ...]`; does NOT recurse into `Quote` nodes
- Full match-dispatch traversal similar to `json-expression` — one arm per `Expression` variant

**Register in `src/main.rs`:**

1. After `expand()` returns `Value::Program`, look up `desugar-program` from `stdlib_env`: `let desugar = stdlib_env.get("desugar-program").expect("prelude must define desugar-program")`
2. Call it via `invoke_function(&desugar, &[program_value], &ctx)` and materialize the result
3. Unwrap returned `Value::Program` back to `Arc<SurfaceProgram>` before passing to resolver
4. Remove calls to `desugar_file()` at ALL Rust call sites (confirmed by code inspection): `src/lib.rs`, `src/main.rs`, `src/builtins.rs`, `src/builtins_meta.rs`, `src/formatter.rs` (search for `desugar_file` or `desugar::`)

This is a Rust-calls-tinct boundary crossing. The mechanism (`invoke_function`) already exists from the `cli-pipeline` call pattern established in include-decomp.

### Remaining cleanup

- `stdlib/codecs/json.llt` — `from-json` tinct implementation already written; activates once `str-at`/`str-slice`/`str-length` are registered (Sprint 3 or separate sprint per TODO `strings-char-access`)
- Audit `stdlib/strings.llt` — replace any direct Rust built-in calls with prelude wrappers
- Confirm deleted: `Value::RustRegistry`, `rust_module()`, `builtin-*` aliases, `EvalState::include_guard`, old `EvalState::include_cache`, `src/eval_deep.rs`, `src/eval_pipeline.rs`, `src/desugar.rs`, `src/ast_dict.rs`
- Scan for `use std::rc::Rc` in any `src/` file — should be zero after Sprint 1E migration
- LSP `block_on` removal once LSP event loop is fully async
- Run `/review-whatif runtime-v2`

**Test checkpoint: `just test` full suite green. `just docgen` runs. Manual smoke test of async programs.**

---

## Dependency Graph

```text
exhaustiveness-multi-field-nominal  (touches type_def.rs)
    └─► runtime-v2-type-prereqs     (touches type_def.rs + typecheck.rs)
            └─► include-decomp chain (expand: Dict→Dict, eval: Dict→Any)
                    │
                    ▼
            Sprint 1 — Foundation
            (Parts A–E atomic; cargo check after E; just test after G)
                    │
                    ▼
            Sprint 2 — Async
            (eval async fn + all primitives; just test at end)
                    │
                    ▼
            Sprint 3 — Stdlib + Cleanup
            (desugar.llt, from-json, full suite green)
```

---

## Risk Register

| Risk | Sprint | Mitigation |
|------|--------|-----------|
| Many corpus tests break on load/expand/eval type changes | 1G | Fix tests as found; include-decomp corpus tests are the reference |
| Parser migration misses a `Rc::new`/`Box::new` call | 1B | `cargo check` catches all after Part E; scan for `Rc::new(Spanned` specifically |
| `lower()` missing a SurfaceExpression variant | 1C | Match is exhaustive — compiler will catch at compile time |
| `eval_dict.rs` / `eval_call.rs` etc. missed in evaluator migration | 1E | `cargo check` catches; explicitly enumerated above |
| `*const Thunk` `!Send` in DashMap | 2A | Use `usize` (pointer cast); documented above |
| `await-all` deadlock if `collect` missing | 2B | Follow whatif impl exactly; `recv-all` uses `reduce` not recursion |
| `desugar-program` call requires tinct-in-Rust mechanism | 3 | Use `invoke_function` pattern from `cli-pipeline` |
| `$_` desugaring misses an Expression variant | 3 | `match` on `Expression` is exhaustive at the type level |
