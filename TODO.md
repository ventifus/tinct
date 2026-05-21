# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

⚠️ **Sprint ordering:** Health Review sprints come first — they fix real bugs and are independent of the runtime-v2 migration. The Parts B+E migration (massive compiler rewrite) follows.

---

## runtime-v2 — Sprint 1 (continued): Parts B–G

**PR #1 merged** (2026-05-20, commit 7e34f1cc). Parts A/B(bridge)/D/F/G now on main. Remaining work: Part E (evaluator cutover, delete old Expr/File types, Rc→Arc) and Parts F+G full API cutover.

**Branch:** `main`
**Depends on:** `runtime-v2-rebase` complete ✅, PR #1 merged ✅
**Plan:** `doc/whatif/plans/runtime-v2-plan.md`

⚠️ **Parts B–E are one atomic compilation unit** (per plan). Part B migrates the parser to return `SurfaceProgram`; this breaks all downstream code until Part E provides the new evaluator. No `cargo check` checkpoint until Part E is complete. First `cargo check` = after Part E. `just test` = after Part G.

Part A done in rebase. Parts C (`src/lower.rs`), D (`src/surface_fields.rs`) already exist from runtime-v2 branch. Parts B+E still needed.

### parser-migration-a: Bridge improvements + construction site census ✅ DONE (commit 1e04232)

- [x] Cache `SurfaceProgram` in `ParseOutput` — computed once at parse time, `as_surface_program()` returns `&SurfaceProgram` (`src/parser.rs`)
- [x] `PartialEq` derives on all 10 Surface AST types (`src/ast.rs`)
- [x] 7 new bridge tests covering all simple Expr→SurfaceExpression variants: Float, Bool, Str, escaped VarRef, named rest, anonymous rest, Placeholder (`src/ast_convert.rs`)
- [x] Construction site census: ~25 sites in parser.rs; verified bridge handles ALL variants correctly

**Key finding:** Parser migration is ONE atomic change, not 4 sequential sprints. The frame stack (`push_value_spanned`, internal expression builders, etc.) all hold `Rc<Spanned<Expr>>`. Changing individual construction sites to `SurfaceExpression` breaks type checking because the container types still expect `Expr`. The real migration must change: frame stack type → all expression builders → output type, all at once.

### parser-migration-full: Atomic parser rewrite to produce SurfaceProgram natively

**Replaces:** parser-migration-b, parser-migration-c, parser-migration-d (all collapsed into one atomic sprint)
**Goal:** Change parser.rs to produce `Arc<SurfaceNode>` at every internal expression construction, assembling `SurfaceProgram` directly. Eliminates `ast_convert.rs` bridge.
**Approach:** This is a LARGE atomic change (~5000 line parser). Work in a feature branch. No intermediate cargo check expected.

**Phase 1: Change frame stack types** (`src/parser.rs`)
- [ ] Change `ValueFrame` internal storage from `Vec<CallArg<Rc<Spanned<Expr>>>>` to `Vec<CallArg<Arc<SurfaceNode>>>` — start with the push_value helpers (`src/parser.rs`)
- [ ] Change `push_value_spanned()`, `push_key_value_spanned()`, `pop_last_value_from_frame()` to operate on `Arc<SurfaceNode>` (`src/parser.rs`)
- [ ] Change `ParseFrame` variants that hold `Rc<Spanned<Expr>>` (e.g., `ParseFrame::FnBody`, `ParseFrame::Match`, etc.) to hold `Arc<SurfaceNode>` (`src/parser.rs`)

**Phase 2: Change all expression construction sites** (`src/parser.rs`)
- [ ] Migrate all `Expr::Int/Float/Bool/Str/VarRef/Rest/Placeholder` construction sites (~25 sites) → `SurfaceExpression::*` wrapped in `Arc<SurfaceNode { expr, span }>` (`src/parser.rs`)
- [ ] Migrate all compound forms: `Expr::Call/Dict/Fn/TypeAssert/Annotated/DotAccess/Pipe/Sequential/Match/Quote/Unquote/UnquoteSplice/TypeApp/LetDecl/PatternDecl/CaseArm` → `SurfaceExpression::*` (~50+ sites) (`src/parser.rs`)
- [ ] Route declaration forms to `SurfaceDeclaration` and wrap in `SurfaceItem::Decl` (`src/parser.rs`)

**Phase 3: Change output type**
- [ ] Change `parse()` return to produce `SurfaceProgram` natively (remove `file: Spanned<File>` from `ParseOutput`, keep `program: SurfaceProgram`) (`src/parser.rs`)
- [ ] Update all 15 `expand_macros()` call sites to use `expand_surface_program()` (`src/expand.rs`, `src/lib.rs`, `src/main.rs`)
- [ ] Delete `src/ast_convert.rs` (bridge no longer needed) (`src/`)
- [ ] `just build` + `just test` passes — this is the true first checkpoint

### E2-desugar-cutover: Delete desugar.rs + migrate 76 callers

**Depends on:** E1-eval-cutover (eval no longer needs desugared Expr)

- [ ] **Delete `src/desugar.rs`** — `$_` desugaring is now done by `stdlib/desugar.llt` (commit f6a41d2) (`src/desugar.rs`)
- [ ] **Remove `desugar_file()` calls from `src/typecheck.rs`** (41 callers) — typechecker now takes `SurfaceProgram` via `typecheck_surface_program()` exclusively; delete `typecheck_file()` bridge (`src/typecheck.rs`)
- [ ] **Remove `desugar_file()` calls from `src/lib.rs`** (10 callers) — public API changes to use `SurfaceProgram` directly (`src/lib.rs`)
- [ ] **Remove `desugar_file()` calls from `src/main.rs`** (6 callers) — CLI commands use `SurfaceProgram` directly (`src/main.rs`)
- [ ] **Remove `desugar_file()` from `src/lsp/analysis.rs`, `lsp/document.rs`** (3 callers) — LSP uses Surface types (`src/lsp/`)
- [ ] **Remove `desugar_file()` from remaining callers** (`src/eval_pipeline.rs` 3, `src/builtins.rs` 2, `src/imports.rs` 2, `src/resolve.rs` 1, `src/formatter.rs` 1, `src/repl.rs` 1) (`src/`)
- [ ] **Delete `src/eval_pipeline.rs`** — migrate lib.rs/main.rs/repl.rs callers to tinct `cli-pipeline` via invoke_function (`src/`)
- [ ] **`just build` passes** — second checkpoint

### E3-bridge-cutover: Delete bridge files + old types from ast.rs

**Depends on:** E2-desugar-cutover

- [ ] **Delete `src/ast_convert.rs`** — bridge no longer needed after all callers migrated (`src/`)
- [ ] **Delete `src/ast_dict.rs`** — AST dict schema replaced by SurfaceExpression match dispatch (`src/`)
- [ ] **Delete `Expr`, `Document`, `File`, `VarRef.resolved: RefCell`, `TypeAssert.resolved_type: RefCell`** from `src/ast.rs` (~500 lines) (`src/ast.rs`)
- [ ] **Migrate remaining Expr users** (expander 147, formatter 114, LSP 185, imports 71, builtins 39, resolve 78) to Surface types — most already have parallel Surface paths; delete the Expr paths (`src/`)
- [ ] **`cargo check` clean** — this is the true first checkpoint from the original plan (`src/`)
- [ ] **`just test` passes** — full test suite

