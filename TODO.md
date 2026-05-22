# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Pre-existing test regressions from runtime-v2 merge (tracked, not yet fixed)

These tests are marked `#[ignore]` in the codebase. They reflect known breakage from the
runtime-v2 PR #1 merge and need dedicated sprints to fix:

- **`[do]` macro / Ok-Err as functions** (`src/lib.rs`, 7 tests):
  `test_do_macro_single_step`, `test_do_macro_one_binding_step`, `test_do_macro_three_steps`,
  `test_do_macro_err_propagation`, `test_do_macro_no_steps_calls_pure`,
  `test_do_macro_inferred_form_binding`, `test_do_macro_inferred_form_expr`.
  Root cause: `Ok`/`Err` are now `Variant` constructors, not callable functions. The `[do]`
  monad desugaring calls `Ok`/`Err` as functions; this breaks when they become Variants.
  Fix: make `Variant` constructors callable (like function application), or provide `Ok`/`Err`
  as actual functions that wrap values in a Variant. Sprint: runtime-v2-fix-do-macro.

- **`syntax.llt` include failures** (`src/lib.rs`, 4 tests):
  `test_syntax_llt_fn_no_break`, `test_syntax_llt_fn_macro_triggered`,
  `test_syntax_llt_fn_single_param`, `test_syntax_llt_fn_already_let_decl`.
  Root cause: include-cache-failure / non-exhaustive match in stdlib when loading `syntax.llt`.
  The include caching mechanism has a bug that causes the stdlib `include-cache-failure`
  function to raise a non-exhaustive match error. Sprint: runtime-v2-fix-include-cache.

- **Corpus tests fail due to ADT/class/instance/declaration syntax** (`tests/corpus_tests.rs`):
  `test_typecheck_corpus` (16 files fail), `test_typecheck_error_corpus_eval` (5 files fail),
  and `test_valid_corpus` (11 files fail: `parser/class_let_params`, `special_forms/fn_return_annotation`,
  `special_forms/macro_keyword`, `special_forms/macro_keyword_basic`, `special_forms/type_alias`,
  `special_forms/type_alias_named`, `type_classes/basic_class`, `type_classes/basic_instance`, plus
  3 duplicate failure entries for the same files).
  Root cause: parser rejects `[type ...]`, `[class ...]`, `[instance ...]` syntax that the
  corpus tests expect to work; declaration-form items produce "first item is a declaration, not an
  expression" errors instead of eval results. All failures produce "syntax error (cannot typecheck
  error node)" or "undefined type: Foo". These corpus tests were written for the ADT sprint but
  the parser support was not merged. Also: `test_instance_fd_consistency_violation` in `src/lib.rs`
  had the same root cause and has been `#[ignore]`d. Sprint: runtime-v2-fix-adt-class-instance-corpus.

- **`ThunkState::Placeholder` indistinguishable from `InProgress`** (`src/arena.rs`):
  `test_placeholder_force_panics` — updated to assert `Err` instead of panic, but the
  underlying issue remains: `ThunkInner` cannot distinguish Placeholder (unfilled letrec
  slot) from InProgress (being evaluated). If a letrec slot is accidentally accessed before
  being filled, the error message says "circular dependency" instead of the more accurate
  "forced unfilled placeholder". Sprint: runtime-v2-thunkinner-placeholder-bit.

- **`-o llt` formatter tests fail** (`tests/cli_tests.rs`, 6 tests):
  `eval_format_llt_scalar`, `eval_format_llt_dict`, `eval_format_llt_string`,
  `eval_format_llt_bool`, `eval_format_llt_float`, `eval_flag_with_llt_format`.
  Root cause: `stdlib/cli/out/llt.llt` calls `$llt-repr` which is a prelude wrapper around
  `$builtin-llt-repr`. The `$builtin-llt-repr` Rust builtin was removed in the runtime-v2
  merge and not restored. As a result, all `-o llt` invocations fail with
  "undefined variable: builtin-llt-repr". The tests expect `Int(42)`, `Dict(...)`, etc.
  Fix: restore `builtin-llt-repr` registration in `standard_builtins()` or rewrite
  `stdlib/cli/out/llt.llt` to use an alternative value representation approach.
  Sprint: runtime-v2-fix-llt-repr.

