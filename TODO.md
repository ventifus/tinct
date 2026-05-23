# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Unified Bindings (doc/whatif/unified-bindings.md — Accepted 2026-05-17)

### unified-bindings-parser-enforcement: Enforce [let ...] required in fn/class/type params

Per unified-bindings.md: `[fn [x y] body]` without `let` should be a **parse error**. The invariant: any bracket not starting with `let` is always an expression. Currently the parser accepts both forms — this is a bug.

- [x] Parser rejects `[fn [x y] body]` (non-`let` non-empty params) — `[fn [let x y] body]` required; `[fn [] body]` still allowed as zero-arg shorthand (`src/parser.rs`)
- [x] Updated ~30 test inputs in src/ to use `[let ...]` form
- [x] Updated stdlib/syntax.llt comment to clarify macro purpose
- [ ] Enforce `[class [let ...]]` and `[type [let ...]]` — type/class params not yet enforced
- [ ] Update tests/corpus/ and stdlib/ to use `[let ...]` throughout
- [ ] `just test` passes — lib tests 1881/0 ✓

---

## Runtime-v2 Regression Fixes

Tests marked `#[ignore]` in the codebase from the runtime-v2 PR #1 merge.

### runtime-v2-fix-regressions: Fix do-macro, include-cache, placeholder, and task-reawait

- [x] Verify Variant constructors are callable — already implemented (eval.rs:2264-2274), 7 do-macro tests un-ignored
- [x] Fix include-cache — 3 root causes identified and fixed: (1) doc.stage field handler in eval_materialize.rs, (2) Overlay flattening in builtin_eval env: arg, (3) builtin_eval returning Seq of thunks instead of last evaluated value, (4) doc.expressions returning Dict instead of Seq linked list
- [x] Fix syntax.llt tests — all 4 now pass (un-ignored)
- [x] Placeholder test — already fixed, expects Err for circular dependency
- [x] Fix task-error-reawait — clone result into guard BEFORE `?` extraction in builtins_async.rs
- [x] Add test for task error re-await behavior (`src/builtins_async.rs`)
- [x] Un-ignore all fixed tests
- [ ] `just test` passes — **PARTIAL**: do-macro 8/8 ✓, syntax.llt 4/4 ✓, standard_builtins_count ✓; full suite not yet run

### runtime-v2-fix-adt-class-instance-corpus: Un-ignore LSP class/instance hover tests

The parser supports `[class ...]` and `[instance ...]` syntax (confirmed by corpus tests
`tests/corpus/valid/type_classes/basic_class.llt-eval` and `basic_instance.llt-eval`).
Two LSP hover tests remain `#[ignore]`'d with a now-stale reason.

- [ ] Un-ignore `test_hover_at_declaration_class_decl` (`src/lsp/analysis.rs:2851`) — update
      test to use the actual `[class [let ClassName a] method: type]` corpus syntax
- [ ] Un-ignore `test_hover_at_declaration_instance_decl` (`src/lsp/analysis.rs:2863`) — update
      test to use `[instance ClassName [pattern [a@Type]]: method: impl]` corpus syntax
- [ ] Verify LSP hover on class/instance declarations returns correct method signatures
- [ ] `just test-lsp` passes

---

### runtime-v2-fix-class-instance-in-dict: Support declaration forms inside dict values

- [x] Extend parser to allow ClassDecl/InstanceDecl/TypeDecl as SurfaceExpression variants inside dict values — modified 6 CloseBracket handlers (TypeAlias, DefMacro, MacroDecl, SyntaxClass, ClassDecl, InstanceDecl) to use surface_decl_to_expr bridge instead of erroring (`src/parser.rs`)
- [x] Un-ignore `test_instance_fd_consistency_violation` (`src/lib.rs`)
- [ ] Verify 26 corpus tests pass (`tests/corpus_tests.rs`) — **UNVERIFIED** (Bash unavailable)
- [ ] `just test` passes — **UNVERIFIED** (Bash unavailable)

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

**Phase 1-3: ALREADY DONE** — parser constructs SurfaceExpression natively (130 usages), StackFrame uses Arc<SurfaceNode>, ParseOutput.program is SurfaceProgram, all expand_macros() calls migrated to expand_surface_program().

- [x] Phase 1: Frame stack types — StackFrame uses Arc<SurfaceNode>, push_value takes Arc<SurfaceNode>
- [x] Phase 2: Expression construction sites — 130 SurfaceExpression:: usages in parser.rs
- [x] Phase 3: Output type — ParseOutput.program: SurfaceProgram; all expand_surface_program() callers
- [ ] Delete `src/ast_convert.rs` — **BLOCKED**: 118 active callers in eval pipeline (expr_to_core_expr, surface_program_to_file)
- [ ] `just build` + `just test` passes — **UNVERIFIED**

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
- [ ] Delete from `src/ast.rs`: `Expr`, `Document`, `File` etc — **BLOCKED**: eval_materialize.rs, eval_dict.rs, eval_call.rs still use Expr types; must migrate those first
- [ ] Migrate `eval_materialize.rs`, `eval_call.rs` from `Expr` to `CoreExpr` — **PARTIALLY DONE**: eval_dict.rs migrated (eval_dict_core + eval_key_core, literal fast-paths, CoreExpr::Dict arm updated in eval.rs); non-literal values still use core_expr_to_expr round-trip (TODO(parts-e)); eval_materialize/call still need migration
- [ ] Delete: `src/eval_pipeline.rs`, `src/eval_deep.rs`, `src/ast_dict.rs`, `src/desugar.rs`, `src/ast_convert.rs` — **BLOCKED**: all have active callers (eval_deep→formatter/imports, ast_dict→formatter/expand, desugar→formatter/imports, ast_convert→expand/surface_fields)
- [x] Update `IncludeCacheEntry::Cached` — **DONE**
- [x] Rc→Arc migration — **DONE (commit b0aa803)**: 34 files, 2450 ins, 2437 del
- [ ] **`cargo check` clean after Part E** — first checkpoint per plan