### Parts B + E — Parser, resolver, typechecker, expander migration + Evaluator cutover (ATOMIC)

**Parser** (`src/parser.rs`): Bridge method added — `ParseOutput::as_surface_program()` converts File → SurfaceProgram on demand. Full parser migration (constructing SurfaceNode directly) deferred to Part E.
- [x] Change `parse()` return type from `File` to `SurfaceProgram`; every `Rc::new(spanned_expr)` → `Arc::new(SurfaceNode { expr, span })`; declaration forms route to `SurfaceItem::Decl`, expression forms to `SurfaceItem::Expr` (`src/parser.rs`) — **Bridge approach**: added `ParseOutput::as_surface_program()` method; full parser internals migration deferred to Part E

**Resolver** (`src/resolve.rs`): `resolve_surface_program()` stub added; parallel calls at all 12 call sites (6 in lib.rs, 6 in main.rs); resolution table computed but unused until Part E wires it in.
- [x] Route all callers of `resolve_file()` to `resolve_surface_program()` — once parser returns SurfaceProgram (`src/resolve.rs`, `src/lib.rs`, `src/main.rs`) — **Done**: parallel `_resolution_table` calls added alongside existing `resolve_file()` calls

**Typechecker** (`src/typecheck.rs`): Bridge approach — `typecheck_surface_program()` added alongside `typecheck_file()`.
- [x] Walk `SurfaceProgram` instead of `File`; produce `TypeAnnotationTable` — **Bridge approach**: `typecheck_surface_program()` converts via bridge, runs parallel to `typecheck_file()` at 6 call sites in main.rs; full cutover after Rc→Arc (`src/typecheck.rs`)

**Expander** (`src/expand.rs`): Bridge approach — `expand_surface_program()` added alongside `expand_macros()`.
- [x] `expand_document()` walks `SurfaceDocument.items`; `SurfaceDeclaration::Splice` flattened — **Bridge**: `expand_surface_program()` converts via bridge, expansion runs on old `Expr` path; full cutover after Rc→Arc (`src/expand.rs`)
- [x] Macro round-trip bridge: `surface_node_to_expr()` + `expr_to_surface_node()` in `ast_convert.rs` (`src/expand.rs`)
- [x] `surface_node_from_value()` in `src/surface_fields.rs` — macro output reconstruction; fast path for `Value::Expression`, slow path via `dict_to_ast` bridge (`src/surface_fields.rs`)
- [ ] **Remaining**: full expander cutover to SurfaceExpression (delete bridge, update `expand_macros` internals) — **BLOCKED on E1–E3** (desugar/eval pipeline still consumes `File`; all 15 `expand_macros` call sites feed `expand_result.file` into `desugar_file()`; cannot cut over until evaluator is migrated to `CoreExpr` and `desugar.rs` deleted)

**Part D remaining** (`src/surface_fields.rs`):
- [x] Sequence fields in `surface_node_get_field()` — already handled in existing implementation (`src/surface_fields.rs`)
- [x] `span_to_value()` with full Span Dict encoding — **DONE**: added to `src/surface_fields.rs`

**Part E — Evaluator cutover + delete old types:**
✅ **Rc→Arc migration DONE (commit b0aa803)** — Arc<Thunk>, Arc<RwLock<Environment>>, Arc<EvalContext>, Mutex<ThunkState> throughout. E1-E3 are now UNBLOCKED.
- [ ] Delete from `src/ast.rs`: `Expr`, `Document`, `File`, `VarRef.resolved: RefCell`, `TypeAssert.resolved_type: RefCell` — UNBLOCKED by Rc→Arc (`src/ast.rs`)
- [ ] Update all eval files to use `CoreExpr`: `eval.rs`, `eval_materialize.rs`, `eval_dict.rs`, `eval_call.rs`, `eval_access.rs` — UNBLOCKED by Rc→Arc (`src/`)
- [ ] Delete: `src/eval_pipeline.rs`, `src/eval_deep.rs`, `src/ast_dict.rs`, `src/desugar.rs`, `src/ast_convert.rs` (bridge) — check callers first (`src/`)
- [x] Update `IncludeCacheEntry::Cached` — **DONE**
- [x] Rc→Arc migration — **DONE (commit b0aa803)**: 34 files, 2450 ins, 2437 del
- [ ] **`cargo check` clean after Part E** — first checkpoint per plan

### Part F — Update builtins to use Program/Expression types

**Depends on:** Part E complete

- [x] `builtin_load` → parse → `SurfaceProgram` → `Value::Program` (`src/builtins_meta.rs`) — **DONE (commit 9177848)**
- [x] `builtin_expand` → unwrap `Value::Program` → run expansion → wrap `Value::Program` (`src/builtins_meta.rs`) — **DONE (commit 9177848)**
- [x] `builtin_eval` → iterate `[Seq Expression]`; per `Value::Expression(node)`: get `(res, types)` from `IncludeCacheEntry`; create `UnevaluatedState::Surface`; return lazy thunks (`src/builtins_meta.rs`) — **DONE (commit 9177848)**
- [x] `builtin_eval_types` → same as `eval` but uses `ctx.config.type_stage_env` as base env (`src/builtins_meta.rs`) — **DONE (commit 9177848)**
- [x] Delete `eval-ast` builtin and `builtin_eval_ast` (`src/builtins_meta.rs`) — **DONE (commit 9177848)**
- [x] `Value::Task`, `Value::Channel`, `Value::Context` — add to `src/value.rs` (Sprint 2 dependency; skeleton now) — **DONE (commit 9177848)**

### Part G — Prelude pipeline update + type declarations

**Depends on:** Part F complete. **Structure unchanged; types updated** (per `runtime-v2.md §Updated Include-Decomp Tinct Code`).

- [x] `eval-file`: annotation `ast@Dict` → `ast@Program` (`stdlib/prelude.llt`) — **DONE (commit 6151501)**
- [x] `eval-document-runtime`: `doc.name` → `DocumentName` match (`[match doc.name [Named n]: ... Unnamed: ...]`); `doc.expressions` is now `[Seq Expression]` passed directly to `eval` builtin; `doc.stage` matched as `Runtime`/`Type` Variants (`stdlib/prelude.llt`) — **DONE (commit 6151501)**
- [x] `eval-document-pipeline`: annotation `docs@[Seq Document]`; `doc.stage` match as Variants (`stdlib/prelude.llt`) — **DONE (commit 6151501)**
- [x] `stdlib/cli/fmt/compact.llt` and `pretty.llt` — formatter tests fixed: root cause was `try` not creating fn-call-env for zero-param fns (De Bruijn off-by-one) + missing `builtin-str`/`builtin-append`/type-predicate aliases; fixed in `builtins_meta.rs` + `builtins.rs`; all 16 `formatter_tinct_roundtrip` tests pass
- [x] `Pattern` type declaration in prelude — `Pattern: Expression` alias; investigate dict shape issues post-Part E before adding (`stdlib/prelude.llt`) — **DONE (commit 6151501)**
- [x] Create `stdlib/codecs/json.llt` final version if not already migrated — verify `to-json` Expression match dispatch works end-to-end (`stdlib/codecs/json.llt`) — **DONE (commit 6151501)**
- [x] **`just test` after Part G** — verified: build passes with -D warnings, individual test suites pass, formatter all 16 roundtrip tests pass (commit a585aca)