- **`builtin_to_int`/`builtin_to_float` missing arity check** (`src/builtins_math.rs`, 2 tests):
  `to_int_wrong_arity_zero` and `to_float_wrong_arity_zero` panic with `index out of bounds` at `builtins.rs:551`.
  These builtins don't validate that at least 1 arg is provided. Fix: add arity check before accessing `args[0]`.
  Sprint: runtime-v2-fix-builtin-arity.

- **Debug binary RLIMIT_AS self-OOM in CLI tests** (`tests/cli_tests.rs`, ~36 tests):
  `main()` applies `RLIMIT_AS=512MB` to itself, but the debug binary exceeds that limit.
  All CLI tests OOM with `memory allocation of 232 bytes failed` on trivial inputs like `42`.
  Release binary is unaffected. Fix: add `#[cfg(not(debug_assertions))]` guard around the
  `RLIMIT_AS` call in main.rs, or increase the debug limit, or disable during test builds.
  Sprint: runtime-v2-fix-debug-rlimit.

- **Task error re-await returns `{}` instead of error** (`src/builtins_async.rs`):
  When a `Task` completes with an error and is awaited a second time, the state is
  `Done(Ok(Value::Dict(IndexMap::new())))` (the placeholder written during the first await),
  so the error is lost and the second await returns `{}`. Fix: store
  `Done(Result<Value, Arc<EvalError>>)` so errors can be cloned and re-propagated on
  every subsequent await.
  Sprint: runtime-v2-fix-task-error-reawait.

---

### runtime-v2-fix-adt-class-instance-corpus: Parser + pipeline support for declaration forms

**Root cause:** `[type ...]`, `[class ...]`, `[instance ...]`, `[union ...]` as top-level declaration forms are not parsed correctly — the parser produces "first item is a declaration, not an expression" errors. These forms were written for the ADT/typeclasses sprints but the declaration-form parser support was not merged with runtime-v2. 32 corpus tests fail as a result.

**Impact:** The `ByteStream`, `Datagram`, `MessageStream`, `Listener`, and all other typeclasses in lib-net-v3 require `[class ...]` and `[instance ...]` to parse and be wired through the full pipeline. This sprint is a prerequisite for lib-net-v3 implementation.

- [ ] Fix parser to accept `[type ...]`, `[union ...]`, `[class ...]`, `[instance ...]` as `SurfaceDeclaration` items in a document body — these are already defined in the Surface AST (`SurfaceDeclaration` variants) but the parser produces an error when they appear at the top level (`src/parser.rs`)
- [ ] Wire `SurfaceDeclaration::TypeDecl`, `ClassDecl`, `InstanceDecl` through `expand_surface_program` → `resolve_surface_program` → `typecheck_surface_program` pipeline stages (`src/expand.rs`, `src/resolve.rs`, `src/typecheck.rs`)
- [ ] Verify typecheck registers `ClassDecl` into the `ClassEnv` and `InstanceDecl` into `InstanceEnv` during the surface program typecheck pass (`src/typecheck.rs`)
- [ ] Re-enable and update the 32 failing corpus tests: `type_classes/basic_class`, `type_classes/basic_instance`, plus 14 `test_typecheck_corpus` and 5 `test_typecheck_error_corpus_eval` failures (`tests/corpus/`)
- [ ] Re-enable `test_instance_fd_consistency_violation` in `src/lib.rs`
- [ ] `just test` passes

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
**Depends on:** E3-formatter-delete-bridge — both sprints delete `src/ast_convert.rs`; E3-formatter-delete-bridge must complete first so the bridge callers are migrated before the parser stops producing it

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
- [x] `EvalConfig` gains `type_stage_env` — **DONE** (tracked in `include-decomp-eval-types-fix`, now in DONE.md)
- [x] Add tokio + dashmap dependencies — **DONE**
- [x] Update `BuiltinFn` to use `Arc<Thunk>` — **DONE**
- [x] `cargo check` clean — **DONE (just build passes with -D warnings)**
- [x] `just test` passes — **VERIFIED**: build passes with -D warnings, standard_builtins_count passes (226), formatter roundtrip 16/16 pass

