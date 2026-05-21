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

### Parts B + E — Parser, resolver, typechecker, expander migration + Evaluator cutover (ATOMIC)

**Parser** (`src/parser.rs`): Bridge method added — `ParseOutput::as_surface_program()` converts File → SurfaceProgram on demand. Full parser migration (constructing SurfaceNode directly) deferred to Part E.
- [x] Change `parse()` return type from `File` to `SurfaceProgram`; every `Rc::new(spanned_expr)` → `Arc::new(SurfaceNode { expr, span })`; declaration forms route to `SurfaceItem::Decl`, expression forms to `SurfaceItem::Expr` (`src/parser.rs`) — **Bridge approach**: added `ParseOutput::as_surface_program()` method; full parser internals migration deferred to Part E

**Resolver** (`src/resolve.rs`): `resolve_surface_program()` stub added; parallel calls at all 12 call sites (6 in lib.rs, 6 in main.rs); resolution table computed but unused until Part E wires it in.
- [x] Route all callers of `resolve_file()` to `resolve_surface_program()` — once parser returns SurfaceProgram (`src/resolve.rs`, `src/lib.rs`, `src/main.rs`) — **Done**: parallel `_resolution_table` calls added alongside existing `resolve_file()` calls

**Typechecker** (`src/typecheck.rs`): ❌ NOT STARTED
- [ ] Walk `SurfaceProgram` instead of `File`; produce `TypeAnnotationTable` instead of mutating `TypeAssert.resolved_type: RefCell<...>` (`src/typecheck.rs`)

**Expander** (`src/expand.rs`): ❌ NOT STARTED
- [ ] Change all `Expr::Variant` match arms → `SurfaceExpression::Variant` (`src/expand.rs`)
- [ ] `expand_document()` walks `SurfaceDocument.items`; `SurfaceDeclaration::Splice` flattened to `SurfaceItem::Expr` (`src/expand.rs`)
- [ ] Macro round-trip: `ast_to_dict_expr` → `surface_expr_tag` + `surface_node_get_field` (`src/expand.rs`)
- [ ] `surface_node_from_value(v: &Value, ctx: &Arc<EvalContext>) -> Result<Arc<SurfaceNode>, MacroError>` in `src/surface_fields.rs` — macro output reconstruction (`src/surface_fields.rs`)

**Part D remaining** (`src/surface_fields.rs`):
- [ ] Sequence fields in `surface_node_get_field()` — `[Seq Expression]` results (needs ThunkId allocation available in Part E env) (`src/surface_fields.rs`)
- [ ] `span_to_value()` with full Span Dict encoding (`src/surface_fields.rs`)

**Part E — Evaluator cutover + delete old types + Rc→Arc:**
- [ ] Delete from `src/ast.rs`: `Expr`, `Document`, `File`, `VarRef.resolved: RefCell`, `TypeAssert.resolved_type: RefCell`; `Annotation::PropertyDict` → `Vec<Spanned<SurfaceEntry>>`; `SurfaceMatchArm.pattern` → `Arc<SurfaceNode>` (`src/ast.rs`)
- [ ] Update all eval files to use `CoreExpr` (not `Expr`): `eval.rs`, `eval_materialize.rs`, `eval_dict.rs`, `eval_call.rs`, `eval_access.rs`; add `CoreExpr::FreeVar` arm (name-based env lookup); add `CoreExpr::RuntimeTypeCheck` arm (`src/`)
- [ ] Delete: `src/eval_pipeline.rs`, `src/eval_deep.rs`, `src/ast_dict.rs`, `src/desugar.rs`, `src/ast_convert.rs` (bridge) (`src/`)
- [ ] Update `IncludeCacheEntry::Cached` to carry `Arc<ResolutionTable>` + `Arc<TypeAnnotationTable>` — needed by Part F `builtin_eval` to retrieve tables for `Surface` thunks (`src/imports.rs`)
- [ ] Rc→Arc migration: `Rc<Thunk>` → `Arc<Thunk>`, `Rc<RefCell<Environment>>` → `Arc<RwLock<Environment>>`, `Rc<EvalConfig>` → `Arc<EvalConfig>`, `ThunkState` → `(Mutex<Option<UnevaluatedState>>, OnceCell<Result<Value, Arc<EvalError>>>)` pair; add tokio dependency (`Cargo.toml`, all src/ files)
- [ ] **`cargo check` clean after Part E** — first checkpoint per plan