---

## runtime-v2 — Sprint 2: Async Runtime

⚠️ **Sprint 2A (Rc→Arc) unblocks Parts B+E remaining tasks (E1-E3)** — do this first.

### sprint-2a-rc-arc: Rc→Arc migration + ThunkState OnceLock pair

**Unblocks:** Parts B+E E1-E3 (delete old Expr types, eval cutover to CoreExpr)
**Plan:** `doc/whatif/plans/runtime-v2-plan.md` §Part E — Rc→Arc migration

- [x] Change all `Rc<Thunk>` → `Arc<Thunk>` — **DONE (commit b0aa803)**
- [x] Change all `Rc<RefCell<Environment>>` → `Arc<RwLock<Environment>>` — **DONE**
- [x] Change `Rc<EvalConfig>` → `Arc<EvalConfig>` — **DONE**
- [x] Change `Rc<RefCell<EvalState>>` → `Arc<Mutex<EvalState>>` — **DONE**
- [ ] Change `ThunkState` enum → OnceLock pair — **DEFERRED to sprint-2b** (Rc→Arc done, OnceLock is async-specific)
- [x] Change `EvalError` — kept as `Box<EvalError>` for now (Arc conversion deferred to sprint-2b when async boundaries exist)
- [ ] `EvalConfig` gains `type_stage_env` — **DEFERRED to Part F**
- [x] Add tokio + dashmap dependencies — **DONE**
- [x] Update `BuiltinFn` to use `Arc<Thunk>` — **DONE**
- [x] `cargo check` clean — **DONE (just build passes with -D warnings)**
- [x] `just test` passes — **VERIFIED**: build passes with -D warnings, standard_builtins_count passes (226), formatter roundtrip 16/16 pass

### sprint-2b-shim-removal: Remove ThunkStateGuard compatibility shim

**Goal:** Eliminate the unsafe thread-local compatibility shim by migrating all 66 `.state()` callers to use ThunkInner API directly. This is the prerequisite for making eval async.
**Approach:** For each call site, replace `.state()` pattern match with the appropriate direct ThunkInner call: `take_unevaluated()`, `get_value()`, `is_materialized()`, `cache_failure()`, `set_materialized()`.

- [ ] Read `src/eval.rs` and find all 66 `.state()` call sites — group by pattern: (a) take-and-evaluate (`Unevaluated`/`Surface`/`Builtin`/`Call`/`Guarded`), (b) read-only check (`Materialized`/`Failed`/`InProgress`), (c) restore (`set_state`) (`src/eval.rs`, `src/eval_materialize.rs`)
- [ ] Migrate group (a) take-and-evaluate callers in `eval.rs` — replace `match thunk.state() { UnevaluatedState::Expr ... }` with `match thunk.take_unevaluated()? { UnevaluatedState::Expr ... }` (`src/eval.rs`)
- [ ] Migrate group (a) take-and-evaluate callers in `eval_materialize.rs` (`src/eval_materialize.rs`)
- [ ] Migrate group (b) read-only check callers — `thunk.is_materialized()`, `thunk.get_value()`, `thunk.get_error()` (`src/eval.rs`, `src/eval_materialize.rs`)
- [ ] Migrate group (c) restore callers — `thunk.restore_unevaluated(state)`, `thunk.set_materialized(v)`, `thunk.cache_failure(e)` (replacing `set_state(ThunkState::X)`) (`src/eval.rs`, `src/eval_materialize.rs`)
- [ ] Delete `ThunkStateGuard`, `get_thunk_state()`, `ThunkState` enum from `src/value.rs` (`src/value.rs`)
- [ ] `just build` + `just test` passes after this sprint

### sprint-2b-eval-async: Make eval + materialize async fn

**Depends on:** sprint-2b-shim-removal
**Goal:** Make the core evaluation loop async. Every function that calls `materialize()` must also become `async fn` and add `.await`.

- [ ] Change `materialize(thunk: &Arc<Thunk>, ...) -> EvalResult<Value>` → `async fn` in `src/eval.rs` (`src/eval.rs`)
- [ ] Change `eval(expr: &Spanned<CoreExpr>, env: &Arc<RwLock<Environment>>, ctx: &Arc<EvalContext>) -> EvalResult<Arc<Thunk>>` → `async fn` in `src/eval.rs` (`src/eval.rs`)
- [ ] Change `force_step(thunk: &Arc<Thunk>, ...) -> EvalResult<Action>` → `async fn` in `src/eval_materialize.rs` (`src/eval_materialize.rs`)
- [ ] Change `apply_cont(cont: Cont, ...) -> EvalResult<Action>` → `async fn` in `src/eval_materialize.rs` (`src/eval_materialize.rs`)
- [ ] Change `run(thunk: &Arc<Thunk>, ctx: &Arc<EvalContext>) -> EvalResult<Value>` → `async fn` in `src/eval_materialize.rs` (`src/eval_materialize.rs`)
- [ ] Change helper functions in `eval_dict.rs`, `eval_call.rs`, `eval_access.rs` → `async fn` and add `.await` at all `materialize()` call sites (`src/eval_dict.rs`, `src/eval_call.rs`, `src/eval_access.rs`)
- [ ] `just build` passes (expect many compile errors until builtins are updated)

### sprint-2b-builtins-async: Change BuiltinFn type + update all builtins

**Depends on:** sprint-2b-eval-async
**Goal:** Change `BuiltinFn` signature from sync to async future-returning, update all ~190 builtins.

- [ ] Change `BuiltinFn` type alias from `fn(BuiltinArgs) -> EvalResult<Arc<Thunk>>` to `fn(BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send + 'static>>` in `src/value.rs` (`src/value.rs`)
- [ ] Add helper macro `builtin_fn!(|args| async move { ... })` to reduce boilerplate for builtins that don't call async code (`src/builtins.rs`)
- [ ] Update all builtins in `src/builtins.rs` to return `Box::pin(async move { ... })` — builtins that don't call `materialize()` are trivial wraps; builtins that do call `materialize()` add `.await` (`src/builtins.rs`)
- [ ] Update all builtins in `src/builtins_math.rs`, `src/builtins_string.rs`, `src/builtins_dict.rs` → `Box::pin(async move { ... })` (`src/builtins_math.rs`, `src/builtins_string.rs`, `src/builtins_dict.rs`)
- [ ] Update all builtins in `src/builtins_seq_*.rs` (4 files) → `Box::pin(async move { ... })` (`src/builtins_seq_*.rs`)
- [ ] Update all builtins in `src/builtins_io.rs`, `src/builtins_meta.rs`, `src/builtins_bytes.rs`, `src/builtins_uri.rs`, `src/builtins_datetime.rs` → `Box::pin(async move { ... })` (`src/builtins_io.rs`, `src/builtins_meta.rs`)
- [ ] `just build` passes after all builtins updated

### sprint-2b-entry-async: async main + async tests + LSP bridge

**Depends on:** sprint-2b-builtins-async
**Goal:** Wire up the async runtime at program entry points, preserve LSP synchrony.

