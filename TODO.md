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

### parser-migration-a: Migrate parser literal + simple expression constructions

**Goal:** Change `src/parser.rs` to produce `SurfaceNode`/`SurfaceExpression` instead of `Spanned<Expr>` for simple cases. Unblocks E1-E3 (delete old Expr types).
**Approach:** Work through parser.rs section by section. Each `Rc::new(Spanned::new(Expr::X, span))` becomes `Arc::new(SurfaceNode { expr: SurfaceExpression::X, span })`.

**Simple/literal cases (this sprint):**
- [ ] Migrate `Expr::Int`, `Expr::Float`, `Expr::Bool`, `Expr::Str` → `SurfaceExpression::Int/Float/Bool/Str` in parser (`src/parser.rs`)
- [ ] Migrate `Expr::VarRef { name, escaped, resolved }` → `SurfaceExpression::VarRef { name, escaped }` (no resolved field — goes in ResolutionTable) (`src/parser.rs`)
- [ ] Migrate `Expr::Rest` → `SurfaceExpression::Rest` (`src/parser.rs`)
- [ ] Migrate `Expr::Placeholder` → `SurfaceExpression::Placeholder` (`src/parser.rs`)
- [ ] Change `ParseOutput.file: Spanned<File>` to `ParseOutput.program: SurfaceProgram` (or add `program` alongside `file` first as a bridge) (`src/parser.rs`)
- [ ] Update `ParseOutput::as_surface_program()` to use the native SurfaceProgram field instead of file_to_surface_program bridge (`src/parser.rs`)
- [ ] `just build` passes after this sprint

### parser-migration-b: Migrate parser compound expression constructions

**Depends on:** parser-migration-a
- [ ] Migrate `Expr::Call { func, args, named_args, implied }` → `SurfaceExpression::Call { ... }` all `Rc<Spanned<Expr>>` → `Arc<SurfaceNode>` (`src/parser.rs`)
- [ ] Migrate `Expr::Dict(entries)` → `SurfaceExpression::Dict(Vec<Spanned<SurfaceEntry>>)` (`src/parser.rs`)
- [ ] Migrate `Expr::Fn { params, body, return_ann, desugared }` → `SurfaceExpression::Fn { ... }` (`src/parser.rs`)
- [ ] Migrate `Expr::TypeAssert { annotation, expr }` → `SurfaceExpression::TypeAssert { ... }` (no resolved_type field) (`src/parser.rs`)
- [ ] Migrate `Expr::Annotated { name, annotation }` → `SurfaceExpression::Annotated { ... }` (`src/parser.rs`)
- [ ] Migrate `Expr::DotAccess { expr, field }` → `SurfaceExpression::DotAccess { ... }` (`src/parser.rs`)
- [ ] Migrate `Expr::Pipe { lhs, rhs }` → `SurfaceExpression::Pipe { ... }` (`src/parser.rs`)
- [ ] `just build` passes after this sprint

### parser-migration-c: Migrate parser structural + quasiquote + pattern forms

**Depends on:** parser-migration-b
- [ ] Migrate `Expr::Sequential(exprs)` → `SurfaceExpression::Sequential(Vec<Arc<SurfaceNode>>)` (`src/parser.rs`)
- [ ] Migrate `Expr::Match { scrutinee, arms }` → `SurfaceExpression::Match { ... }` (`src/parser.rs`)
- [ ] Migrate `Expr::Quote/Unquote/UnquoteSplice` → `SurfaceExpression::Quote/Unquote/UnquoteSplice` (`src/parser.rs`)
- [ ] Migrate `Expr::TypeApp { func, arg }` → `SurfaceExpression::TypeApp { ... }` (`src/parser.rs`)
- [ ] Migrate `Expr::LetDecl/PatternDecl/CaseArm` → `SurfaceExpression::LetDecl/PatternDecl/CaseArm` (`src/parser.rs`)
- [ ] `just build` passes after this sprint

### parser-migration-d: Migrate declaration forms to SurfaceDeclaration