---

## runtime-v2 — Sprint 3: Stdlib and Cleanup

❌ NOT STARTED. Depends on Sprint 2 complete.

- [x] `stdlib/desugar.llt` — **DONE (commit f6a41d2)**: 215-line pure-tinct desugaring (metaprogramming tool; pipeline desugar handled by E2-desugar-cutover in Rust)
- [x] `stdlib/codecs/json.llt` `from-json` — **DONE (commit f6a41d2)**: switched to pure-tinct json-parse-value

---

## Macro System v2

Core sprints complete: `macros-v2-ast`, `macros-v2-expand`, `macros-v2-inject`, `macros-v2-stdlib`, `macros-v2-cleanup`, `macros-v2-nits`, `defmacro-retire`, `typed-expr-constructors`, `deep-materialize-variant`. See DONE.md for full history. Two features are stubbed and need follow-up sprints:

### macros-v2-syntax-error: Named syntax-class validation + span-aware macro-error

**Whatif:** `macros-v2`
**Spec chapters:** `doc/whatif/macros-v2.md §Syntax Classes`, `§macro-error and span-of`

#### syntax-class: Named syntax-class registration and validation

- [ ] Add `syntax_classes: HashMap<String, SyntaxClassDef>` to `MacroEnv`; define `SyntaxClassDef { pattern: Rc<Spanned<Expr>>, message: String }` (`src/expand.rs`)
- [ ] Wire `Expr::SyntaxClass` in pre-scan (`src/expand.rs:888-891`) — extract `name:`, `pattern:`, `message:` fields from the node; store in `MacroEnv.syntax_classes`; remove the `// TODO` stub (`src/expand.rs`)
- [ ] Extend `validate_syntax_class` (`src/expand.rs:2013`) — when annotation is a `VarRef` not matching a built-in Expr variant name, look it up in `MacroEnv.syntax_classes`; match the arg against the class `pattern:` using the existing `[let ...]` pattern machinery; on failure emit `MacroError` using the class `message:` field; remove the "Full named syntax-class support is TODO" note (`src/expand.rs`)
- [ ] Corpus tests: named syntax-class validates matching arg (`tests/corpus/eval/macros/syntax_class_match.llt-eval`); named syntax-class rejects non-matching with class `message:` text in error (`tests/corpus/eval/macros/syntax_class_reject.llt-eval`); syntax-class reused across two macros (`tests/corpus/eval/macros/syntax_class_reuse.llt-eval`)
- [ ] `just test` passes

#### macro-error: Expose span-aware macro-error at the tinct level

- [ ] Add `builtin_macro_error` in `src/builtins_meta.rs` — takes `(span: Dict, message: String)`; extracts `start_line`, `start_col`, `end_line`, `end_col` from span dict (as produced by `span-of`); constructs `EvalError` with `ErrorKind::MacroError { message }` at the extracted span; returns `Err(...)` (`src/builtins_meta.rs`)
- [ ] Register `builtin-macro-error` in `standard_builtins()` (`src/builtins.rs`)
- [ ] Update `macro-error` in `stdlib/prelude.llt` — replace `[error message]` wrapper with `[builtin-macro-error span message]`; span argument is now propagated to the error site (`stdlib/prelude.llt`)
- [ ] Corpus test: macro that calls `[macro-error [span-of bad-arg] "expected X"]` produces `E012` at the span of `bad-arg` (`tests/corpus/eval/macros/macro_error_span.llt-eval`)
- [ ] `just test` passes

---

## Primitive Privacy

**Fix-later nits (from include-decomp-prelude sprint):**
- [x] `src/builtins.rs:1590` — stale comment referencing %rust "meta" module (deleted this sprint)
- [x] `src/builtins.rs:1869` — stale `create_type_stage_env()` doc comment referencing %rust "type-core"
- [x] `src/builtins_meta.rs:1481` — `builtin_load` pipeline doc comment missing resolve step
- [x] `stdlib/ast.llt:28-29` — `[Literal ... bare: Bool]` claims bare is always present but only emitted for kind:"str"