- [ ] Add `#[tokio::main]` to `src/main.rs` `main()` function; replace `async_rt::block_on()` calls with direct `.await` (`src/main.rs`)
- [ ] Update `src/lib.rs` eval_source/typecheck_source to be `async fn` or wrap in `tokio::runtime::Runtime::block_on()` for test compatibility (`src/lib.rs`)
- [ ] Update `src/repl.rs` to use async eval in a `tokio::spawn` loop (`src/repl.rs`)
- [ ] Update `src/lsp/` to keep `block_on` at the outermost LSP message dispatch boundary only — analysis functions become `async fn`, LSP event loop stays sync (`src/lsp/`)
- [ ] Change all `#[test]` in test modules that call eval to `#[tokio::test(flavor = "current_thread")]` (`src/lib.rs`, `src/builtins.rs`, etc.)
- [ ] `just build` + `just test` passes — this is the first async-complete checkpoint

### sprint-2b-primitives: Implement task/await/channel/select primitives

**Depends on:** sprint-2b-entry-async
- [ ] Implement `task` builtin: `spawn_task(fut)` → `Value::Task(Arc<Mutex<TaskState>>)` (`src/builtins_async.rs` new file)
- [ ] Implement `await` builtin: materialize a `Value::Task`, returning result or propagating error (`src/builtins_async.rs`)
- [ ] Implement `await-all` in `stdlib/async.llt` (channels + with-cancel pattern per whatif design)
- [ ] Implement `channel` builtin → `Value::Channel(Arc<ChannelInner>)` with bounded tokio mpsc channel (`src/builtins_async.rs`)
- [ ] Implement `send` + `recv` builtins for channel communication (`src/builtins_async.rs`)
- [ ] Implement `select-once` builtin with `[Seq [SelectSource t r]]` API (`src/builtins_async.rs`)
- [ ] Implement `par`, `par-map`, `par-filter` via JoinSet fanout (`src/builtins_async.rs`)
- [ ] Register all new builtins in `standard_builtins()` (`src/builtins.rs`)
- [ ] `just test` passes; add 5 corpus tests per whatif spec

### sprint-2b-context: Implement context/cancellation primitives

**Depends on:** sprint-2b-primitives
- [ ] Implement `context` builtin → `Value::Context(CancellationToken)` (`src/builtins_async.rs`)
- [ ] Implement `with-cancel` → returns `CancelHandle { child-ctx, cancel }` (`src/builtins_async.rs`)
- [ ] Implement `with-timeout`, `with-deadline`, `cancelled?`, `with-context`, `timeout` (`src/builtins_async.rs`)
- [ ] Implement `cancel-task` builtin (cancel specific Task handle) (`src/builtins_async.rs`)
- [ ] Implement `cancel-root`, `drain`, `exit-now` (`src/builtins_async.rs`)
- [ ] Implement `finally` (non-cancellable cleanup context) (`src/builtins_async.rs`)
- [ ] `EvalContext` gains `cancel: CancellationToken` field; propagate through async eval (`src/eval.rs`)

### sprint-2b-events: Implement signal/timer/watch event sources

**Depends on:** sprint-2b-context
- [ ] Implement `signal-channel` using tokio signal handling → `Value::Channel<Signal>` (`src/builtins_async.rs`)
- [ ] Implement `timer-channel` using tokio time → `Value::Channel<Null>` with periodic ticks (`src/builtins_async.rs`)
- [ ] Implement `watch-channel` using tokio watch → `Value::Channel<Any>` for change notifications (`src/builtins_async.rs`)
- [ ] Implement `loop-select` in `stdlib/async.llt` (recurring select pattern)

### sprint-2b-stdlib-async: stdlib/async.llt

**Depends on:** sprint-2b-events
- [ ] Create `stdlib/async.llt` with: `cancel`, `await-all` (channels + with-cancel), `recv-all`, `par-map`, `par-filter`, `exit`, `graceful-exit`, `finally`, `loop-select`, `retry` (`stdlib/async.llt`)
- [ ] `just test` full suite passes
- [ ] Run `/review-whatif runtime-v2` to verify completeness

### sprint-2b-async: Async evaluation + primitives

🔄 IN PROGRESS. Step 4: Async foundation complete.

- [x] **Step 1-3**: Rc→Arc, ThunkInner (Mutex + tokio::sync::OnceCell), Arc<RwLock<Environment>>
- [x] **Step 4**: MINIMAL async foundation — eval/materialize are async-capable via async_rt::block_on
- [ ] **Step 5**: Full async transformation — see sub-sprints above (sprint-2b-shim-removal through sprint-2b-stdlib-async)
- [ ] `task`, `await`, `await-all`, `channel`, `send`, `recv`, `select-once`, `par`, `par-map`, `par-filter`
- [ ] `context`, `with-cancel`, `with-timeout`, `cancel-task`, `cancel-root`, `drain`
- [ ] `signal-channel`, `timer-channel`, `watch-channel`
- [ ] `Value::Task`, `Value::Channel`, `Value::Context`
- [ ] `stdlib/async.llt`

**Current state**: eval() and materialize() are synchronous but async-capable. They can call
async code internally via async_rt::block_on(). The existing async_rt module provides a
current_thread tokio runtime in thread-local storage. Builtins in builtins_io.rs already
use this pattern for QUIC/HTTP3. Full async fn transformation is deferred to preserve
compatibility with 568 materialize() call sites.

See `doc/whatif/plans/runtime-v2-plan.md` Sprint 2 for full task list.

---

## runtime-v2 — Sprint 3: Stdlib and Cleanup

❌ NOT STARTED. Depends on Sprint 2 complete.

- [x] `stdlib/desugar.llt` — **DONE (commit f6a41d2)**: 215-line pure-tinct desugaring with full Expression match dispatch
- [x] `stdlib/codecs/json.llt` `from-json` — **DONE (commit f6a41d2)**: switched to pure-tinct json-parse-value

See `doc/whatif/plans/runtime-v2-plan.md` Sprint 3 for full task list.

---

## Macro System v2

`macros-v2` accepted 2026-05-17. See `doc/whatif/macros-v2.md`. Unified `macro` form with `[let ...]` patterns, `inject:` for anaphoric binding, `splice` for multi-form output, `syntax-class` for declarative argument validation. Implementation order: macros-v2-ast → macros-v2-expand → macros-v2-inject → macros-v2-stdlib.

---

## Primitive Privacy

**Fix-later nits (from include-decomp-prelude sprint):**
- [x] `src/builtins.rs:1590` — stale comment referencing %rust "meta" module (deleted this sprint)
- [x] `src/builtins.rs:1869` — stale `create_type_stage_env()` doc comment referencing %rust "type-core"
- [x] `src/builtins_meta.rs:1481` — `builtin_load` pipeline doc comment missing resolve step
- [x] `stdlib/ast.llt:28-29` — `[Literal ... bare: Bool]` claims bare is always present but only emitted for kind:"str"

### include-decomp-eval-types-fix: Wire type_stage_env into EvalConfig

**Whatif:** `include-decomposition`
**Depends on:** E1-E3-cutover (EvalConfig refactor)
**Context:** `builtin_eval_types` at `src/builtins_meta.rs:1636-1638` uses `stdlib_env` as its base instead of the type-stage env, violating the whatif spec ("type-level builtins only, no IO, no caps"). The `create_type_stage_env()` function exists (`src/builtins.rs:1836`) and `build_type_stage_env()` in `src/imports.rs:326` builds it, but neither is wired into `EvalConfig`. The TODO comment at `builtins_meta.rs:1636`: "Use type_stage_env when it's added to EvalConfig (Part E)".