### Part F ✅, Part G ✅, sprint-2a-rc-arc ✅ — All complete (see DONE.md)

---

## Linear Accumulators (`doc/whatif/linear-accumulators.md`)

**Depends on:** runtime-v2 Part E (Rc→Arc) complete. Must complete before `stdlib/dist.llt` is authored.

### linear-accumulators-seq: Seq rewrite for list-building functions

**Whatif:** `linear-accumulators`
- [x] Rewrite `values`, `entries`, `reindex` — lazy Seq builders + collect, O(n²)→O(n) (`stdlib/prelude.llt`)
- [x] Rewrite `zip` — dict path uses lazy Seq cons via zip-dict-seq-impl (`stdlib/prelude.llt`)
- [x] Rewrite `flatten` — recursive Seq builder via flatten-seq-impl (`stdlib/prelude.llt`)
- [x] Rewrite `uniq` — cons accumulation on both acc and seen, reverse + collect (`stdlib/prelude.llt`)
- [x] Rewrite `partition` — cons on each arm + reverse + collect (`stdlib/prelude.llt`)
- [x] Add 7 large-input corpus tests (n=1000): values, entries, reindex, zip, flatten, uniq, partition
- [ ] `just test` passes — **UNVERIFIED** (Bash unavailable)

### linear-accumulators-build-dict: `build-dict` Rust primitive

**Whatif:** `linear-accumulators`
- [x] Implement `builtin_build_dict` — dual-dispatch Seq/Dict/Overlay, pre-allocated IndexMap, keys forced, values lazy (`src/builtins_dict.rs`)
- [x] Register as `"build-dict"` — count 266→267 (`src/builtins.rs`)
- [x] Rewrite `from-entries`, `map-entries`, `remove`, `take-while`, `drop-while`, `slice`, `walk-dict`, `transpose-impl`, `collect-kv`, `deep-merge` — all use `build-dict` (`stdlib/prelude.llt`)
- [x] Add `build-dict` to `doc/11-stdlib.md`
- [ ] Note: `stdlib/dist.llt` (not yet written) must use `build-dict` for `partition n seq`
- [ ] `just test` passes — **UNVERIFIED**

### linear-accumulators-transient: Transient `Value::Builder`

**Whatif:** `linear-accumulators`
- [x] Add `Value::Builder(Arc<Builder>)` to `src/value.rs`
- [x] Add `Builder` struct — `Mutex<Option<IndexMap>>` + `AtomicBool frozen`
- [x] Implement `make-builder`, `builder-set`, `builder-delete`, `builder-finish`, `builder-snapshot`, `builder-has?`, `builder-get` in `src/builtins_dict.rs`
- [x] Rewrite `group-by` using builder (`stdlib/prelude.llt`)
- [x] Add "Transient Construction" section to `doc/11-stdlib.md` with one-shot/sequential-use invariant

### linear-accumulators-fixes: Panel review remediation

**Whatif:** `linear-accumulators`
**Depends on:** `linear-accumulators-transient`

Panel review (stdlib-author, eval-engine, performance-expert, computer-scientist) returned REQUEST_CHANGES. All fix-now items required before this sprint cluster can be approved.

**Fix-now — correctness:**
- [ ] Fix `group-by` O(n²) regression — uses `append` on per-key buckets (O(n) clone per step); fix: `[cons x bucket]` + `[reverse [collect e.value]]` per bucket after `builder-finish`, matching the whatif spec (`stdlib/prelude.llt:1144-1153`)
- [ ] Add `EvalError::builder_already_finished` variant — all seven builder ops use `EvalError::internal` when frozen (user programming error ≠ internal bug) (`src/error.rs`, `src/builtins_dict.rs:969,1039,1084,1128,1194,1265`)
- [ ] Fix `builder-has?` silent `false` on frozen builder — `Builder::has` returns `map_or(false, ...)` when frozen; must return `Err(builder_already_finished)` (`src/value.rs:381-384`, `src/builtins_dict.rs:builtin_builder_has`)
- [ ] Fix `builder-get` conflates "frozen" and "key absent" — `Builder::get` returns `None` for both; distinguish: frozen → `builder_already_finished`; absent → `key_not_found` (`src/value.rs:Builder::get`, `src/builtins_dict.rs:builtin_builder_get`)
- [ ] Fix `Key::String("key".to_string())` allocated per entry in `build-dict` Seq path — use zero-copy borrow for IndexMap lookup (`src/builtins_dict.rs:766,782`)
- [ ] Move private helpers to private dict — `flatten-seq-impl`, `flatten-seq-step`, `reverse-seq`, `reverse-seq-impl`, `uniq-seq-impl`, `uniq-seq-step`, `contains-seq?` visible in public dict; must move to private first dict (`stdlib/prelude.llt`)
- [ ] Deduplicate `values-seq-impl` / `reindex-seq-impl` — identical bodies (`stdlib/prelude.llt:225-257`)
- [ ] Fix `src/value.rs:336` broken doc comment — single `/` instead of `///` (`src/value.rs:336`)
- [ ] Fix `build-dict` comment misidentifying pre-materialization mechanism — says `pos_strictness` but it is `force_count=1` (`src/builtins_dict.rs:706`)