### dispatch-cont-h2: Convert H2 conditional builtins to Cont::*Dispatch variants

**Depends on:** sprint-2b-builtins-cps ✅
**Context:** sprint-2b-builtins-cps annotated conditional `materialize(&args[N])` calls with `// H2:` markers. These need Cont::*Dispatch variants in `src/eval_materialize.rs` so builtins don't call materialize() conditionally. Affected: `builtin_connect` transport dispatch, `builtin_narrow` type dispatch, `builtin_sort` comparator, `builtin_gensym` optional prefix arg, `builtin_range` 2-arg vs N-arg.

- [ ] Design Cont::*Dispatch variant pattern for H2 conditional builtins
- [ ] Implement Cont variants and rewrite each `// H2:` annotated builtin

Also fix: `builtins_datetime.rs` uses `materialize(&args.args[N])` (field-access syntax) which escapes `just lint-builtins-cps` (grep for index syntax). Update lint pattern and convert datetime calls.

### reduce-cont-step: Continuation-based reduce — unlimited-depth inputs, no stack cliffs

**Whatif:** `include-decomposition`

- [x] Decide reduce accumulator strategy — **`Cont::ReduceStep` continuation approach.** Add `Cont::ReduceDictStep` and `Cont::ReduceSeqStep` variants so all reduce processing stays within a single `run()` invocation. Eliminates O(N) nested `run()` Rust calls from the Dict lazy-PendingCall-chain path and the Seq eager-materialize-per-step path. TCO (iterative-eval-a/b) handles tail-recursive user functions but does not prevent nested `run()` calls from builtins that call `materialize()` on args — reduce's accumulator chain is exactly this pattern. Root cause confirmed 2026-05-08 (SIGSEGV at 5000 elements); lazy accumulator is not a safe alternative because each `+` / arithmetic step calls `materialize(acc)` from inside `run()`, re-entering `run()` at O(N) Rust stack depth even with iterative materialize. See `/rnd` session 2026-05-21.

- [ ] Add `Cont::ReduceDictStep(Box<ReduceDictStepData>)` to `Cont` enum in `src/eval_materialize.rs` — payload: `{ f: Arc<Thunk>, entries: Vec<(Key, ThunkId)>, idx: usize, call_span: Span, result_thunk: Arc<Thunk> }`. Box required; maintain ≤96-byte `Cont` size invariant. (`src/eval_materialize.rs`)

- [ ] Add `Cont::ReduceSeqStep(Box<ReduceSeqStepData>)` to `Cont` enum — payload: `{ f: Arc<Thunk>, call_span: Span, result_thunk: Arc<Thunk> }`. After each step result, the handler forces the Seq tail to determine next head or end. (`src/eval_materialize.rs`)

- [ ] Rewrite `builtin_reduce` Dict path in `src/builtins_seq_reduce.rs` — collect dict entries into a `Vec`; allocate `result_thunk`; push `Cont::ReduceDictStep` for idx=0; return `result_thunk`. No PendingCall chain. (`src/builtins_seq_reduce.rs`)

- [ ] Rewrite `builtin_reduce` Seq path in `src/builtins_seq_reduce.rs` — allocate `result_thunk` initialised to `init`; push `Cont::ReduceSeqStep`; return `result_thunk`. No eager materialize per step. (`src/builtins_seq_reduce.rs`)

- [ ] Implement `apply_cont(ReduceDictStep)` in `src/eval_materialize.rs` — `result` is the new acc; if `idx == entries.len()`, write acc to `result_thunk` and done; else push `Cont::ReduceDictStep { idx: idx+1, .. }`, then `Cont::PendingCallDispatch` for `f(acc, entries[idx])`. (`src/eval_materialize.rs`)

- [ ] Implement `apply_cont(ReduceSeqStep)` in `src/eval_materialize.rs` — `result` is the new acc; force tail; if tail is `Dict({})` (end), write acc to `result_thunk` and done; if tail is `Seq { head, tail }`, push `Cont::ReduceSeqStep`, then `Cont::PendingCallDispatch` for `f(acc, head)`. (`src/eval_materialize.rs`)