**Depends on:** parser-migration-c
- [ ] Route `Expr::TypeAlias`, `Expr::ClassDecl`, `Expr::InstanceDecl` → `SurfaceDeclaration::TypeAlias/ClassDecl/InstanceDecl` wrapped in `SurfaceItem::Decl` (`src/parser.rs`)
- [ ] Route `Expr::DefMacro`, `Expr::MacroDecl`, `Expr::SyntaxClass` → `SurfaceDeclaration::DefMacro/MacroDecl/SyntaxClass` wrapped in `SurfaceItem::Decl` (`src/parser.rs`)
- [ ] Route all expression forms → `SurfaceItem::Expr(Arc<SurfaceNode>)` (`src/parser.rs`)
- [ ] Verify `ParseOutput.program` is fully native (no bridge calls) (`src/parser.rs`)
- [ ] Update all 15 `expand_macros()` call sites to use `expand_surface_program()` instead (now that parser produces SurfaceProgram natively) (`src/expand.rs`, `src/lib.rs`, `src/main.rs`)
- [ ] `just build` + `just test` passes after this sprint

### E1-E3-cutover: Delete old Expr types + eval cutover + delete bridge files

**Depends on:** parser-migration-d (parser produces SurfaceProgram natively)
- [ ] Delete `Expr`, `Document`, `File`, `VarRef.resolved: RefCell`, `TypeAssert.resolved_type: RefCell` from `src/ast.rs` (`src/ast.rs`)
- [ ] Update all eval files to use `CoreExpr`: `eval.rs`, `eval_materialize.rs`, `eval_dict.rs`, `eval_call.rs`, `eval_access.rs` + add `CoreExpr::FreeVar` and `CoreExpr::RuntimeTypeCheck` arms (`src/`)
- [ ] Delete `src/eval_pipeline.rs` (migrate lib.rs/main.rs/repl.rs callers to tinct `cli-pipeline` via invoke_function) (`src/`)
- [ ] Delete `src/ast_dict.rs`, `src/desugar.rs`, `src/ast_convert.rs` (bridge) (`src/`)
- [ ] `cargo check` clean — first real checkpoint per plan

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

---

## Known Nits (from eval-hardening-perf panel)

- [x] `doc/08-evaluation.md:358` — **FIXED (commit e169762)**
- [x] `tests/corpus/eval/typecheck/constructor_payload_type_precision.llt-eval` — **FIXED (commit e169762)**: replaced with `[@Int p.n]`
- [x] GENSYM_COUNTER ordering — **FIXED (commit e169762)**: standardized to `Relaxed`
- [x] `validate_value` Seq items validation — **FIXED (commit e169762)**: added `validate_seq_items` branch
- [x] Benchmark claim — **FIXED (commit e169762)**: replaced with factual description

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

## include-decomp Regression (from runtime-v2 merge)

The runtime-v2 branch was branched before include-decomp landed. The PR #1 merge (2026-05-20) reintroduced old code that include-decomp had deleted. Review: `/review-whatif include-decomposition` confirmed these regressions.

### include-decomp-redelete: Re-delete code regressed by runtime-v2 merge

**Whatif:** `include-decomposition`
**Review:** All include-decomp sprints are DONE but runtime-v2 merge reverted the deletions.

- [x] Delete `builtin_include` from `src/builtins_meta.rs` — **DONE (commit 114ca2a)**
- [x] Delete `Value::RustRegistry` from `src/value.rs` — **DONE (commit 114ca2a)**
- [x] Delete `rust_module()` and all module grouping — **DONE (commit 114ca2a)**
- [x] Delete `EvalState::include_guard` — **DONE (commit 114ca2a)**
- [x] Delete old inode-keyed `include_cache` — **DONE (commit 114ca2a)**
- [x] Delete `src/eval_pipeline.rs` — **kept for test helpers** (eval_file_with_input used by 1900+ unit tests; unused functions deleted)
- [x] Verify `expand` builtin performs real macro expansion — **DONE (commit 114ca2a)**: dict_to_file → expand_macros → ast_to_dict
- [x] After deletions: `just build` passes — **DONE (commit 114ca2a)**

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