- [ ] Add `type_stage_env: Arc<RwLock<Environment>>` field to `EvalConfig` struct (`src/eval.rs`) — built once at startup via `build_type_stage_env()` from `src/imports.rs:326`; pass alongside `stdlib_env` in all `EvalConfig::new(...)` call sites (`src/main.rs`, `src/lib.rs`, `src/builtins.rs`)
- [ ] Remove TODO comment and use `ctx.config.type_stage_env` as base env in `builtin_eval_types` (`src/builtins_meta.rs:1636-1638`)
- [ ] `just test` passes

### include-decomp-reduce-decision: Document or implement lazy reduce accumulator

**Whatif:** `include-decomposition`
**Context:** The whatif specifies "Delete the `materialize` call on the accumulator between iterations (`builtins_seq_reduce.rs:80-81`). Pass each step result as a thunk directly." The Seq reduce path still eagerly materializes the accumulator (`src/builtins_seq_reduce.rs:101-102`). The computer-scientist re-review (2026-05-18) found this is sound for `eval-document-pipeline` specifically (each step materializes to a shallow dict) but NOT sound for general reduce over >2048 elements (continuation stack limit). Options: (a) keep eager for safety and document, (b) implement lazy accumulator with documented 2048-element limit, (c) tail-call optimization.

- [ ] Decision: choose option (a), (b), or (c) — update this task with rationale
- [ ] If (a): add a code comment at `src/builtins_seq_reduce.rs:101` explaining the intentional divergence from the whatif and why (continuation stack depth bounds)
- [ ] If (b): remove the `materialize` call; add a comment at the function documenting the depth limit; add a corpus test for reduce over 100 elements
- [ ] `just test` passes

---

## Known Nits (from eval-hardening-perf panel)

- [x] `doc/08-evaluation.md:358` — **FIXED (commit e169762)**
- [x] `tests/corpus/eval/typecheck/constructor_payload_type_precision.llt-eval` — **FIXED (commit e169762)**: replaced with `[@Int p.n]`
- [x] GENSYM_COUNTER ordering — **FIXED (commit e169762)**: standardized to `Relaxed`
- [x] `validate_value` Seq items validation — **FIXED (commit e169762)**: added `validate_seq_items` branch
- [x] Benchmark claim — **FIXED (commit e169762)**: replaced with factual description

## Lint Findings (from lint-clippy / lint-clippy-allows / lint-stdlib-strict — 2026-05-21)

**Note:** `just lint-clippy` and `just lint-stdlib-strict` were blocked during E1-eval-cutover by `E0425 cannot find value 'expr'` errors in `src/parser.rs`. E1 is now complete (2026-05-21); lint gates should be re-checked.

**Fixes applied (2026-05-21):**
- [x] `src/eval.rs:804-1656` — dangling old `eval_recursive` body (853 lines) left by partial deletion in E1-eval-cutover; removed
- [x] `src/builtins_meta.rs:44` — unused import `Spanned`; removed
- [x] `src/builtins_meta.rs:1913-1940` — orphaned `///` doc comment for deleted `builtin_include`; removed (caused `empty_line_after_doc_comment` lint)
- [x] `src/ast_convert.rs:674` — `crate::span::Span` (no such module); corrected to `crate::ast::Span`

**Untracked dead code from lint-clippy-allows (needs future sprint):**
- [ ] `src/type_class.rs:88` — `resolver_injective: bool` has `#[allow(dead_code)]` with note "Wired up when chr-gaps Gap 1 (resolver evaluation) is implemented" — chr-gaps Gap 1 is not tracked in TODO.md; either add a tracking sprint or delete if abandoned
- [ ] `src/type_unify.rs:2135` — `process_deferred_equalities` has `#[allow(dead_code)]` with note "future TypeStageApp deferral sprint (doc/06-type-inference.md:884)" — this sprint is not tracked in TODO.md; either add a tracking sprint or delete if abandoned

**Justified allows (no action needed):** All `mutable_key_type` suppressions in LSP (Uri HashMap keys), all `disallowed_methods` suppressions with AMBIENT-OK comments, all `too_many_arguments` suppressions on recursive helpers, `large_enum_variant` on CLI Args, `type_complexity` on ThunkInner, all `dead_code` on runtime-v2 bridge scaffolding (`surface_fields.rs`, `lower.rs`, `ast_convert.rs`, `arena.rs`).

---

## Known Bugs

- [x] `just test-lib` fails with exit 101. **Fixed (2026-05-19):** Root cause was a parser bug in `src/parser.rs`: `pop_last_value_from_frame` in the `Token::Dot` handler correctly handles `CallArg::Positional` but returns a parse error for `CallArg::Named`. This caused `%: state.prev` in `eval-document-runtime` to fail — the `%` was consumed as the named-arg key, then `state` was consumed as its value (before `.prev` was attached via dot-access). Fix: replaced `%: state.prev` with a let-binding `[prev-val: state.prev]` + `%: prev-val` in `stdlib/prelude.llt::eval-document-runtime`. Also added `expand_macros_in_ctx` to `src/expand.rs` (reuses existing stdlib env when `builtin_expand` is called from within evaluation, eliminating the redundant stdlib reload). All 4 `test_syntax_llt_fn_*` tests pass; `just test-lib` exits 0.

- [ ] **`test_eval_corpus` SIGKILL (OOM) after runtime-v2-rebase additions:** `test_eval_corpus` in `tests/corpus_tests.rs` is killed by the OOM killer (signal 9) after 60+ seconds of running 500+ corpus tests. Root cause is unknown but confirmed to be cumulative memory growth over 500+ test iterations in the shared `ThunkArena` (allocated during `eval_source_with_config` and `typecheck_source_errors_only` for each test). Investigation showed per-test allocation should be <200KB but the OOM fires after ~275 tests (suggesting ~29MB/test somehow). Possible causes: (1) large `ast_to_dict` output accumulation in shared arena from included stdlib files (strings.llt, json-pretty.llt, toml-lite.llt), (2) `deep_materialize` thunks accumulating across tests, (3) interaction between new prelude type declarations (Expression with 23 variants, 12 other type aliases) and the shared arena pattern. Partial mitigation: removed dead `as_surface_program()`/`_resolution_table` calls from `lib.rs` (5 occurrences) since these were dead code. Full fix requires investigating the shared arena growth pattern — either make per-test eval use `clone_for_child` (requires also fixing ThunkId cross-arena access in the include pipeline) or reduce prelude memory footprint. `just test-lib` still passes; only `just test-corpus` fails. (`tests/corpus_tests.rs`, `src/lib.rs`, `src/arena.rs`)

- [x] **CLI Seq tests OOM: FIXED (commit d08f064)** — Root cause: `range` wrapper had `rest` as required parameter instead of variadic `...rest`. `[range 0 3]` bound `rest=3` (Int), `[seq? 3]` returned false, so it called `[builtin-range 0]` (infinite), causing stack exhaustion when `collect` tried to materialize it. Fix: changed to `...rest` variadic.

- [x] **Parser bug FIXED (commit dc1d2d3):** `pop_last_value_from_frame` now handles `CallArg::Named` by extracting the value, restoring `pending_key`, returning value for dot-access transformation. `[foo bar: baz.field]` correctly parses as named arg `bar: DotAccess(baz, "field")`.

### Known Bugs (Type Checker)