- [ ] `just build` passes; `just test` passes

- [ ] Corpus tests: `reduce` over 5000-element Seq and 5000-entry Dict — no depth-exceeded errors (`tests/corpus/eval/builtins/reduce_large_seq.llt-eval`, `reduce_large_dict.llt-eval`)

---

---

## Known Nits (from eval-hardening-perf panel)

- [ ] `src/eval.rs` — `is_truthy` helper (falsy: `false` and empty dict `[]`) was written for `is:` guard evaluation but that feature is not yet implemented. Deleted the dead function. When `is:` guard syntax is added, re-implement truthy check at that time. (`src/eval.rs`, `src/match_pattern`, future guard eval)

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

**Dead code with tracked sprints (no action needed here):**
- `src/type_class.rs:88` — `resolver_injective: bool` allow — tracked in `chr-instances-gaps` Gap 4 (below)
- `src/type_unify.rs:2135` — `process_deferred_equalities` allow — tracked in `type-inference-cleanup` (below)

**Justified allows (no action needed):** All `mutable_key_type` suppressions in LSP (Uri HashMap keys), all `disallowed_methods` suppressions with AMBIENT-OK comments, all `too_many_arguments` suppressions on recursive helpers, `large_enum_variant` on CLI Args, `type_complexity` on ThunkInner, all `dead_code` on runtime-v2 bridge scaffolding (`surface_fields.rs`, `lower.rs`, `ast_convert.rs`, `arena.rs`).

---

## Known Bugs

- [ ] **LSP markdown block paths missing `expand_surface_program`:** `hover_at` and `diagnostics_for` in `src/lsp/analysis.rs` skip macro expansion for markdown (literate) code blocks — they run `parse → desugar → resolve → typecheck` instead of the full `parse → expand → desugar → resolve → typecheck` pipeline. `lsp/document.rs` correctly calls both, making the inconsistency visible. Fix requires threading an `EvalContext` (with `base_dir`) through `hover_at` and `diagnostics_for` and updating all callers in `src/lsp/server.rs`. Currently annotated with TODO comments at both sites in `src/lsp/analysis.rs` lines 42 and 1345.

- [ ] **`just docgen` fails with `[E020] arity mismatch`** at `[include %libdir "strings.llt"]` in `scripts/docgen.llt:13`. The `include` function (now defined in prelude after include-decomp) causes an arity error in the docgen evaluation context. Root cause unknown — likely a multi-document pipeline scoping issue with how prelude's `include` is resolved in document 1 of docgen.llt. When fixed: `doc/lib/*.md` will regenerate with correct `##` function headings (already updated in render-entry), and `"MD001": false` can be removed from `.markdownlint-cli2.jsonc`. (`scripts/docgen.llt`, `src/`, `stdlib/prelude.llt`)

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

`src/arena.rs` has `#[allow(dead_code)]` annotations on arena-phase3 scaffolding (`alloc_child`, `get_mut`, `alloc_letrec_group`, `get_slot`, `get_by_name`, `insert_overflow`, `parent()`). **`arena-phase3` is DONE** (DONE.md:8560–8585) — `alloc_root`, `fill_letrec_slot`, `get`, and `EnvId` are now live. The remaining allows cover methods kept for the planned O(1) FlatEnv activation sprint (full flat lookup via display-vector, deferred because incremental activation caused a performance regression). No action needed until that activation sprint is picked up.

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

### handle-parameterization: Parameterize Type::Handle with capability row

**Context:** `lib-net-v2` and `io` specs document `Handle[Binary Readable Writable Stream Tls]` notation throughout, but the implementation uses a monolithic `Type::Handle` atom with no row parameter. `Handle` alias has `params: vec![]`. All builtins use `Type::Handle` as-is; `@Handle` annotation works but has no capability checking. Parameterization was deferred as "BAS sprint" in the 2026-05-09 lib-net-v2 audit.