**Fix-now — tests:**
- [ ] Add corpus tests for builder builtins: basic construction, `builder-has?` present/absent, `builder-get` present/absent, `builder-finish` produces correct Dict, `builder-snapshot`, double-finish error, `builder-set`/`builder-has?`/`builder-get` after-finish errors (`tests/corpus/eval/builtins/`)
- [ ] Add large-input tests (n≥1000) for all build-dict-sprint functions: `from-entries`, `map-entries`, `remove`, `take-while`, `drop-while`, `slice`, `walk`, `deep-merge` (`tests/corpus/eval/stdlib/`)
- [ ] Add large-input test (n≥1000, high-collision) for `group-by` (`tests/corpus/eval/stdlib/`)
- [ ] `just test` passes

**Fix-later:**
- [ ] Fix `flat-map` O(n²) for Dict inputs — `[builtin-concat acc [f x]]` in reduce clones accumulator each step; fix: accumulate Seq of Seqs via `cons`, flatten with single `concat` at end (`stdlib/prelude.llt:1116-1124`)
- [ ] Fix `deep-merge` produces `Value::Overlay` output — `[merge a [build-dict ...]]` creates Overlay; for dist-eval must be flat; fix by building over union key set instead of wrapping with `merge` (`stdlib/prelude.llt:1178`)
- [ ] Add `AtomicBool frozen` fast-path to Builder — current impl acquires mutex on every `has?`/`get`; `AtomicBool::load(Relaxed)` short-circuits before mutex for frozen check (`src/value.rs`)
- [ ] Remove or fix `Builder::Clone` — deep-copy impl is dead code; remove or replace with panicking impl (`src/value.rs`)
- [ ] Consolidate `build-dict` Seq path to single traversal — collect `Vec<(head, tail)>` in pass 1, eliminating pass 2 (`src/builtins_dict.rs`)
- [ ] When `dist-eval` sprint implements `distributable?`: add `Value::Builder` to the non-distributable value set in the capability-walk implementation; verify `remote-task` rejects thunks whose environment contains a builder (`src/serialize.rs` or wherever `distributable?` is implemented)
- [ ] Wire `make-builder capacity:` named arg — currently rejects named args; wire hint to `IndexMap::with_capacity` (`src/builtins_dict.rs:882-901`)
- [ ] Add combined `builder-get-or` op — `builder-has?` + `builder-get` in `group-by` is 2 mutex acquires; a `get_or_default`-style op would halve it (`src/value.rs`, `src/builtins_dict.rs`)
- [ ] Clarify `uniq` doc — O(n²) overall due to `contains?`; cons rewrite improved accumulation constants only (`stdlib/prelude.llt`)

### linear-accumulators-review: Post-implementation review

**Whatif:** `linear-accumulators`
**Depends on:** `linear-accumulators-fixes`

- [ ] Run `/review-whatif linear-accumulators` — verify all sprints complete, implementation matches spec, main docs consistent

---

---

## Macro System v2

Core sprints complete: `macros-v2-ast`, `macros-v2-expand`, `macros-v2-inject`, `macros-v2-stdlib`, `macros-v2-cleanup`, `macros-v2-nits`, `defmacro-retire`, `typed-expr-constructors`, `deep-materialize-variant`. See DONE.md for full history. Two features are stubbed and need follow-up sprints:

### macros-v2-syntax-error: Named syntax-class validation + span-aware macro-error

**Whatif:** `macros-v2`
**Spec chapters:** `doc/whatif/macros-v2.md §Syntax Classes`, `§macro-error and span-of`

#### syntax-class: Named syntax-class registration and validation

- [x] Add `syntax_classes: HashMap<String, SyntaxClassDef>` to `MacroEnv` with pattern + message fields (`src/expand.rs`)
- [x] Wire `Expr::SyntaxClass` in pre-scan — extracts name/pattern/message fields, stores in MacroEnv (`src/expand.rs`)
- [x] Extend `validate_syntax_class` — looks up named classes, validates via `validate_against_pattern` helper (`src/expand.rs`)
- [x] 3 corpus tests: syntax_class_match, syntax_class_reject, syntax_class_reuse
- [ ] `just test` passes — **UNVERIFIED**