### Part F — Update builtins to use Program/Expression types

**Depends on:** Part E complete

- [ ] `builtin_load` → parse → `SurfaceProgram` → `Value::Program` (`src/builtins_meta.rs`)
- [ ] `builtin_expand` → unwrap `Value::Program` → run expansion → wrap `Value::Program` (`src/builtins_meta.rs`)
- [ ] `builtin_eval` → iterate `[Seq Expression]`; per `Value::Expression(node)`: get `(res, types)` from `IncludeCacheEntry`; create `UnevaluatedState::Surface`; return lazy thunks (`src/builtins_meta.rs`)
- [ ] `builtin_eval_types` → same as `eval` but uses `ctx.config.type_stage_env` as base env (`src/builtins_meta.rs`)
- [ ] Delete `eval-ast` builtin and `builtin_eval_ast` (`src/builtins_meta.rs`)
- [ ] `Value::Task`, `Value::Channel`, `Value::Context` — add to `src/value.rs` (Sprint 2 dependency; skeleton now)

### Part G — Prelude pipeline update + type declarations

**Depends on:** Part F complete. **Structure unchanged; types updated** (per `runtime-v2.md §Updated Include-Decomp Tinct Code`).

- [ ] `eval-file`: annotation `ast@Dict` → `ast@Program` (`stdlib/prelude.llt`)
- [ ] `eval-document-runtime`: `doc.name` → `DocumentName` match (`[match doc.name [Named n]: ... Unnamed: ...]`); `doc.expressions` is now `[Seq Expression]` passed directly to `eval` builtin; `doc.stage` matched as `Runtime`/`Type` Variants (`stdlib/prelude.llt`)
- [ ] `eval-document-pipeline`: annotation `docs@[Seq Document]`; `doc.stage` match as Variants (`stdlib/prelude.llt`)
- [ ] `stdlib/cli/fmt/compact.llt` and `pretty.llt` — update to Expression match dispatch; formatter bug fix is prerequisite OR accept broken formatter tests (`stdlib/cli/fmt/`) (16 tests in `tests/formatter_tinct_roundtrip.rs` are marked `#[ignore]` pending this)
- [ ] `Pattern` type declaration in prelude — `Pattern: Expression` alias; investigate dict shape issues post-Part E before adding (`stdlib/prelude.llt`)
- [ ] Create `stdlib/codecs/json.llt` final version if not already migrated — verify `to-json` Expression match dispatch works end-to-end (`stdlib/codecs/json.llt`)
- [ ] **`just test` after Part G** — highest-risk: corpus tests for `load`/`expand`/`eval` may fail on type changes; fix as found

---

## runtime-v2 — Sprint 2: Async Runtime

❌ NOT STARTED. Depends on Sprint 1 Part E complete.

- [ ] Rc→Arc migration throughout; `ThunkState` → `OnceCell` pair; add tokio dependency
- [ ] `eval`/`materialize` → `async fn`; multi-thread Tokio runtime
- [ ] `task`, `await`, `await-all`, `channel`, `send`, `recv`, `select-once`, `par`, `par-map`, `par-filter`
- [ ] `context`, `with-cancel`, `with-timeout`, `cancel-task`, `cancel-root`, `drain`
- [ ] `signal-channel`, `timer-channel`, `watch-channel`
- [ ] `Value::Task`, `Value::Channel`, `Value::Context`
- [ ] `stdlib/async.llt`

See `doc/whatif/plans/runtime-v2-plan.md` Sprint 2 for full task list.

---

## runtime-v2 — Sprint 3: Stdlib and Cleanup

❌ NOT STARTED. Depends on Sprint 2 complete.

- [ ] `stdlib/desugar.llt` — `$_` implicit lambda desugaring as tinct surface pass
- [ ] `stdlib/codecs/json.llt` `from-json` — replace Rust builtin with pure-tinct `json-parse-value` implementation (str-at/str-slice available post-rebase)

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