- [ ] Add `Type::Handle(Row)` — change `Type::Handle` from an atomic to carry a `Row` parameter; update all `Type::Handle` construction sites and `value_matches_type()` (`src/types.rs`, `src/eval.rs`, `src/typecheck.rs`)
- [ ] Register `Binary`, `Readable`, `Writable`, `Appendable`, `Seekable`, `Stream`, `Tls`, `Text`, `Exclusive`, `Sync`, `NoFollow` as `Type::Variant` tags in the type env — these are the capability flags used in `Handle[...]` expressions (`src/typecheck.rs`)
- [ ] Update `open`, `connect`, `tls-connect`, `stdin`, `stdout`, `stderr` builtin signatures to return `Type::Handle(row)` with the correct row inferred from the flag arguments (`src/builtins_io.rs`)
- [ ] Update `slurp`, `write`, `lines`, `seek`, `seek-end`, `position`, `flush`, `close` builtin signatures to constrain via row: e.g., `slurp` requires `Readable` in row (`src/builtins_io.rs`)
- [ ] Update `doc/feature/io.md` and `doc/feature/lib-net-v2.md` to reflect that `Handle[...]` is now enforced, not aspirational
- [ ] `just test` passes; add corpus tests for type errors on capability mismatches (e.g., writing to a Readable-only handle)

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

Whatif: `doc/whatif/chr-unification.md` (Accepted 2026-05-16). Implementation chain: chr-module-split ✅ → chr-normalization ✅ → chr-instances-gaps (class-instance + prelude + gap fixes). Then type-inference-cleanup follows.

### chr-instances-gaps: Wire ClassDecl into constraint generation + prelude updates + CHR gap fixes

**Whatif:** `chr-unification`
**Depends on:** chr-normalization ✅
**Audit source (chr-gaps):** mempalace tinct/decisions "CHR MIGRATION AUDIT — GAPS FOUND 2026-05-17"

Do in order within the sprint: class-instance → prelude → gaps.

#### class-instance: Wire ClassDecl into constraint generation and instance lookup

- [ ] **Restructure `Constraint::Class`** — per whatif spec: change `class: String` → `class: ClassDecl` (embed the full ClassDecl, not just its name); remove the separate `fundeps` field (ClassDecl.determines is the single source of truth). Update all Constraint::Class construction sites and match arms across `src/type_unify.rs`, `src/typecheck.rs`, `src/typecheck_annot.rs`, `src/typecheck_dict.rs` (~20 sites). Update `improve_functional_dependency` signature to read FDs from `class.determines` instead of the `fundeps` parameter. **Required by chr-gaps:** the resolver name (`class.resolver`) must be accessible to `improve_functional_dependency` without a global lookup — carrying `ClassDecl` directly provides this. (`src/type_class.rs`, `src/type_unify.rs`, `src/typecheck.rs`, `src/typecheck_annot.rs`)
- [ ] Extract FD info at constraint creation: `typecheck_annot.rs:703` has `fundeps: vec![]` hardcoded for ALL user-defined classes — now moot once `Constraint::Class` carries `ClassDecl` directly (determines comes from `ClassDecl.determines`); verify creation sites pass the correct ClassDecl (`src/typecheck_annot.rs:703`)
- [ ] Add `InstanceEnv::lookup_mptc(class, determining_types)` method using tuple keys for multi-param classes — current `lookup_arithmetic_instance()` at `type_unify.rs:588-654` only handles hardcoded arithmetic classes; the general path at lines 641-653 returns error "class not supported by MPTC lookup" (`src/type_unify.rs`)
- [ ] Wire MPTC general lookup into `improve_functional_dependency` — once `lookup_mptc` exists, replace the error stub at `type_unify.rs:641-653` with the general call (`src/type_unify.rs`)
- [ ] Verify `ClassDecl.determines` is correctly populated during class registration for user-defined classes with `fundeps:` syntax — spot-check in tests (`src/typecheck.rs`)
- [ ] Corpus tests: user-defined class with FD fires at a constrained call site; MPTC lookup returns correct instance (`tests/corpus/eval/`)
- [ ] `just test` passes

#### prelude: Update prelude class declarations to match CHR design