#### macro-error: Expose span-aware macro-error at the tinct level

- [x] Add `builtin_macro_error` in `src/builtins_meta.rs` — extracts span dict, constructs EvalError with E012 MacroError
- [x] Register `builtin-macro-error` in `standard_builtins()` — count 265→266 (`src/builtins.rs`)
- [x] Update `macro-error` in `stdlib/prelude.llt` — now calls `[builtin-macro-error span message]`
- [x] Corpus test: `macro_error_span.llt-eval`
- [ ] `just test` passes — **UNVERIFIED**

---

## Continuation-Based Builtins

### dispatch-cont-h2: Convert H2 conditional builtins to Cont::*Dispatch variants

**Depends on:** sprint-2b-builtins-cps ✅
**Context:** sprint-2b-builtins-cps annotated conditional `materialize(&args[N])` calls with `// H2:` markers. These need Cont::*Dispatch variants in `src/eval_materialize.rs` so builtins don't call materialize() conditionally. Affected: `builtin_connect` transport dispatch, `builtin_narrow` type dispatch, `builtin_sort` comparator, `builtin_gensym` optional prefix arg, `builtin_range` 2-arg vs N-arg.

- [x] Survey all 9 H2 sites — 1 real fix (sort: updated registration to `[Spine, Spine]`, replaced materialize with try_get_materialized), 8 documented as safe conditionals (args.len() check or pre-materialized discriminant dispatch)
- [x] All `// H2:` markers removed — replaced with safe-conditional documentation
- [ ] `builtins_datetime.rs` field-access materialize — **DEFERRED** (agent did not find H2 markers in datetime; needs separate lint check)
- [ ] `just test` passes — **UNVERIFIED**

### reduce-cont-step: Continuation-based reduce — unlimited-depth inputs, no stack cliffs

**Whatif:** `include-decomposition`

- [x] Decide reduce accumulator strategy — **`Cont::ReduceStep` continuation approach.** Add `Cont::ReduceDictStep` and `Cont::ReduceSeqStep` variants so all reduce processing stays within a single `run()` invocation. Eliminates O(N) nested `run()` Rust calls from the Dict lazy-PendingCall-chain path and the Seq eager-materialize-per-step path. TCO (iterative-eval-a/b) handles tail-recursive user functions but does not prevent nested `run()` calls from builtins that call `materialize()` on args — reduce's accumulator chain is exactly this pattern. Root cause confirmed 2026-05-08 (SIGSEGV at 5000 elements); lazy accumulator is not a safe alternative because each `+` / arithmetic step calls `materialize(acc)` from inside `run()`, re-entering `run()` at O(N) Rust stack depth even with iterative materialize. See `/rnd` session 2026-05-21.

- [x] Rewrite `builtin_reduce` to use PendingBuiltin chain (heap iteration, not Rust stack) — added `builtin_reduce_dict_step` and `builtin_reduce_seq_step` helpers that create lazy PendingBuiltin chains, following existing `concat_seq_step` pattern. Chose PendingBuiltin chains over Cont variants because builtins cannot push continuations directly. (`src/builtins_seq_reduce.rs`)
- [x] Register `reduce-dict-step` and `reduce-seq-step` in `standard_builtins()` — builtin count 263→265 (`src/builtins.rs`)
- [x] Corpus tests: reduce over 5000-element Seq and 1000-entry Dict (`tests/corpus/eval/builtins/reduce_large_seq.llt-eval`, `reduce_large_dict.llt-eval`)
- [ ] `just build` passes; `just test` passes — **UNVERIFIED** (Bash unavailable)

---

## Known Bugs + Nits

### rc-arc-complete: Complete Rc→Arc migration — make `Thunk` fully Send+Sync

**Decision:** Option B (`#![allow(clippy::arc_with_non_send_sync)]`) is NOT acceptable.
The migration must be completed. `#[allow]` suppression of a soundness-adjacent lint is
off the table.

The outer Rc→Arc migration (commit b0aa803) wrapped things in Arc but left `Rc<>` inside
`Thunk` itself. This causes `clippy::arc_with_non_send_sync` to fire 690 times in
`ast_dict.rs` because `Arc<Thunk>` requires `Thunk: Send + Sync`, but `Thunk` contains
`Rc<Environment>` and other non-Send types.

**Scope:** Replace remaining `Rc<>` inside `Thunk` and its dependencies with `Arc<>` or
`Arc<RwLock<>>` / `Arc<Mutex<>>` as appropriate. This is the final step to make `Thunk`
fully thread-safe.

**Files likely affected** (grep for `Rc<` in src/):
- `src/value.rs` — `Thunk` struct; `Environment` fields; `EvalContext`
- `src/arena.rs` — `ThunkArena` internal storage
- `src/eval.rs`, `src/eval_call.rs`, `src/eval_materialize.rs` — callers

- [ ] Grep `src/` for remaining `Rc<` usages — identify all that need migration
- [ ] Migrate `Thunk` internals: replace `Rc<Environment>`, `Rc<EvalContext>`, etc. with `Arc<>`
- [ ] Ensure `ThunkState` mutations use `Mutex` or `RwLock` correctly (already has `Mutex<ThunkState>`)
- [ ] Verify `cargo clippy -- -D warnings` passes with zero `arc_with_non_send_sync` errors
- [ ] `just test` passes (1874+ tests)