- [ ] `doc/08-evaluation.md:358` — still says `DepthExceeded` "no longer arises from the core materialize/eval loop" and "can only be raised by individual builtins" — contradicts the corrected line 278 (which correctly notes it CAN arise from `check_stack_depth()` inside the CEK loop); fix the same sentence at line 358 (`doc/08-evaluation.md`)
- [ ] `tests/corpus/eval/typecheck/constructor_payload_type_precision.llt-eval` — corpus test overly loose: `[+ p.n 1]` passes even when `p.n` is `Unknown` (arithmetic on Unknown uses consistency, not subtype); replace with `[@Int p.n]` or `[[fn [x@Int] x] p.n]` which IS rejected when `p.n` is Unknown — ensures regression in `collect_pattern_bindings` Intersection arm would be caught (`tests/corpus/eval/typecheck/constructor_payload_type_precision.llt-eval`)
- [ ] Two independent `GENSYM_COUNTER` statics with inconsistent `Ordering`: `eval_pipeline.rs:182` uses `Relaxed` but `builtins_meta.rs:446` uses `SeqCst`; standardize to `Relaxed` (counter uniqueness doesn't require cross-thread ordering; `Relaxed` is sufficient) (`src/eval_pipeline.rs:182`, `src/builtins_meta.rs:446`)
- [ ] `validate_value` silently skips `Value::Seq` for `items` constraints — add a branch to validate Seq elements against the `items` schema entry (`src/builtins_meta.rs:2303-2305`)
- [ ] Informal `~40% faster` benchmark claim — either add a real criterion.rs benchmark citation or rephrase as "avoids O(n) repeated key collection" (`src/eval_deep.rs:64`)

---

## Known Bugs

- [x] `just test-lib` fails with exit 101. **Fixed (2026-05-19):** Root cause was a parser bug in `src/parser.rs`: `pop_last_value_from_frame` in the `Token::Dot` handler correctly handles `CallArg::Positional` but returns a parse error for `CallArg::Named`. This caused `%: state.prev` in `eval-document-runtime` to fail — the `%` was consumed as the named-arg key, then `state` was consumed as its value (before `.prev` was attached via dot-access). Fix: replaced `%: state.prev` with a let-binding `[prev-val: state.prev]` + `%: prev-val` in `stdlib/prelude.llt::eval-document-runtime`. Also added `expand_macros_in_ctx` to `src/expand.rs` (reuses existing stdlib env when `builtin_expand` is called from within evaluation, eliminating the redundant stdlib reload). All 4 `test_syntax_llt_fn_*` tests pass; `just test-lib` exits 0.

- [ ] **`test_eval_corpus` SIGKILL (OOM) after runtime-v2-rebase additions:** `test_eval_corpus` in `tests/corpus_tests.rs` is killed by the OOM killer (signal 9) after 60+ seconds of running 500+ corpus tests. Root cause is unknown but confirmed to be cumulative memory growth over 500+ test iterations in the shared `ThunkArena` (allocated during `eval_source_with_config` and `typecheck_source_errors_only` for each test). Investigation showed per-test allocation should be <200KB but the OOM fires after ~275 tests (suggesting ~29MB/test somehow). Possible causes: (1) large `ast_to_dict` output accumulation in shared arena from included stdlib files (strings.llt, json-pretty.llt, toml-lite.llt), (2) `deep_materialize` thunks accumulating across tests, (3) interaction between new prelude type declarations (Expression with 23 variants, 12 other type aliases) and the shared arena pattern. Partial mitigation: removed dead `as_surface_program()`/`_resolution_table` calls from `lib.rs` (5 occurrences) since these were dead code. Full fix requires investigating the shared arena growth pattern — either make per-test eval use `clone_for_child` (requires also fixing ThunkId cross-arena access in the include pipeline) or reduce prelude memory footprint. `just test-lib` still passes; only `just test-corpus` fails. (`tests/corpus_tests.rs`, `src/lib.rs`, `src/arena.rs`)

- [ ] **CLI Seq tests OOM:** `seq_with_collect_produces_json_array` and `seq_at_top_level_with_emit_and_none_output` fail with "memory allocation of 360 bytes failed" — likely infinite recursion or excessive memory usage in `collect`/`range` evaluation within the 8GB container. Tests use `[call $collect [call $range 0 3]]` which should be bounded. Investigate stack depth or prelude `collect`/`range` wrapper issues. (`tests/cli_tests.rs`)

- [ ] **Parser bug (tracked for fix):** `named-arg: expr.field` patterns silently misbehave — `pop_last_value_from_frame` in `Token::Dot` handler (src/parser.rs ~line 4434) returns a parse error when it pops a `CallArg::Named` instead of the expected `CallArg::Positional`. Parser recovers by closing the call frame prematurely, producing a wrong AST. Fix: when `pop_last_value_from_frame` finds a `CallArg::Named`, it should NOT pop it as the dot-access target — instead, the dot-access should apply to the named arg's value, or the parser should accumulate DotAccess before consuming the named arg value (`src/parser.rs`).

### Known Bugs (Type Checker)

- [x] `typecheck::tests::test_dot_access_intersection_found` — Intersection type unification bug: fixed by adding `(Type::Record(..), Type::Intersection(..))` arm to `src/type_unify.rs` that distributes unification across intersection members. Tests pass.
- [x] `typecheck::tests::test_dot_access_intersection_missing_field_returns_unknown` — same Intersection unification bug; fixed by same arm. Tests pass.

---

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class: `∀f a. Filterable f ⇒ (a → Bool) → f a → f a`; compare `Mappable` extension vs separate class using `Data.Witherable` as precedent; include instance examples for `Seq` and `Dict` (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint in TODO.md for `from-json @Schema` schema-directed typed parse (`doc/whatif/schema-directed-from-json.md`)

---

## include-decomp Regression (from runtime-v2 merge)

The runtime-v2 branch was branched before include-decomp landed. The PR #1 merge (2026-05-20) reintroduced old code that include-decomp had deleted. Review: `/review-whatif include-decomposition` confirmed these regressions.

### include-decomp-redelete: Re-delete code regressed by runtime-v2 merge

**Whatif:** `include-decomposition`
**Review:** All include-decomp sprints are DONE but runtime-v2 merge reverted the deletions.

- [ ] Delete `builtin_include` from `src/builtins_meta.rs` (the entire function, ~350+ lines) and remove its registration from `standard_builtins()` (`src/builtins.rs`) — already deleted once in include-decomp-prelude sprint, reintroduced by runtime-v2 merge
- [ ] Delete `Value::RustRegistry` from `src/value.rs` and all match arms that handle it (`src/value.rs`, `src/lib.rs`, `src/builtins.rs`)
- [ ] Delete `rust_module()` and all module grouping (`"core"`, `"io"`, `"net"`, etc.) from `src/builtins.rs`
- [ ] Delete `EvalState::include_guard: HashSet<(u64, u64)>` from `src/eval.rs` (replaced by `[Pending]` cache state in tinct)
- [ ] Delete old inode-keyed `include_cache` from `src/eval.rs` (replaced by content-addressed cache in tinct)
- [ ] Delete `src/eval_pipeline.rs` entirely (`eval_file_with_input`, `eval_document`, `run_eval`) and update all callers in `src/main.rs`, `src/lib.rs`, `src/repl.rs`, `src/formatter.rs` to use the tinct `cli-pipeline`/`eval-file` functions via `invoke_function`
- [ ] Verify `expand` builtin in `src/builtins_meta.rs` performs real macro expansion (currently identity stub) — use `dict_to_file` → `expand()` → `ast_to_dict` round-trip as specified in whatif
- [ ] After deletions: `just test` must pass; `just test-lib` must pass (`src/builtins_meta.rs`, `src/eval.rs`, `src/main.rs`)

---

## Health Review #22 Findings (2026-05-19)

### grammar-doc-polish: Fix grammar/doc consistency issues

- [ ] **CRITICAL** `doc/feature/macros.md:176` incorrectly states `[defmacro ...]` produces the same AST node as `[macro ...]` — fix: defmacro produces `Expr::DefMacro` (Variant tag "DefMacro"), macro produces `Expr::MacroDecl` (Variant tag "MacroDecl"); distinct serialized tags (`doc/feature/macros.md:176`)
- [ ] **REOPENED** `stdlib/ast.llt:29` — `Literal.bare` is typed `Bool` for ALL literal kinds but `bare` is only emitted for `kind:"str"` nodes; should be `[Bool Null]`; prior fix only changed the comment, not the type definition (`stdlib/ast.llt:29`)
- [ ] `stdlib/ast.llt:57` — `DefMacro.params` described as `[Seq Unknown]` but the field is a single `Expr` (a LetDecl node, not a sequence); fix: `[DefMacro name: String  params: Expr  body: Expr]` (`stdlib/ast.llt:57`)
- [ ] `doc/feature/ast-schema.md:259-271` — Document schema omits the `stage:` field which is always emitted as `Variant("Runtime"|"Type")`; any code building a document node from the schema spec will produce output that can't round-trip through `dict_to_file` (`doc/feature/ast-schema.md:259-271`, `src/ast_dict.rs:182-195`)
- [ ] `ClassDecl.superclasses` is silently dropped by `ast_to_dict` (field set to `_` at `src/ast_dict.rs:675`) — formatter/macro code reconstructing ClassDecl loses superclass declarations; design decision needed on schema representation before implementation (`src/ast_dict.rs:675`)
- [ ] `doc/02-syntax.md:733` — `defmacro` example uses bare `[pred body]` params without `[let ...]`; verify this still works at evaluation time (`doc/02-syntax.md:733-740`)

### stdlib-doc-polish: Fix stdlib documentation and coverage gaps

- [ ] **CRITICAL** `doc/11-stdlib.md:296-308` §Loading mechanism is completely stale — still describes `[include %rust "core"]` mechanism that was deleted; rewrite to describe actual post-include-decomp bootstrap (all builtins pre-injected by `create_root_env()`/`create_stdlib_env_inner()`) (`doc/11-stdlib.md:296-308`)
- [ ] **CRITICAL** `doc/11-stdlib.md:236` §Evaluation control table lists `error` as Rust builtin — actual Rust builtin is `raise`; `error` is now a prelude alias (`error: raise` at prelude.llt:1886); update table to `eval, raise, try, apply` with note about `error` alias (`doc/11-stdlib.md:236`)
- [ ] **CRITICAL** `doc/11-stdlib.md:239` claims `include` is a Rust builtin — it is now a pure-LLT function in prelude.llt:2462; remove from I/O row; add section documenting the 8 thin Rust primitives (`load`, `expand`, `eval`, `eval-types`, `blake3`, `cap-identity`, `include-cache-get`, `include-cache-put`) (`doc/11-stdlib.md:239`)
- [ ] **CRITICAL** `doc/11-stdlib.md:308` `builtin-*` aliases accessibility claim is stale — "only accessible via `[include %rust "core"]`" is false; they are pre-injected by `create_stdlib_env_inner` and accessible to prelude via env chain without any include (`doc/11-stdlib.md:308`)
- [ ] `upper`/`lower` have zero corpus tests — add `tests/corpus/eval/stdlib/upper_basic.llt-eval`, `upper_unicode.llt-eval`, `lower_basic.llt-eval`, `lower_unicode.llt-eval` covering multi-byte codepoints (`tests/corpus/eval/stdlib/`)
- [ ] `pick` has zero corpus tests — add `pick_basic.llt-eval`, `pick_missing_key.llt-eval`, `pick_empty.llt-eval` (`tests/corpus/eval/stdlib/`)
- [ ] `format-instance` in compact.llt/pretty.llt emits `<N arm(s)>` placeholder instead of formatting arms — stub produces unparseable output; needs full arm formatting implementation (`stdlib/cli/fmt/compact.llt:276-283`, `stdlib/cli/fmt/pretty.llt:444-451`)
- [ ] `doc/11-stdlib.md:314` — Optional modules table for strings.llt missing `upper` and `lower` (`doc/11-stdlib.md:314`)
- [ ] `prelude.llt:1423-1429` — `result-ok` and `and-then` have `fn@[return: Unknown]` annotation; should be `fn@[return: Result]` or similar (`stdlib/prelude.llt`)
- [ ] `prelude.llt:2295-2297` — `first-or` uses `[match [empty? xs] [case true ...]]` boolean dispatch; replace with `[if [empty? xs] default [first xs]]` for consistency with prelude style (`stdlib/prelude.llt:2295-2297`)
- [ ] `strings.llt` — `str-reverse-impl`, `upper`, `lower` use bare annotated params without `[let ...]`; inconsistent with `[fn@T [let ...]]` style used throughout prelude.llt; standardize or document the new-style param choice (`stdlib/strings.llt:47,94,100`)

### integration-fixes: Cross-layer integration issues

- [x] **MAJOR** `ast-of` builtin (`builtin_ast_of`) is defined in `src/builtins_meta.rs:790` but NOT registered in `standard_builtins()` — **FIXED by runtime-v2-rebase Phase 8**: now registered, returns `Value::Expression`, corpus tests updated (`src/builtins.rs`, `tests/corpus/eval/builtins/ast_of_*.llt-eval`)
- [ ] Add `Value::Expression` arm to `value_to_expr` in `src/eval.rs:1787` via `ast_convert::surface_node_to_expr()` — needed for `[unquote (ast-of x)]` to work correctly; currently `Value::Expression` falls through to the Placeholder arm producing `Expr::Placeholder` instead of the wrapped expression (deferred to Part D) (`src/eval.rs:1787`, `src/ast_convert.rs`)
- [ ] **MAJOR** `dict_to_ast` Variant path calls `try_get_materialized()` on payload thunk — fails for any Variant constructed via `[variant "Tag" payload]` in LLT (payload is lazy); macros producing Variant AST nodes cannot pass them to `eval-ast` until payload is materialized; document this constraint explicitly in `dict_to_ast` doc comment (`src/ast_dict.rs:1468-1486`)
- [ ] Add RAII depth guard for `EXPAND_MACROS_DEPTH` in `src/expand.rs:378-406` — a panic inside `create_stdlib_env_with_arena()` leaves depth stuck at 1, causing all subsequent `expand_macros` calls to use bare root env silently (`src/expand.rs:381-405`)
- [ ] `dict_to_file` only accepts `Value::Dict` for file root (not `Value::Variant`) while `dict_to_ast` accepts both — asymmetry is currently correct but undocumented and fragile; add doc comment (`src/ast_dict.rs:2256-2265`)
- [ ] `do_infer_resolutions` not wired when typecheck is skipped — `[do]` inferred forms fail at runtime with undefined-variable for `:do-infer:N` sentinel; add comment documenting this expected degraded behavior (`src/lib.rs:279-280`, `src/eval.rs:785-800`)

### test-corpus-fixes: Fix broken/stale corpus tests

- [ ] **CRITICAL** Remove stale `=== warn: T010` sections from: `do_minimal.llt-eval:3-5`, `do_hardcoded.llt-eval:4-5`, `list_dir.llt-eval:2-3` — the eval corpus runner calls `typecheck_source_errors_only` which never produces T010 warnings so these never match; causes test failures (`tests/corpus/eval/macros/`, `tests/corpus/eval/builtins/`)
- [ ] **CRITICAL** `begin_basic.llt-eval:4` has wrong `=== out` format — expected `3` but `DisplayVisitor::visit_int` formats as `Int(3)`; change to `Int(3)` (`tests/corpus/eval/macros/begin_basic.llt-eval`)
- [ ] **CRITICAL** Remove stale `=== warn` block from `macro_expansion_provenance.llt-eval:13-18` — contains hardcoded builtins.rs span `691:` that drifts; `[E001]` is a runtime error that `typecheck_source_errors_only` cannot produce (`tests/corpus/eval/errors/macro_expansion_provenance.llt-eval`)
- [ ] **CRITICAL** Remove copypaste `=== warn` sections from `macro_syntax_class_validation.llt-eval:11-12` and `macro_error_provenance_placeholder.llt-eval:21-22` — `[E012]`/`[E080]` are runtime error codes, not typecheck warnings; never match (`tests/corpus/eval/macros/`)
- [ ] Add `NominalVariant` exhaustive-match happy-path test — create `tests/corpus/eval/typecheck/nominal_variant_exhaustive_match.llt-eval` with a `[type [Circle r: Int] [Square s: Int]]` declaration and fully exhaustive `[match]`; no `=== warn` section asserts zero warnings (`tests/corpus/eval/typecheck/`)
- [ ] Rename/move `macro_in_include.llt-eval` from `eval/macros/` to `eval/errors/include_removed_gives_e002.llt-eval` — tests that a removed feature errors, not macro behavior (`tests/corpus/eval/macros/`)
- [ ] Add `[macro]` keyword syntax error tests: `macro_missing_name.llt-eval`, `macro_non_let_pattern.llt-eval`, `macro_missing_body.llt-eval` (`tests/corpus/invalid/syntax_errors/`)
- [ ] Add `eval/macros/` minimum count to `test_corpus_structure` — add `const EVAL_MACROS_MIN: usize = 40;` (`tests/corpus_tests.rs:229-265`)
- [ ] Add `eval_error_propagation.llt-eval` and `eval_types_multiple_exprs.llt-eval` — current `eval_basic`/`eval_types_basic` only test trivial integer literal; mutation-blind (`tests/corpus/eval/builtins/`)
- [ ] Pin `expand_basic.llt-eval` more precisely — currently only checks `.type == "file"`; add a test that accesses the `exprs` field to actually verify expansion result structure (`tests/corpus/eval/builtins/`)
- [ ] Add comments to `do_three_step`, `do_nonbinding_step`, `do_no_steps` explaining why `=== warn: undefined variable: result` is correct (macro expansion resolves it at eval time) (`tests/corpus/eval/macros/`)
- [ ] Add span to `ast_of_fn_no_force.llt-eval:7` warning assertion — `undefined variable: ast-of` too loose; add `at 1:9` span (`tests/corpus/eval/builtins/`)
- [ ] Add legacy comment to `macro_lib.llt:7` — `[defmacro]` is intentional for backward-compatibility regression testing (`tests/corpus/eval/macros/macro_lib.llt`)

### security-sprint: Fix security regressions from include-decomp

- [ ] **CRITICAL** `--require-integrity` is completely non-functional after include-decomp-prelude deleted `builtin_include` — `EvalError::include_hash_mismatch`/`include_hash_required` exist but are never called; self-hosted `include` in `stdlib/prelude.llt` never checks `ctx.config.require_integrity`; implement integrity checking: add `hash:` named arg to `builtin_load` for the expected blake3 hash, verify in the self-hosted `include` function before returning the cached/evaluated result, raise `EvalError::include_hash_mismatch` on mismatch (`src/builtins_meta.rs:1685-1687`, `stdlib/prelude.llt:2462-2477`)
- [ ] **CRITICAL** Path traversal in macro pre-scan: `pre_scan_follow_libdir_include` reads files via `std::fs::read_to_string(libdir_path.join(file_name))` with no canonicalization or prefix check — `[include %libdir "../../etc/passwd"]` in a defmacro body reads arbitrary files bypassing cap-std; fix: replace `std::fs::read_to_string` with `cap_std::fs::Dir::open()` + `Read::read_to_string()` using the `libdir` DirCap (which enforces `RESOLVE_BENEATH` at the kernel level); also add `if ctx.config.no_fs { return; }` guard before the read (`src/expand.rs:594-602`)
- [ ] `builtin_slurp` has no file size limit — self-hosted include pipeline reads files via `[slurp [open cap path Readable]]`; no MAX_FILE_SIZE enforcement; add `Read::take(MAX_FILE_SIZE + 1)` + length check to both text and binary branches (`src/builtins_io.rs:439-466`)
- [ ] `check-ambient-dir` CI check — rewrite to list all `#[allow(clippy::disallowed` callsites as a human-review reminder (enforcement moves to clippy); remove the `|| true` escape hatch and the `// AMBIENT-OK` filter; also add `#[allow(clippy::disallowed_methods)]` to `src/type_normalize.rs:355` (currently uncaught bare `open_ambient_dir`) (`justfile:294`, `src/type_normalize.rs:355`)
- [ ] Nested includes in `resolve_includes` fall back to `std::fs` + software path check — recursive `resolve_includes` at `src/imports.rs:820-828` passes `None` for `base_cap_dir`, using software `starts_with()` check instead of cap-std kernel-level RESOLVE_BENEATH; fix: open a new `cap_std::fs::Dir` for the nested file's parent and pass it in the recursive call (`src/imports.rs:820-828`)
- [ ] Remove dead `lsp_eval_env` construction block at `src/lsp/document.rs:160-306` — constructs `DirPerms::full()` DirCaps and immediately discards them; misleading dead code; replace with comment explaining LSP eval is intentionally skipped (`src/lsp/document.rs:160-306`)
- [ ] `cap-identity` non-Unix fallback uses `DefaultHasher` (non-stable, randomized per process) — include cache key invalid across restarts; fix: use `blake3::hash(...)` for stable, collision-resistant identity (`src/builtins_meta.rs:1466-1474`)
- [ ] Add `clippy.toml` with `disallowed-types` (`std::fs::File`, `std::fs::OpenOptions`, `std::fs::DirEntry`, `std::fs::ReadDir`) and `disallowed-methods` (`std::fs::read`, `std::fs::read_to_string`, `std::fs::write`, `std::fs::metadata`, `std::fs::read_dir`, `std::fs::canonicalize`, `std::fs::remove_file`, `std::fs::create_dir_all`, `cap_std::fs::Dir::open_ambient_dir`) with `reason:` pointing to cap-std alternatives; add `#![deny(clippy::disallowed_types, clippy::disallowed_methods)]` to `src/lib.rs` and `src/main.rs` (`clippy.toml`, `src/lib.rs`, `src/main.rs`)
- [ ] Audit all callsites flagged by the above lints and annotate the bare minimum legitimate ones with `#[allow(clippy::disallowed_methods)]` — all others are regressions to fix (`src/`)