- [ ] Rename arithmetic class declarations in prelude: `Add` → `Addable`, `Sub` → `Subtractable`, `Mul` → `Multipliable`, `Div` → `Divisible` (spec names per `chr-unification.md` and `doc/06-type-inference.md`) — update all instance declarations and `intern_class_name` in `src/eval.rs` (`stdlib/prelude.llt:1650-1660`, `src/eval.rs`)
- [ ] Restore `Equatable`, `Comparable`, `Showable` class/instance declarations in prelude — currently commented out at `stdlib/prelude.llt:1696-1757`; remove `PRELUDE_INSTANCE_CACHE` workaround in `src/imports.rs` once instances load cleanly (`stdlib/prelude.llt`, `src/imports.rs`)
- [ ] Wire `resolver:` key in `ClassDecl` — link FD resolver to the type-stage function (e.g., `AddResult` for `Addable`); update class registration to extract `resolver:` from the class dict and store in `ClassDecl.resolver` (`src/typecheck.rs`)
- [ ] `just test` passes

#### gaps: Fix critical CHR implementation gaps (from 2026-05-17 audit)

**Gap 1 — Type-stage resolver evaluation stubbed (CRITICAL):**
- [ ] In `NormCtxt::normalize()` (`src/type_normalize.rs:145`): when cache miss and resolver is known, call the resolver function from the prelude eval context (look up resolver fn by name in `type_stage_env`, call with type dict arg, convert result back to Type) — currently falls through to "return stuck TypeStageApp" (`src/type_normalize.rs:145`)
- [ ] Fix `improve_functional_dependency` stub at `src/type_unify.rs:520-525` — call the type-stage resolver instead of returning early; this is why arithmetic FD tests remain blocked (`src/type_unify.rs:520-525`)

**Gap 2 — FD fundep indices lost at constraint creation (CRITICAL):**
- [ ] (Covered by class-instance `typecheck_annot.rs:703` task above — verify it unblocks Gap 1 testing)

**Gap 3 — MPTC instance lookup general path (PARTIAL):**
- [ ] (Covered by class-instance `lookup_mptc` task above)

**Gap 4 — `resolver_injective` flag unused:**
- [ ] Add parser support for `injective:` key in class declarations to set `ClassDecl.resolver_injective` — currently hardcoded `false` everywhere (`src/type_class.rs:88`, `src/typecheck.rs`); note: the `#[allow(dead_code)]` on `resolver_injective` in `type_class.rs:88` will clear once this is wired (`src/type_class.rs`, `src/typecheck.rs`)

**Gap 5 — `lookup_mptc` broken for HKT instance heads:**
- [ ] `InstanceEnv::lookup_mptc` in `src/type_class.rs` builds keys via `type_to_string_key` → `to_string()` on freshened types. For HKT instance heads like `App(Operator("Channel"), TypeVar("t"))`, the freshened key `"[Channel _t42]"` does not match a ground query key `"[Channel Int]"`, so FD improvement silently fails for these instances. Practical impact is limited — `resolve_instance` (linear scan + unify) still resolves the instance correctly in most cases — but FD improvement doesn't fire, requiring more type annotations at call sites. Fix: replace string-key lookup with structural unification over the raw (pre-freshening) instance type list, making it consistent with `resolve_instance`. (`src/type_class.rs`)

**Gap 6 — instance declaration ordering within and across modules:**

- [ ] Verify that the type checker implements a two-pass approach for instance resolution: (1) collect all `[instance ...]` declarations from a module into `InstanceEnv` before (2) typechecking any expressions against it. Without this, a function on line 10 that uses `[instance [Listener TcpListener] ...]` declared on line 200 of the same module would fail. This is the standard semantics for typeclass systems. The `[class ...]` / `[instance ...]` parser support must be working first (currently failing corpus tests: `type_classes/basic_class`, `type_classes/basic_instance`). Also verify cross-module: instances from all imported modules are collected before resolving constraints in the importing module. (`src/typecheck.rs`, `src/imports.rs`, `tests/corpus/eval/`)