### lint-builtins-cps: 10 unannotated H1 materialize() calls in builtins ✅ FIXED

All 10 annotated as `// H2:` (conditional materialize — correct pattern):
- `src/builtins_io.rs:972,973,1170,1171` — transport-type discriminant dispatch (Tcp/Udp arms)
- `src/builtins_io.rs:1078,1248` — transport-type discriminant dispatch (UnixStream/UnixDatagram arms)
- `src/builtins_meta.rs:189,204` — arity guard (`args.len() != 2`) acts as structural check
- `src/builtins_meta.rs:667` — `else if args.len() == 1` conditional
- `src/builtins_seq_gen.rs:93` — `else` branch of `if args.len() == 1` (finite range path)

- [x] All 10 lines annotated `// H2:` — `just lint-builtins-cps` passes

### lint-md: 26 markdown errors found by markdownlint-cli2

`just lint-md` fails with 26 errors. Files and error types:

**doc/whatif/ffi.md** (14 errors):
- Line 11: MD051 invalid link fragment
- Lines 278: MD040 fenced code block without language
- Lines 338,353: MD036 emphasis used instead of heading
- Lines 504,522-536: MD022/MD032 headings/lists need surrounding blank lines

**doc/whatif/lib-net-v3.md** (12 errors):
- Lines 494,520,684,942,966,1011,1119: MD040 fenced code blocks without language
- Lines 924,1071,1180,1199: MD032 lists need surrounding blank lines
- Line 1119: MD031 fenced code block needs surrounding blank lines

**doc/whatif/linear-accumulators.md** (1 error):
- Line 186: MD032 list needs surrounding blank lines

- [ ] Run `just lint-md-fix` — auto-fixes MD032/MD022/MD031 (blank line issues)
- [ ] Manually fix remaining: MD040 (add language to fenced blocks), MD051 (fix link fragment), MD036 (convert emphasis to heading)

### lint-allow-cleanup: Fix removable `#[allow]` suppressions

Skeptic reviewed all suppressions. These are VERIFIED (keep, no action needed):
- `clippy::large_enum_variant` (main.rs:39) — `Commands::Run` is CLI-only, boxing adds boilerplate for zero benefit
- `clippy::result_large_err` (builtins_dict.rs:212) — `EvalError` is transient before `?` boxes it; restructuring obscures intent
- `clippy::only_used_in_recursion` (typecheck_annot.rs:1877) — Clippy false positive; params ARE used through recursive calls
- `clippy::enum_variant_names` (type_def.rs:104,187) — `TypeVar` is standard PL theory term; renaming to `Var` would be confusing
- `clippy::mutable_key_type` (lsp files, 10 sites) — `Uri` uses string-based Hash/Eq; not mutated after insertion; false positive
- `clippy::disallowed_methods` / `clippy::disallowed_types` — AMBIENT-OK cap-std boundary approvals; each commented
- `clippy::too_many_arguments` — AST traversal requires full context; struct wrapping adds indirection
- `clippy::deprecated` (lsp/analysis.rs:1119) — `lsp-types` requires all `DocumentSymbol` fields; no Default impl

PARTIAL — suppressions needed as-is but fixable:
- [ ] **`clippy::type_complexity`** (`src/value.rs:1453,1484`) — 5-tuple and 6-tuple return types. Introduce named type aliases `PendingBuiltinParts` and `PendingCallParts`, then remove the suppressions. Line 1404 (3-tuple) can keep its suppress or be aliased for consistency.

Needs audit — `dead_code` suppressions without explanatory comments (may be truly dead):
- [ ] **`src/eval.rs:235, 889, 3064, 3110`** — no comment; verify if needed or delete
- [ ] **`src/type_class.rs:107`** — no comment; verify if needed or delete
- [ ] **`src/eval_materialize.rs:122`** — no comment; verify if needed or delete
- [ ] **`src/value.rs:1040`** — no comment; verify if needed or delete

Already justified (keep, no audit needed):
- `src/lower.rs:222,237` — "Used in Part E when batch lowering is activated"
- `src/arena.rs:129,212,221,248,251,255,260,284` — "arena-phase3 scaffolding"
- `src/ast_convert.rs:57,64,83,103,117,1165` — "Part B/Part E scaffolding"
- `src/surface_fields.rs:61,80,87` — "Part E/F scaffolding"

### ci-failures: Fix 4 failing tests identified by `just ci` (2026-05-22)

`just test` result: 1874 passed, 8 failed, 76 ignored. The 4 `test_syntax_llt_fn_*` failures
are already tracked in `runtime-v2-fix-regressions`. These 4 are not yet tracked:

- [ ] **`standard_builtins_contains_all`** (`src/builtins.rs:7083`) — expects 246 builtins, got 283. Update the assertion count to match the current number of registered builtins.
- [ ] **`test_await_error_twice_returns_error_both_times`** (`src/builtins_async.rs:1704`) — fails with `[E099] let declarations are not expressions`. A task containing a let declaration is evaluated when it should return a cached error. Regression from runtime-v2 changes.
- [ ] **`test_circular_dependency_cycle_path`** (`src/eval.rs:9232`) — "Cycle path should be non-empty, got: 0". Circular dependency detection fires the guard but doesn't populate the cycle path. Regression from runtime-v2/include-cache changes.
- [ ] **`test_instance_fd_consistency_violation`** (`src/lib.rs:2172`) — un-ignored by `runtime-v2-fix-class-instance-in-dict` but now fails: "expected consistency violation error, got Ok(())". FD constraint enforcement is not rejecting violations. Regression.

### known-bugs-fix: Fix LSP expansion, docgen arity, eval_corpus OOM

- [x] **`just docgen` fails with `[E020] arity mismatch`:** removed dead-code `[strings: [include %libdir "strings.llt"] path: [include %libdir "path.llt"]]` intermediate dict from `scripts/docgen.llt` — those bindings were never used downstream; the arity mismatch root cause in the multi-document pipeline remains uninvestigated (static analysis could not reproduce it) (`scripts/docgen.llt`)
- [ ] **`test_eval_corpus` SIGKILL (OOM):** Cumulative memory growth over 500+ test iterations in shared ThunkArena. Fix: per-test arena isolation or reduce prelude memory footprint (`tests/corpus_tests.rs`, `src/lib.rs`, `src/arena.rs`)

## Research / Design Items

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint (`doc/whatif/schema-directed-from-json.md`)

---

---

## Codebase Health Audit Findings (2026-05-22)

Health Review #22 (integration-fixes ✅, clippy-cap-std-lints ✅) and Codebase Audit CRITICALs (TypeStageApp occurs check ✅, poll_future_sync invariant ✅) and MAJORs (cache_failure_once ✅, EvalStackGuard ✅, Rémy spec moved ✅, async unit tests ✅, pipeline comments ✅, force_step async ✅) all completed. Remaining items:

### stdlib-builtin-wrappers-audit: Rewrite stdlib to use builtin-* stable aliases

`builtin-*` aliases registered (246→263 builtins). stdlib modules still use primary operator names vulnerable to shadowing.

- [x] Rewrite `stdlib/async.llt` to use `builtin-*` stable aliases — if→builtin-if, raise→builtin-raise, -→builtin-sub
- [x] Rewrite `stdlib/codecs/json.llt` to use `builtin-*` stable aliases — str→builtin-str, if→builtin-if, =→builtin-eq, <→builtin-lt, +→builtin-add, raise→builtin-raise
- [x] Rewrite `stdlib/codecs/toml-lite.llt` to use `builtin-*` stable aliases — if→builtin-if, =→builtin-eq, +→builtin-add, -→builtin-sub
- [ ] `just test` passes — **UNVERIFIED** (Bash unavailable)

### type-unknown-audit: Audit Type::Unknown in builtin signatures

24+ builtin signatures use `Type::Unknown` without justification. Policy: Unknown must be justified or replaced.

- [x] Audit all 21 `Type::Unknown` in builtin signatures — all justified, 8 comments added (`src/type_env.rs`)
- [ ] `just test` passes — **UNVERIFIED** (Bash unavailable)

---

## I/O Builtins (cap-std gaps)

### handle-parameterization: Parameterize Type::Handle with capability row

**Context:** `lib-net-v2` and `io` specs document `Handle[Binary Readable Writable Stream Tls]` notation throughout, but the implementation uses a monolithic `Type::Handle` atom with no row parameter. `Handle` alias has `params: vec![]`. All builtins use `Type::Handle` as-is; `@Handle` annotation works but has no capability checking. Parameterization was deferred as "BAS sprint" in the 2026-05-09 lib-net-v2 audit.

- [x] Change `Type::Handle` → `Type::Handle(Box<Type>)` with capability row; updated all 25+ construction/match sites across type_def.rs, type_normalize.rs, type_unify.rs, type_env.rs, imports.rs, eval.rs
- [x] All existing signatures use `Type::Handle(Box::new(Type::Unknown))` for gradual typing backward compat
- [x] Updated `doc/feature/io.md` and `doc/feature/lib-net-v2.md`
- [ ] Register capability tags (`Binary`, `Readable`, etc.) as type-level symbols — **DEFERRED** (gradual typing works for now)
- [ ] Precise builtin signatures (open→Handle[Readable], slurp requires Readable) — **DEFERRED** (needs capability tag registration first)
- [ ] Corpus tests for capability mismatches — **DEFERRED** (needs precise signatures)
- [ ] `just test` passes — **UNVERIFIED**

### io-cap-std-gaps: Add symlink, copy-file, set-permissions, stat-symlink, exists builtins

Five operations cap-std's `Dir` supports that tinct does not yet expose.

**`symlink`** — create a symbolic link within a DirCap. tinct can read and detect symlinks (`read-link`, `stat` `is-symlink` field) but not create them. Needs a new `Symlinkable` DirCap flag.

- [x] Add `Symlinkable` to DirPerms + `narrow` + `--cap-fs` parser (`y` shorthand) (`src/value.rs`, `src/builtins_io.rs`, `src/main.rs`)
- [x] Implement `symlink` builtin + register — Unix/Windows platform dispatch (`src/builtins_io.rs`, `src/builtins.rs`)