- [x] `typecheck::tests::test_dot_access_intersection_found` — Intersection type unification bug: fixed by adding `(Type::Record(..), Type::Intersection(..))` arm to `src/type_unify.rs` that distributes unification across intersection members. Tests pass.
- [x] `typecheck::tests::test_dot_access_intersection_missing_field_returns_unknown` — same Intersection unification bug; fixed by same arm. Tests pass.

---

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class: `∀f a. Filterable f ⇒ (a → Bool) → f a → f a`; compare `Mappable` extension vs separate class using `Data.Witherable` as precedent; include instance examples for `Seq` and `Dict` (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint in TODO.md for `from-json @Schema` schema-directed typed parse (`doc/whatif/schema-directed-from-json.md`)

---

## Post-Migration Health Check Findings (2026-05-20)

### MAJOR: `ThunkStateGuard` unsafe aliasing hazard (PARTIALLY ADDRESSED)

`ThunkStateGuard::deref()` in `src/value.rs:1158-1178` uses `unsafe` to return a reference to thread-local storage that is overwritten on every `.state()` call. If two guards exist simultaneously on the same thread (or any code between guard creation and use calls `.state()` again), the earlier reference silently aliases overwritten data. 66 call sites use `.state()`. This is a latent memory safety bug.

**Status:** Hazard documented with comprehensive safety comments (lines 1157-1183) and debug assertion added to catch double-guard creation in debug builds (lines 1197-1202). The `Drop` impl (lines 1217-1224) clears the guard flag. This provides detection but NOT prevention of the UB.

**Remaining work:** Full fix requires migrating all 66 `.state()` call sites to either return owned `ThunkState` or use `take_*` methods directly.

- [x] Document the aliasing hazard in `ThunkStateGuard` safety comments (`src/value.rs:1157-1183`)
- [x] Add debug assertion to detect double-guard creation (`src/value.rs:1197-1202`)
- [ ] Migrate all 66 `.state()` call sites to owned-state return or `take_*` methods (full fix)

### MAJOR: `builtin_load` discards resolution tables (FIXED)

`builtin_load` in `src/builtins_meta.rs:1369-1370` computes `res_table` and `types_table` but assigns them to `_res_arc`/`_types_arc` (underscore-prefixed), which are immediately dropped. The comment says "Cache the res_table alongside the thunk" but no caching occurs. Downstream, `builtin_eval` in `src/builtins_meta.rs:1551-1552` creates empty tables with a TODO comment. Result: Surface thunks from the `eval` builtin lack resolution data, so `lower()` produces `FreeVar` instead of de Bruijn `Var` for all variable references, causing fallback to name-based chain walks.

**Fix applied:** 
1. Changed `Value::Program` from tuple variant to struct variant with `program`, `resolutions`, and `types` fields (`src/value.rs:426-432`)
2. Updated `builtin_load` to store computed tables in the `Value::Program` (`src/builtins_meta.rs:1366-1372`)
3. Updated `builtin_expand` to extract and re-compute resolution tables (`src/builtins_meta.rs:1394-1430`)
4. Updated `builtin_eval` to accept optional `program:` named argument and extract tables from it (`src/builtins_meta.rs:1448, 1465-1478, 1560-1577`)
5. Updated all pattern matches on `Value::Program` in `src/value.rs`, `src/lib.rs`, `src/eval_materialize.rs`, `src/builtins_meta.rs`

- [x] Thread ResolutionTable + TypeAnnotationTable from `builtin_load` to `builtin_eval` — now stored in `Value::Program` struct fields

### Minor: Blanket `#![allow(dead_code)]` on 3 files

`src/lower.rs`, `src/ast_convert.rs`, `src/surface_fields.rs` all have file-level `#![allow(dead_code)]`. Their public APIs are actively used (11+ call sites for `ast_convert`, 8+ for `surface_fields`, 1 for `lower`). The blanket allow hides actually-dead internal helpers.

- [x] Narrow `#![allow(dead_code)]` — **DONE (commit ca9ca10)**: removed file-level allows, added targeted per-item allows with future-use comments

### Minor: Arena scaffolding `#[allow(dead_code)]` (8 items)

`src/arena.rs` has 8 `#[allow(dead_code)]` annotations on arena-phase3 scaffolding (`alloc_root`, `alloc_letrec_group`, `fill_letrec_slot`, `FlatEnv` fields). These are intentional future-use scaffolding per the arena-phase3 plan. No action needed unless arena-phase3 is abandoned.

### Nit: Stale `%rust` module comments

`src/builtins.rs:1602` ("exposed via %rust 'meta' module") and `src/builtins.rs:1838` ("and %rust 'type-core'") reference the deleted `%rust` module mechanism. The `%rust` dict and `[include %rust "..."]` were deleted in include-decomp-redelete.

- [x] Update stale `%rust` comments — **DONE (commit ca9ca10)**: removed 4 stale %rust module references

### Nit: `lower.rs` has zero unit tests

`src/lower.rs` is the SurfaceExpression-to-CoreExpr lowering pass, called from the Surface thunk handler in `eval.rs:2879`. It has zero unit tests. Exercised indirectly via the Surface thunk path in integration tests, but no targeted coverage of individual lowering cases (VarRef resolution, Pipe desugaring, TypeAssert elaboration).

- [x] Add unit tests for `src/lower.rs` — **DONE (commit ca9ca10)**: 3 tests: Int literal, VarRef with ResolutionTable (→Var), VarRef without entry (→FreeVar)

---

---

## Health Review #22 Findings (2026-05-19)

### integration-fixes: Cross-layer integration issues

- [x] **MAJOR** `ast-of` builtin (`builtin_ast_of`) is defined in `src/builtins_meta.rs:790` but NOT registered in `standard_builtins()` — **FIXED by runtime-v2-rebase Phase 8**: now registered, returns `Value::Expression`, corpus tests updated (`src/builtins.rs`, `tests/corpus/eval/builtins/ast_of_*.llt-eval`)
- [x] Add `Value::Expression` arm to `value_to_expr` — **FIXED (commit ed33064)**
- [x] `dict_to_ast` Variant payload doc comment — **FIXED (commit ed33064)**
- [x] RAII depth guard for `EXPAND_MACROS_DEPTH` — **FIXED (commit ed33064)**: `DepthGuard` struct with Drop impl
- [x] `dict_to_file` doc comment — **FIXED (commit ed33064)**
- [x] `do_infer_resolutions` degraded behavior doc — **FIXED (commit ed33064)**

### clippy-cap-std-lints: Enforce cap-std usage via clippy lints

- [x] Add `clippy.toml` with `disallowed-types` (`std::fs::File`, `std::fs::OpenOptions`, `std::fs::DirEntry`, `std::fs::ReadDir`) and `disallowed-methods` (`std::fs::read`, `std::fs::read_to_string`, `std::fs::write`, `std::fs::metadata`, `std::fs::read_dir`, `std::fs::canonicalize`, `std::fs::remove_file`, `std::fs::create_dir_all`, `cap_std::fs::Dir::open_ambient_dir`) with `reason:` pointing to cap-std alternatives; add `#![deny(clippy::disallowed_types, clippy::disallowed_methods)]` to `src/lib.rs` and `src/main.rs` (`clippy.toml`, `src/lib.rs`, `src/main.rs`)
- [x] Audit all callsites flagged by the above lints and annotate the bare minimum legitimate ones with `#[allow(clippy::disallowed_methods)]` — all others are regressions to fix (`src/`)

---

## I/O Builtins (cap-std gaps)

### io-cap-std-gaps: Add symlink, copy-file, set-permissions, stat-symlink, exists builtins

Five operations cap-std's `Dir` supports that tinct does not yet expose.

**`symlink`** — create a symbolic link within a DirCap. tinct can read and detect symlinks (`read-link`, `stat` `is-symlink` field) but not create them. Needs a new `Symlinkable` DirCap flag.

- [ ] Add `Symlinkable` to the `DirCapFlags` set in `src/value.rs`; update `narrow` to respect it; update `--cap-fs` CLI parser to accept `y` shorthand or fold into `w` bundle (`src/value.rs`, `src/main.rs`)
- [ ] Implement `symlink` builtin: `[symlink cap@DirCap target@String link-path@String]` — calls `Dir::symlink_file()` or `Dir::symlink_dir()` depending on target type; requires `Symlinkable`; both paths RESOLVE_BENEATH (`src/builtins_io.rs`)
- [ ] Register `symlink` in `standard_builtins()` (`src/builtins.rs`)

**`copy-file`** — efficient within-cap file copy via `Dir::copy()` (uses `copy_file_range` on Linux). Currently programs must `slurp` + `write`, allocating the whole file as a String. Requires `Readable` on source cap, `Writable` on destination; no new flag.

- [ ] Implement `copy-file` builtin: `[copy-file src-cap@DirCap src-path@String dst-cap@DirCap dst-path@String]` — calls `src_cap.copy(src_path, &dst_cap, dst_path)` (`src/builtins_io.rs`)
- [ ] Register `copy-file` in `standard_builtins()` (`src/builtins.rs`)

**`set-permissions`** — chmod via `Dir::set_permissions()`. On Unix, requires the process to own the file or hold `CAP_FOWNER`; cap-std uses `fchmodat(dirfd, path, mode, 0)` which follows symlinks (Linux does not support `AT_SYMLINK_NOFOLLOW` for chmod). Not all filesystems support POSIX permissions (e.g., S3-backed DirCaps, FAT volumes, some FUSE mounts would not).

**Decision:** New `PosixPermissions` DirCap flag — not `Writable`, because write authority and permission-bit authority are orthogonal (you can have a writable DirCap on a filesystem that has no POSIX permission concept). `PosixPermissions` explicitly signals that the underlying filesystem supports POSIX mode bits.

- [ ] Add `PosixPermissions` to the `DirCapFlags` set in `src/value.rs`; update `narrow` to respect it; update `--cap-fs` CLI parser (no shorthand — explicit `[PosixPermissions]` only, since it's filesystem-dependent) (`src/value.rs`, `src/main.rs`)
- [ ] Implement `set-permissions` builtin: `[set-permissions cap@DirCap path@String mode@Int]` — `mode` is a Unix octal bitmask (e.g., `0o755`); requires `PosixPermissions` flag; calls `Dir::set_permissions()` with the constructed `Permissions`; on non-Unix, raise a clear error ("set-permissions requires a filesystem with POSIX permission support") (`src/builtins_io.rs`)
- [ ] Register `set-permissions` in `standard_builtins()` (`src/builtins.rs`)

**`stat-symlink`** — lstat equivalent via `Dir::symlink_metadata()`. The existing `stat` follows symlinks; `stat-symlink` does not. Lets programs inspect a broken symlink without error. Same `Statable` flag as `stat`.

- [ ] Implement `stat-symlink` builtin: `[stat-symlink cap@DirCap path@String]` — calls `Dir::symlink_metadata()`; returns same dict schema as `stat` (`name`, `type`, `size`, `mtime`, `is-dir`, `is-file`, `is-symlink`) (`src/builtins_io.rs`)
- [ ] Register `stat-symlink` in `standard_builtins()` (`src/builtins.rs`)

**`exists`** — existence check via `Dir::try_exists()`. Cheaper than `try`+`stat`; distinguishes "not found" (`false`) from "permission denied" (error). Requires `Statable` flag.

- [ ] Implement `exists` builtin: `[exists cap@DirCap path@String]` — calls `Dir::try_exists()`; returns `true`/`false` or raises on permission error (`src/builtins_io.rs`)
- [ ] Register `exists` in `standard_builtins()` (`src/builtins.rs`)

**`get-xattr` / `set-xattr` / `remove-xattr` / `list-xattrs`** — POSIX extended attributes. Not part of cap-std's `Dir` API; implemented by opening the file via the DirCap (getting an fd) then calling `fgetxattr`/`fsetxattr`/`fremovexattr`/`flistxattr` on the fd — this preserves the capability model (no ambient path access). Linux only (macOS has xattrs but different syscall convention; Windows has alternate data streams, not xattrs). Requires a new `ExtendedAttributes` DirCap flag following the `PosixPermissions` naming pattern: the flag asserts the underlying filesystem supports xattrs (ext4, btrfs, tmpfs do; FAT, some network filesystems do not).

**Decision:** `ExtendedAttributes` DirCap flag — no shorthand, explicit `[ExtendedAttributes]` only.

- [ ] Add `ExtendedAttributes` to the `DirCapFlags` set in `src/value.rs`; update `narrow` to respect it (`src/value.rs`)
- [ ] Add `xattr` crate (or use `nix` crate's `fgetxattr`/`fsetxattr` directly) to `Cargo.toml`; gate behind `#[cfg(target_os = "linux")]` (`Cargo.toml`)
- [ ] Implement `get-xattr` builtin: `[get-xattr cap@DirCap path@String name@String]` — opens file via `cap.open(path)` to get an fd, calls `fgetxattr`; returns `Bytes` if the attribute exists, `[]` if not found (ENODATA/ENOATTR); requires `ExtendedAttributes` flag (`src/builtins_io.rs`)
- [ ] Implement `set-xattr` builtin: `[set-xattr cap@DirCap path@String name@String value@Bytes]` — calls `fsetxattr`; requires `ExtendedAttributes` + `Writable` flags (`src/builtins_io.rs`)
- [ ] Implement `remove-xattr` builtin: `[remove-xattr cap@DirCap path@String name@String]` — calls `fremovexattr`; requires `ExtendedAttributes` + `Writable` flags; no-ops gracefully if attribute does not exist (`src/builtins_io.rs`)
- [ ] Implement `list-xattrs` builtin: `[list-xattrs cap@DirCap path@String]` — calls `flistxattr`; returns `[Seq String]` of attribute names; requires `ExtendedAttributes` flag (`src/builtins_io.rs`)
- [ ] Register all four in `standard_builtins()` (`src/builtins.rs`)

**Shared finishing tasks:**
- [ ] Update `doc/11a-builtins.md` §I/O table and §DirCap Permission Flags for all builtins plus `Symlinkable`, `PosixPermissions`, `ExtendedAttributes` (`doc/11a-builtins.md`)
- [ ] Corpus tests for each builtin (see individual specs above) (`tests/corpus/eval/builtins/`)
- [ ] `just test` passes

---

## CHR (Constraint Handling Rules)

Whatif: `doc/whatif/chr-unification.md` (Accepted 2026-05-16). Implementation chain: chr-module-split ✅ → chr-normalization ✅ → chr-class-instance → chr-prelude. Then chr-gaps addresses known implementation gaps found in the 2026-05-17 audit.

### chr-class-instance: Wire ClassDecl into constraint generation and instance lookup

**Whatif:** `chr-unification`
**Depends on:** chr-normalization

- [ ] **Restructure `Constraint::Class`** — per whatif spec: change `class: String` → `class: ClassDecl` (embed the full ClassDecl, not just its name); remove the separate `fundeps` field (ClassDecl.determines is the single source of truth). Update all Constraint::Class construction sites and match arms across `src/type_unify.rs`, `src/typecheck.rs`, `src/typecheck_annot.rs`, `src/typecheck_dict.rs` (~20 sites). Update `improve_functional_dependency` signature to read FDs from `class.determines` instead of the `fundeps` parameter. **Required by chr-gaps:** the resolver name (`class.resolver`) must be accessible to `improve_functional_dependency` without a global lookup — carrying `ClassDecl` directly provides this. (`src/type_class.rs`, `src/type_unify.rs`, `src/typecheck.rs`, `src/typecheck_annot.rs`)
- [ ] Extract FD info at constraint creation: `typecheck_annot.rs:703` has `fundeps: vec![]` hardcoded for ALL user-defined classes — now moot once `Constraint::Class` carries `ClassDecl` directly (determines comes from `ClassDecl.determines`); verify creation sites pass the correct ClassDecl (`src/typecheck_annot.rs:703`)
- [ ] Add `InstanceEnv::lookup_mptc(class, determining_types)` method using tuple keys for multi-param classes — current `lookup_arithmetic_instance()` at `type_unify.rs:588-654` only handles hardcoded arithmetic classes; the general path at lines 641-653 returns error "class not supported by MPTC lookup" (`src/type_unify.rs`)
- [ ] Wire MPTC general lookup into `improve_functional_dependency` — once `lookup_mptc` exists, replace the error stub at `type_unify.rs:641-653` with the general call (`src/type_unify.rs`)
- [ ] Verify `ClassDecl.determines` is correctly populated during class registration for user-defined classes with `fundeps:` syntax — spot-check in tests (`src/typecheck.rs`)
- [ ] Corpus tests: user-defined class with FD fires at a constrained call site; MPTC lookup returns correct instance (`tests/corpus/eval/`)
- [ ] `just test` passes

### chr-prelude: Update prelude class declarations to match CHR design

**Whatif:** `chr-unification`
**Depends on:** chr-class-instance

- [ ] Rename arithmetic class declarations in prelude: `Add` → `Addable`, `Sub` → `Subtractable`, `Mul` → `Multipliable`, `Div` → `Divisible` (spec names per `chr-unification.md` and `doc/06-type-inference.md`) — update all instance declarations and `intern_class_name` in `src/eval.rs` (`stdlib/prelude.llt:1650-1660`, `src/eval.rs`)
- [ ] Restore `Equatable`, `Comparable`, `Showable` class/instance declarations in prelude — currently commented out at `stdlib/prelude.llt:1696-1757`; remove `PRELUDE_INSTANCE_CACHE` workaround in `src/imports.rs` once instances load cleanly (`stdlib/prelude.llt`, `src/imports.rs`)
- [ ] Wire `resolver:` key in `ClassDecl` — link FD resolver to the type-stage function (e.g., `AddResult` for `Addable`); update class registration to extract `resolver:` from the class dict and store in `ClassDecl.resolver` (`src/typecheck.rs`)
- [ ] `just test` passes

### chr-gaps: Fix critical CHR implementation gaps (from 2026-05-17 audit)

**Whatif:** `chr-unification`
**Depends on:** chr-prelude
**Audit source:** mempalace tinct/decisions "CHR MIGRATION AUDIT — GAPS FOUND 2026-05-17"

**Gap 1 — Type-stage resolver evaluation stubbed (CRITICAL):**
- [ ] In `NormCtxt::normalize()` (`src/type_normalize.rs:145`): when cache miss and resolver is known, call the resolver function from the prelude eval context (look up resolver fn by name in `type_stage_env`, call with type dict arg, convert result back to Type) — currently falls through to "return stuck TypeStageApp" (`src/type_normalize.rs:145`)
- [ ] Fix `improve_functional_dependency` stub at `src/type_unify.rs:520-525` — call the type-stage resolver instead of returning early; this is why arithmetic FD tests remain blocked (`src/type_unify.rs:520-525`)

**Gap 2 — FD fundep indices lost at constraint creation (CRITICAL):**
- [ ] (Covered by chr-class-instance `typecheck_annot.rs:703` task above — verify it unblocks Gap 1 testing)

**Gap 3 — MPTC instance lookup general path (PARTIAL):**
- [ ] (Covered by chr-class-instance `lookup_mptc` task above)

**Gap 4 — `resolver_injective` flag unused:**
- [ ] Add parser support for `injective:` key in class declarations to set `ClassDecl.resolver_injective` — currently hardcoded `false` everywhere (`src/type_class.rs:88`, `src/typecheck.rs`); note: the `#[allow(dead_code)]` on `resolver_injective` in `type_class.rs:88` will clear once this is wired (`src/type_class.rs`, `src/typecheck.rs`)

- [ ] End-to-end test: `[class [Add a b c] fundeps: [[[a b] c]] resolver: AddResult +: [fn@c [a b]]]` with `[+ 1 2.0]` infers `c = Float` via FD improvement calling `AddResult` type-stage fn (`tests/corpus/eval/`)
- [ ] `just test` passes

### chr-typestageapp-deferral: Implement TypeStageApp deferral sprint

**Context:** `process_deferred_equalities` in `src/type_unify.rs:2135` has `#[allow(dead_code)]` referencing "future TypeStageApp deferral sprint (doc/06-type-inference.md:884)". This function handles deferred equality constraints that arise when TypeStageApp normalization is stuck (resolver not yet callable). Without deferral, stuck TypeStageApps cause premature unification failures.

- [ ] Read `doc/06-type-inference.md:884` for the TypeStageApp deferral design; confirm it matches `process_deferred_equalities` signature and intent (`doc/06-type-inference.md`)
- [ ] Wire `process_deferred_equalities` into the inference loop — call it after each unification round when deferred equalities are non-empty (`src/type_unify.rs`, `src/typecheck.rs`)
- [ ] The `#[allow(dead_code)]` on `process_deferred_equalities` at `src/type_unify.rs:2135` will clear once wired (`src/type_unify.rs:2135`)
- [ ] `just test` passes

### unknown-elimination: Eliminate `Type::Unknown` from inference output

**Depends on:** chr-class-instance (Type::Variant handling), and HKT support for operator kinds
**Context:** `Type::Unknown` currently leaks into inferred types for expressions the type checker cannot handle — dual-dispatch builtins, HKT positions, gradual typing escape. Goal: make Unknown a controlled escape hatch, not a default fallback.

- [ ] Audit all sites that produce `Type::Unknown` in `src/typecheck.rs` — classify as: (a) intentional gradual typing escape, (b) missing instance lookup (fixable by chr-class-instance), (c) missing HKT support (deferred) (`src/typecheck.rs`)
- [ ] Replace (b) sites with proper constraint generation once chr-class-instance lands
- [ ] Add a lint/test that counts `Type::Unknown` occurrences in inferred output and fails if count exceeds a documented threshold (`tests/`)
- [ ] `just test` passes