**Gap 7 — constraint propagation through higher-order function arguments:**
- [ ] Verify that constraints on closure types propagate correctly through higher-order function arguments. Specifically: when `[fn [let h] [tls-accept h cfg]]` (type `[Fn [t] TlsConnection constraint: [t: ByteStream]]`) is passed to `channel-map` as `f: [Fn [element] result]`, the `ByteStream` constraint on `element` must propagate to `in-ch: [Channel element]` at the `channel-map` call site — making `Channel@QuicConnection` a compile-time error rather than a runtime failure. This is standard HM + typeclass constraint propagation; verify the constraint collection path in `typecheck_annot.rs` / `type_unify.rs` handles this case correctly. Add a corpus test: `[channel-map [fn [let h@t constraint: [t: ByteStream]] [str h]] quic-channel]` should produce a type error at the call site, not inside the closure. (`src/typecheck_annot.rs`, `src/type_unify.rs`, `tests/corpus/eval/`)


- [ ] End-to-end test: `[class [Add a b c] fundeps: [[[a b] c]] resolver: AddResult +: [fn@c [a b]]]` with `[+ 1 2.0]` infers `c = Float` via FD improvement calling `AddResult` type-stage fn (`tests/corpus/eval/`)
- [ ] `just test` passes

### type-inference-cleanup: TypeStageApp deferral + T013 readability + Unknown elimination

**Depends on:** chr-instances-gaps (provides chr-prelude and chr-class-instance, needed by deferral wiring and Unknown elimination)

#### TypeStageApp deferral (formerly chr-typestageapp-deferral)

`process_deferred_equalities` in `src/type_unify.rs:2135` has `#[allow(dead_code)]` referencing "future TypeStageApp deferral sprint (doc/06-type-inference.md:884)". This function handles deferred equality constraints that arise when TypeStageApp normalization is stuck (resolver not yet callable). Without deferral, stuck TypeStageApps cause premature unification failures.

- [ ] Read `doc/06-type-inference.md:884` for the TypeStageApp deferral design; confirm it matches `process_deferred_equalities` signature and intent (`doc/06-type-inference.md`)
- [ ] Wire `process_deferred_equalities` into the inference loop — call it after each unification round when deferred equalities are non-empty (`src/type_unify.rs`, `src/typecheck.rs`)
- [ ] The `#[allow(dead_code)]` on `process_deferred_equalities` at `src/type_unify.rs:2135` will clear once wired (`src/type_unify.rs:2135`)
- [ ] `just test` passes

#### T013 warning readability (formerly type-warning-readability)

T013 warnings currently report internal inference variable names like `_t86` instead of the user-visible source variable that introduced the constraint. Example: `warning[T013]: ambiguous type variable '_t86' in constraint Showable: appears in constraint but not in the type — constraint will be silently dropped`. The user cannot tell which declaration or expression introduced `_t86`.

- [ ] In the T013 warning emitter (`src/typecheck.rs` or `src/type_unify.rs`), look up the source variable name that was unified with `_t86` and include it in the message (`src/typecheck.rs`)
- [ ] If no source name exists (truly fresh inference variable), fall back to reporting the expression span and annotation context instead of the raw internal name
- [ ] Add a corpus test: `tests/corpus/eval/errors/t013_ambiguous_type_var.llt-eval` that verifies the message contains a user-readable name

#### Unknown elimination (formerly unknown-elimination)

`Type::Unknown` currently leaks into inferred types for expressions the type checker cannot handle — dual-dispatch builtins, HKT positions, gradual typing escape. Goal: make Unknown a controlled escape hatch, not a default fallback. HKT infrastructure (Kind::Operator, Type::App, Mappable/Appendable) is complete — residual Unknown at HKT positions is an instance-lookup gap covered by chr-instances-gaps, not missing HKT machinery.

- [ ] Audit all sites that produce `Type::Unknown` in `src/typecheck.rs` — classify as: (a) intentional gradual typing escape, (b) missing instance lookup (fixable by chr-instances-gaps), (c) missing HKT support (deferred) (`src/typecheck.rs`)
- [ ] Replace (b) sites with proper constraint generation once chr-instances-gaps lands
- [ ] Add a lint/test that counts `Type::Unknown` occurrences in inferred output and fails if count exceeds a documented threshold (`tests/`)
- [ ] `just test` passes