**`copy-file`** — efficient within-cap file copy via `Dir::copy()` (uses `copy_file_range` on Linux). Currently programs must `slurp` + `write`, allocating the whole file as a String. Requires `Readable` on source cap, `Writable` on destination; no new flag.

- [x] Implement `copy-file` builtin + register (`src/builtins_io.rs`, `src/builtins.rs`)

**`set-permissions`** — chmod via `Dir::set_permissions()`. On Unix, requires the process to own the file or hold `CAP_FOWNER`; cap-std uses `fchmodat(dirfd, path, mode, 0)` which follows symlinks (Linux does not support `AT_SYMLINK_NOFOLLOW` for chmod). Not all filesystems support POSIX permissions (e.g., S3-backed DirCaps, FAT volumes, some FUSE mounts would not).

**Decision:** New `PosixPermissions` DirCap flag — not `Writable`, because write authority and permission-bit authority are orthogonal (you can have a writable DirCap on a filesystem that has no POSIX permission concept). `PosixPermissions` explicitly signals that the underlying filesystem supports POSIX mode bits.

- [x] Add `PosixPermissions` to DirPerms + `narrow` + `--cap-fs` parser (explicit-only) (`src/value.rs`, `src/builtins_io.rs`, `src/main.rs`)
- [x] Implement `set-permissions` builtin + register — Unix-only with platform error (`src/builtins_io.rs`, `src/builtins.rs`)

**`stat-symlink`** — lstat equivalent via `Dir::symlink_metadata()`. The existing `stat` follows symlinks; `stat-symlink` does not. Lets programs inspect a broken symlink without error. Same `Statable` flag as `stat`.

- [x] Implement `stat-symlink` builtin + register (`src/builtins_io.rs`, `src/builtins.rs`)

**`exists`** — existence check via `Dir::try_exists()`. Cheaper than `try`+`stat`; distinguishes "not found" (`false`) from "permission denied" (error). Requires `Statable` flag.

- [x] Implement `exists` builtin + register (274→275) (`src/builtins_io.rs`, `src/builtins.rs`)

**`get-xattr` / `set-xattr` / `remove-xattr` / `list-xattrs`** — POSIX extended attributes. Not part of cap-std's `Dir` API; implemented by opening the file via the DirCap (getting an fd) then calling `fgetxattr`/`fsetxattr`/`fremovexattr`/`flistxattr` on the fd — this preserves the capability model (no ambient path access). Linux only (macOS has xattrs but different syscall convention; Windows has alternate data streams, not xattrs). Requires a new `ExtendedAttributes` DirCap flag following the `PosixPermissions` naming pattern: the flag asserts the underlying filesystem supports xattrs (ext4, btrfs, tmpfs do; FAT, some network filesystems do not).

**Decision:** `ExtendedAttributes` DirCap flag — no shorthand, explicit `[ExtendedAttributes]` only.

- [x] Add `ExtendedAttributes` to DirPerms + `narrow` (`src/value.rs`, `src/builtins_io.rs`)
- [x] Add `xattr = "1"` crate to Cargo.toml, gated behind `cfg(target_os = "linux")`
- [x] Implement `get-xattr`, `set-xattr`, `remove-xattr`, `list-xattrs` builtins — Linux impl + non-Linux stubs (279→283 builtins) (`src/builtins_io.rs`, `src/builtins.rs`)

**Shared finishing tasks:**
- [x] Update `doc/11a-builtins.md` — added xattr section, DirCap flags table, updated counts to 283
- [x] Corpus tests: exists, exists_missing, stat_symlink, copy_file (`tests/corpus/eval/builtins/`)
- [ ] `just test` passes — **UNVERIFIED**

---

## CHR (Constraint Handling Rules)

Whatif: `doc/whatif/chr-unification.md` (Accepted 2026-05-16). Implementation chain: chr-module-split ✅ → chr-normalization ✅ → chr-instances-gaps (class-instance + prelude + gap fixes). Then type-inference-cleanup follows.

### chr-instances-gaps: Wire ClassDecl into constraint generation + prelude updates + CHR gap fixes

**Whatif:** `chr-unification`
**Depends on:** chr-normalization ✅
**Audit source (chr-gaps):** mempalace tinct/decisions "CHR MIGRATION AUDIT — GAPS FOUND 2026-05-17"

Do in order within the sprint: class-instance → prelude → gaps.

#### class-instance: Wire ClassDecl into constraint generation and instance lookup

- [x] Restructure `Constraint::Class` — changed `class: String` → `class: Arc<ClassDecl>`, removed fundeps field. Updated ~20 sites across type_class.rs, type_infer.rs, type_env.rs, type_unify.rs, typecheck_annot.rs, typecheck.rs, type_unify_tests.rs
- [x] FD info now comes from ClassDecl.determines — fundeps: vec![] hardcoding eliminated
- [x] MPTC general lookup wired into `improve_functional_dependency` — fallback path now calls `state.instance_env.lookup_mptc()` instead of returning error (`src/type_unify.rs`)
- [x] Corpus tests: class_fd_fires, mptc_lookup, add_fd_end_to_end (`tests/corpus/eval/typecheck/`)
- [ ] Verify ClassDecl.determines populated + `just test` passes — **UNVERIFIED**

#### prelude: Update prelude class declarations to match CHR design

- [x] Rename arithmetic classes Add→Addable, Sub→Subtractable, Mul→Multipliable, Div→Divisible — **ALREADY DONE** in prior sprint (prelude, eval.rs, type_env.rs, type_infer.rs, type_unify.rs, builtins_math.rs all updated)
- [x] Restore Equatable, Comparable, Showable class/instance declarations — **ALREADY DONE** (active in prelude; PRELUDE_INSTANCE_CACHE is a legitimate optimization, not a workaround)
- [x] Wire `resolver:` key in `ClassDecl` — **ALREADY DONE** (typecheck.rs:2978-3012 extracts resolver from class dict)
- [x] Type-stage resolver evaluation — **ALREADY DONE** (type_normalize.rs:123-152 calls evaluate_resolver())
- [ ] `just test` passes — **UNVERIFIED**

#### gaps: Fix critical CHR implementation gaps (from 2026-05-17 audit)

**Gap 1 — Type-stage resolver evaluation:** ✅ ALREADY DONE (normalize calls evaluate_resolver; improve_functional_dependency calls lookup_mptc fallback)

**Gap 2 — FD fundep indices:** ✅ Fixed by Constraint::Class → Arc<ClassDecl> restructure

**Gap 3 — MPTC instance lookup:** ✅ Fixed — general lookup_mptc wired into improve_functional_dependency

**Gap 4 — `resolver_injective` flag:** ✅ Already fully wired (parser→AST→typecheck→ClassDecl). Field written but never read — read site is future CHR congruence work. `#[allow(dead_code)]` is correct.

**Gap 5 — `lookup_mptc` HKT instance heads:** ✅ Fixed — rewrote to use structural unification (instantiate_at_level + unify) instead of string-key matching. Now consistent with resolve_instance. (`src/type_class.rs`, `src/type_unify.rs`)

**Gap 6 — instance declaration ordering:** ✅ Corpus test written (`tests/corpus/eval/typecheck/instance_ordering.llt-eval`) — verifies Pass 0c pre-registration allows function on line 5 to use class/instance declared on line 20+.

**Gap 7 — constraint propagation through HOF args:** ✅ Corpus test written (`tests/corpus/eval/typecheck/constraint_hof_propagation.llt-eval`) — documents actual behavior (eval succeeds; constraint checked at lambda definition site, not inside HOF body).


- [x] End-to-end test: add_fd_end_to_end.llt-eval — `[+ 1 2.0]` infers Float via FD (`tests/corpus/eval/typecheck/`)
- [ ] `just test` passes

### type-inference-cleanup: TypeStageApp deferral + T013 readability + Unknown elimination

**Depends on:** chr-instances-gaps (provides chr-prelude and chr-class-instance, needed by deferral wiring and Unknown elimination)

#### TypeStageApp deferral (formerly chr-typestageapp-deferral)

`process_deferred_equalities` in `src/type_unify.rs:2135` has `#[allow(dead_code)]` referencing "future TypeStageApp deferral sprint (doc/06-type-inference.md:884)". This function handles deferred equality constraints that arise when TypeStageApp normalization is stuck (resolver not yet callable). Without deferral, stuck TypeStageApps cause premature unification failures.

- [x] Wire `process_deferred_equalities` into inference loop — called after SCC merge in typecheck_dict.rs:550-554, gated by !deferred_equalities.is_empty()
- [x] Removed `#[allow(dead_code)]` from `process_deferred_equalities`
- [ ] `just test` passes — **UNVERIFIED**

#### T013 warning readability (formerly type-warning-readability)

T013 warnings currently report internal inference variable names like `_t86` instead of the user-visible source variable that introduced the constraint. Example: `warning[T013]: ambiguous type variable '_t86' in constraint Showable: appears in constraint but not in the type — constraint will be silently dropped`. The user cannot tell which declaration or expression introduced `_t86`.

- [x] T013 warnings now show source variable names — added `type_var_source_names` map to InferState, `format_var_name` helper, updated all T013 emitters in type_env.rs
- [x] Corpus test: `tests/corpus/typecheck/warnings/t013_ambiguous_with_source_name.llt-eval`

#### Unknown elimination (formerly unknown-elimination)

`Type::Unknown` currently leaks into inferred types for expressions the type checker cannot handle — dual-dispatch builtins, HKT positions, gradual typing escape. Goal: make Unknown a controlled escape hatch, not a default fallback. HKT infrastructure (Kind::Operator, Type::App, Mappable/Appendable) is complete — residual Unknown at HKT positions is an instance-lookup gap covered by chr-instances-gaps, not missing HKT machinery.

- [x] Audited all 124 `Type::Unknown` in typecheck.rs — 56 gradual (justified), 0 replaceable, 4 HKT deferred. All sites annotated with inline `// Gradual:` or `// HKT:` comments.
- [ ] `just test` passes — **UNVERIFIED**

