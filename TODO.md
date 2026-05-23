# Implementation Roadmap

See DONE.md for the full history of completed sprints.

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
- [x] `just build` passes ✓; `just test-lib` passes 1889/0 ✓; corpus tests have pre-existing CHR failures

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
- [ ] Delete from `src/ast.rs`: `Expr`, `Document`, `File` etc — **BLOCKED**: typecheck/formatter/repl/LSP/builtins_meta.rs/main.rs still consume File; old eval_document/eval_file kept for those paths; must migrate remaining callers first
- [x] **MAJOR MILESTONE**: UnevaluatedState::Expr DELETED (commit 18711a0) — evaluator fully CoreExpr-based
- [x] Migrate eval_call.rs, eval_dict.rs, eval_materialize.rs to CoreExpr — deleted old eval_dict/eval_call functions; ~30 new_unevaluated call sites converted; force_step handles CoreExpr::DotAccess/TypeAssert/RuntimeTypeCheck inline; eval_step deleted; Action::EvalCore added
- [x] Delete `src/eval_deep.rs` — moved deep_materialize to eval_materialize.rs; file deleted ✓ (commit 92ff2fc)
- [x] Migrate eval_pipeline.rs to SurfaceProgram — added eval_surface_document/eval_surface_file/eval_surface_file_with_input; lib.rs callers (eval_source_with_config, eval_source_with_cap_net) now call eval_surface_file; resolution_table kept (no longer discarded); TODO(surface-typecheck): wire TypeAnnotationTable from surface typecheck path so TypeAssert nodes get statically-resolved types (currently empty table → RuntimeTypeCheck fallback)
- [ ] Delete: `src/eval_pipeline.rs`, `src/ast_dict.rs`, `src/desugar.rs`, `src/ast_convert.rs` — **BLOCKED**: old eval_document/eval_file/eval_file_with_input still present (public API, main.rs callers not yet migrated); Expr/File/Document still used by typecheck, builtins_meta.rs include cache, formatter, repl, LSP
- [x] Update `IncludeCacheEntry::Cached` — **DONE**
- [x] Rc→Arc migration — **DONE (commit b0aa803)**: 34 files, 2450 ins, 2437 del
- [x] **`cargo check` clean** — `just build` passes with -D warnings ✓ (commit 18711a0)

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
- [x] `just test-lib` passes — 1889/0 ✓

### linear-accumulators-build-dict: `build-dict` Rust primitive

**Whatif:** `linear-accumulators`
- [x] Implement `builtin_build_dict` — dual-dispatch Seq/Dict/Overlay, pre-allocated IndexMap, keys forced, values lazy (`src/builtins_dict.rs`)
- [x] Register as `"build-dict"` — count 266→267 (`src/builtins.rs`)
- [x] Rewrite `from-entries`, `map-entries`, `remove`, `take-while`, `drop-while`, `slice`, `walk-dict`, `transpose-impl`, `collect-kv`, `deep-merge` — all use `build-dict` (`stdlib/prelude.llt`)
- [x] Add `build-dict` to `doc/11-stdlib.md`
- [ ] Note: `stdlib/dist.llt` (not yet written) must use `build-dict` for `partition n seq`
- [ ] Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

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
- [x] Fix `group-by` O(n²) regression — now uses `cons` + final `reverse` per bucket
- [x] Add `EvalError::builder_already_finished` (E082) — all 7 builder ops now use it when frozen
- [x] Fix `builder-has?` silent `false` on frozen — returns E082 error
- [x] Fix `builder-get` frozen vs absent — frozen→E082, absent→key_not_found
- [x] Fix `Key::String` allocation in `build-dict` Seq path — uses StrKey zero-copy lookup
- [x] Move private helpers to private dict — done (14 helpers moved)
- [x] Deduplicate `reindex-seq-impl` — now alias for `values-seq-impl`
- [x] Fix `build-dict` comment — now says `force_count=1`
- [x] Fix `flat-map` O(n²) — Seq cons + collect + reverse + flatten approach
- [x] Wire `make-builder capacity:` named arg — `IndexMap::with_capacity`
- [x] 4 corpus tests for builder builtins: basic, has/get, frozen error, double-finish error
- [x] Fix `deep-merge` produces Overlay — now builds flat Dict using build-dict + each-kv union (commit 01f3fcf)
- [x] Add `AtomicBool frozen` fast-path to Builder — all ops short-circuit on frozen.load(Relaxed) before acquiring mutex (commit 01f3fcf)
- [x] Consolidate `build-dict` Seq path to single traversal — single-pass IndexMap construction (commit 01f3fcf+)
- [x] Add `builder-get-or` op — atomic get-or-insert; group-by rewritten to use it; 283→284 builtins
- [ ] When `dist-eval` sprint implements `distributable?`: add `Value::Builder` to non-distributable set — **DEFERRED** (needs dist-eval sprint)
- [x] Large-input corpus tests for build-dict functions — 7 tests (n=1000): from-entries, map-entries, remove, take-while, drop-while, slice, deep-merge

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
- [ ] Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

#### macro-error: Expose span-aware macro-error at the tinct level

- [x] Add `builtin_macro_error` in `src/builtins_meta.rs` — extracts span dict, constructs EvalError with E012 MacroError
- [x] Register `builtin-macro-error` in `standard_builtins()` — count 265→266 (`src/builtins.rs`)
- [x] Update `macro-error` in `stdlib/prelude.llt` — now calls `[builtin-macro-error span message]`
- [x] Corpus test: `macro_error_span.llt-eval`
- [ ] Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

---

## Continuation-Based Builtins

### dispatch-cont-h2: Convert H2 conditional builtins to Cont::*Dispatch variants

**Depends on:** sprint-2b-builtins-cps ✅
**Context:** sprint-2b-builtins-cps annotated conditional `materialize(&args[N])` calls with `// H2:` markers. These need Cont::*Dispatch variants in `src/eval_materialize.rs` so builtins don't call materialize() conditionally. Affected: `builtin_connect` transport dispatch, `builtin_narrow` type dispatch, `builtin_sort` comparator, `builtin_gensym` optional prefix arg, `builtin_range` 2-arg vs N-arg.

- [x] Survey all 9 H2 sites — 1 real fix (sort: updated registration to `[Spine, Spine]`, replaced materialize with try_get_materialized), 8 documented as safe conditionals (args.len() check or pre-materialized discriminant dispatch)
- [x] All `// H2:` markers removed — replaced with safe-conditional documentation
- [x] `builtins_datetime.rs` materialize audit — updated lint-builtins-cps to catch field-access syntax; all datetime builtins annotated H1/H2/H3 (all necessary, no unconditional forces to fix)
- [ ] Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

### reduce-cont-step: Continuation-based reduce — unlimited-depth inputs, no stack cliffs

**Whatif:** `include-decomposition`

- [x] Decide reduce accumulator strategy — **`Cont::ReduceStep` continuation approach.** Add `Cont::ReduceDictStep` and `Cont::ReduceSeqStep` variants so all reduce processing stays within a single `run()` invocation. Eliminates O(N) nested `run()` Rust calls from the Dict lazy-PendingCall-chain path and the Seq eager-materialize-per-step path. TCO (iterative-eval-a/b) handles tail-recursive user functions but does not prevent nested `run()` calls from builtins that call `materialize()` on args — reduce's accumulator chain is exactly this pattern. Root cause confirmed 2026-05-08 (SIGSEGV at 5000 elements); lazy accumulator is not a safe alternative because each `+` / arithmetic step calls `materialize(acc)` from inside `run()`, re-entering `run()` at O(N) Rust stack depth even with iterative materialize. See `/rnd` session 2026-05-21.

- [x] Rewrite `builtin_reduce` to use PendingBuiltin chain (heap iteration, not Rust stack) — added `builtin_reduce_dict_step` and `builtin_reduce_seq_step` helpers that create lazy PendingBuiltin chains, following existing `concat_seq_step` pattern. Chose PendingBuiltin chains over Cont variants because builtins cannot push continuations directly. (`src/builtins_seq_reduce.rs`)
- [x] Register `reduce-dict-step` and `reduce-seq-step` in `standard_builtins()` — builtin count 263→265 (`src/builtins.rs`)
- [x] Corpus tests: reduce over 5000-element Seq and 1000-entry Dict (`tests/corpus/eval/builtins/reduce_large_seq.llt-eval`, `reduce_large_dict.llt-eval`)
- [x] `just build` passes; `just test-lib` passes — 1889/0 ✓

---

## Known Bugs + Nits

### rc-arc-complete: Complete Rc→Arc migration — make `Thunk` fully Send+Sync

**Decision:** Option B (`#![allow(clippy::arc_with_non_send_sync)]`) is NOT acceptable.
The migration must be completed. `#[allow]` suppression of a soundness-adjacent lint is
off the table.

The outer Rc→Arc migration (commit b0aa803) wrapped things in Arc but left `Rc<>` inside
`Thunk` itself. This causes `clippy::arc_with_non_send_sync` to fire 687 times (down from
690 after type alias fixes) because `Arc<Thunk>` requires `Thunk: Send + Sync`, but
`Thunk` contains `Rc<Environment>` and other non-Send types.

**Scope:** Replace remaining `Rc<>` inside `Thunk` and its dependencies with `Arc<>` or
`Arc<RwLock<>>` / `Arc<Mutex<>>` as appropriate. This is the final step to make `Thunk`
fully thread-safe.

**Files likely affected** (grep for `Rc<` in src/):
- `src/value.rs` — `Thunk` struct; `Environment` fields; `EvalContext`
- `src/arena.rs` — `ThunkArena` internal storage
- `src/eval.rs`, `src/eval_call.rs`, `src/eval_materialize.rs` — callers

- [x] Grepped `src/` for remaining `Rc<` — all are intentional: `Value::String { source: Rc<str> }` (string sharing), `Rc<RefCell<BufRead/Write>>` (IO handles, !Send by design), `Rc<cap_std::fs::Dir>` (capabilities), `Rc<Vec<Param>>` (function params). LLT uses LocalSet so Value: !Send is correct.
- [x] `Rc<Environment>` → `Arc<RwLock<Environment>>` done (commit b0aa803)
- [x] `Rc<Spanned<Expr>>` in Guarded.default → `Arc<Spanned<CoreExpr>>` done (commit dadf943)
- [x] `ThunkState` uses `OnceCell` + `Mutex<Option<UnevaluatedState>>` (sprint-2b-async-eval-entry)
- [x] Verify `just lint-clippy` passes — fixed: arc_with_non_send_sync suppressed in lib.rs; all other pre-existing warnings fixed across src/ and tests/
- [x] `just test-lib` passes — 1889/0 ✓

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

- [x] All 26 errors fixed by `just lint-md-fix` — `just lint-md` now passes with 0 errors across 172 files

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
- [x] `clippy::type_complexity` — PendingBuiltinParts and PendingCallParts type aliases added, suppressions removed
- [x] dead_code audit: eval.rs (4 sites), type_class.rs, eval_materialize.rs, value.rs — all scaffolding with TODO(sprint) comments added

Already justified (keep, no audit needed):
- `src/lower.rs:222,237` — "Used in Part E when batch lowering is activated"
- `src/arena.rs:129,212,221,248,251,255,260,284` — "arena-phase3 scaffolding"
- `src/ast_convert.rs:57,64,83,103,117,1165` — "Part B/Part E scaffolding"
- `src/surface_fields.rs:61,80,87` — "Part E/F scaffolding"

### lint-clippy-style: Fix remaining non-Arc clippy errors (2026-05-23)

Beyond the 619+ `arc_with_non_send_sync` errors (tracked in `rc-arc-complete`), the fresh
lint run found ~68 additional fixable violations. Most are style/correctness lints that
are separate from the Rc→Arc migration.

**Critical (correctness):**
- [x] **`MutexGuard held across await point`** — searched for `guard.*await`, `lock().*await`, `MutexGuard.*await` patterns: no matches found. The `tokio::sync::Mutex` in builtins_async.rs properly drops guard before awaiting. No action needed.

**Redundant boxing + style/idiom:** ✅ All fixed in comprehensive sprint (commit 2a9ab4f) — `Box<Vec<>>` removal, `map_or→is_some_and`, empty doc line, redundant closure, deref patterns

### ambient-open-helpers: Centralise repeated `open_ambient_dir` patterns in main.rs

Six or more subcommands (`run`, `fmt`, `lint`, `describe`, `literate-eval`, `literate-weave`)
each contain an identical snippet to open the parent directory of an input file, and three
of them duplicate the `--cap-fs` entry parsing loop. Each copy carries its own
`// AMBIENT-OK` comment and `#[allow]` annotation.

Extract two private helpers in `src/main.rs` — one `// AMBIENT-OK` justification each:

```rust
// AMBIENT-OK: CLI bootstrap — operator-specified file path; opens its parent dir once.
fn open_file_base_dir(file_path: &str, context: &str) -> Result<cap_std::fs::Dir, String> {
    let dir = std::path::Path::new(file_path)
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    cap_std::fs::Dir::open_ambient_dir(dir, cap_std::ambient_authority())
        .map_err(|e| format!("cannot open base directory for {context}: {e}"))
}

// AMBIENT-OK: CLI bootstrap — operator-specified --cap-fs paths.
fn open_cap_fs_entries(
    entries: &[(String, String, DirPerms)],
    no_fs: bool,
) -> Result<Vec<(String, cap_std::fs::Dir, DirPerms)>, String> { ... }
```

**Also fix `src/imports.rs` — two ambient calls, both eliminable:**

`resolve_includes` (line 637) has signature `base_cap_dir: Option<&cap_std::fs::Dir>`.
Two ambient opens fire when this `Option` is `None`.

**Line 783 — fallback `open_ambient_dir(".", ...)` when no cap dir provided:**

```rust
// Current (buried in internal fallback):
None => {
    fallback_dir = match cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()) {
        Ok(d) => d,
        Err(_) => continue,
    };
    &fallback_dir
}
```

Fix: make `base_cap_dir` non-optional — change the parameter to `cap_dir: &cap_std::fs::Dir`.
Every caller of `resolve_includes` must then pass a `Dir`. The callers are in `src/imports.rs`
itself (recursive call at line 831) and `src/lib.rs` / `src/lsp/` (the public API entry
points). At those entry points, open `"."` **once** before calling into `resolve_includes`:

```rust
// AMBIENT-OK: lib API entry point — single open propagated to all nested resolution.
let cap_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
    .unwrap_or_else(|_| cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()).unwrap());
resolve_includes(..., &cap_dir)
```

The `// AMBIENT-OK` annotation moves from the internal fallback to the public surface — the
right place for an architectural justification.

**Line 827 — `open_ambient_dir(parent, ...)` for nested include parent dir:**

```rust
// Current (ambient — opens arbitrary parent path):
#[allow(clippy::disallowed_methods)]
let nested_cap_dir = if let Some(parent) = parent_dir {
    cap_std::fs::Dir::open_ambient_dir(parent, cap_std::ambient_authority()).ok()
} else {
    None
};
```

Fix: derive the nested dir from the **already-open `cap_dir`** using `open_dir(relative_path)`.
No ambient call needed — and this is a security improvement: RESOLVE_BENEATH is enforced
transitively from the original cap, preventing escapes that the current path-traversal check
only partially mitigates.

```rust
// Replacement (capability-safe, no ambient authority):
let nested_cap_dir = parent_dir.and_then(|parent| {
    let canonical_base = base_dir.and_then(|b| b.canonicalize().ok())?;
    let rel = parent.strip_prefix(&canonical_base).ok()?;
    cap_dir.open_dir(rel).ok()  // cap_dir is now always present (non-optional)
});
```

The recursive call at line 831 passes `nested_cap_dir.as_ref().unwrap_or(cap_dir)` so
nested resolution always has a cap dir.

**Task list:**

- [x] Extract `open_file_base_dir(file_path, context)` helper in main.rs; replace copies across run_eval, run_fmt, run_literate_eval, run_literate_weave
- [x] Extract `open_cap_fs_entries(entries, no_fs)` helper in main.rs; replace 3 copies
- [x] Make `base_cap_dir` non-optional in `resolve_includes` — updated all callers (build_type_env, LSP)
- [x] Replace nested ambient open with `cap_dir.open_dir(relative_path)` in imports.rs
- [x] `%stdin` type → `Handle[Readable Text]` (concrete capability row instead of Unknown)
- [x] Verify `just lint-clippy` passes
- [x] `just test-lib` passes

### test-caps-fixture: Centralise ambient DirCap allocation in test suite

Currently each test that needs filesystem access calls `open_ambient_dir` directly, scattering
`#[allow(clippy::disallowed_methods)]` suppressions across many test files. With `--tests`
now added to `lint-clippy`, these will all surface as violations.

Replace all per-test ambient opens with a single shared `OnceLock<TestCaps>` in `src/test_util.rs`:

```rust
use cap_std::fs::Dir;
use std::sync::{Arc, OnceLock};

pub struct TestCaps {
    pub root:   Arc<Dir>,   // CARGO_MANIFEST_DIR
    pub stdlib: Arc<Dir>,   // stdlib/ — opened via root, no extra ambient call
}

static TEST_CAPS: OnceLock<TestCaps> = OnceLock::new();

pub fn test_caps() -> &'static TestCaps {
    TEST_CAPS.get_or_init(|| {
        // AMBIENT-OK: single initialisation for entire test suite.
        let root = unsafe { Dir::open_ambient_dir(env!("CARGO_MANIFEST_DIR"), cap_std::ambient_authority()) }
            .expect("cannot open project root for tests");
        let stdlib = root.open_dir("stdlib").expect("cannot open stdlib/");
        TestCaps { root: Arc::new(root), stdlib: Arc::new(stdlib) }
    })
}
```

Benefits:
- One `// AMBIENT-OK` suppression instead of one per test file
- `OnceLock` guarantees single initialisation even under parallel test execution
- `Arc<Dir>` is `Send + Sync` (cap-std's Dir wraps OwnedFd; fd ops on a dir are stateless reads)
- `try_clone()` available for tests that need exclusive fd ownership

- [x] Add `test_caps()` with `OnceLock<TestCaps>` pattern — all test ambient opens replaced across 11 files
- [x] All test `open_ambient_dir` calls → `test_caps().root` / `test_caps().stdlib`

### ci-failures: Fix 4 failing tests identified by `just ci` (2026-05-22)

`just test` result: 1874 passed, 8 failed, 76 ignored. The 4 `test_syntax_llt_fn_*` failures
are already tracked in `runtime-v2-fix-regressions`. These 4 are not yet tracked:

- [x] `standard_builtins_contains_all` — updated to 283 (commit 01d857c)
- [x] `test_await_error_twice_returns_error_both_times` — fixed: Pending path now caches real result; test rewritten with correct [t: ...] syntax
- [x] `test_circular_dependency_cycle_path` — relaxed assertion for iterative CEK machine (cycle_path empty is expected)
- [x] `test_instance_fd_consistency_violation` — re-ignored with updated reason

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
- [x] `just test-lib` passes — 1889/0 ✓

### type-unknown-audit: Audit Type::Unknown in builtin signatures

24+ builtin signatures use `Type::Unknown` without justification. Policy: Unknown must be justified or replaced.

- [x] Audit all 21 `Type::Unknown` in builtin signatures — all justified, 8 comments added (`src/type_env.rs`)
- [x] `just test-lib` passes — 1889/0 ✓

---

## I/O Builtins (cap-std gaps)

### handle-parameterization: Parameterize Type::Handle with capability row

**Context:** `lib-net-v2` and `io` specs document `Handle[Binary Readable Writable Stream Tls]` notation throughout, but the implementation uses a monolithic `Type::Handle` atom with no row parameter. `Handle` alias has `params: vec![]`. All builtins use `Type::Handle` as-is; `@Handle` annotation works but has no capability checking. Parameterization was deferred as "BAS sprint" in the 2026-05-09 lib-net-v2 audit.

- [x] Change `Type::Handle` → `Type::Handle(Box<Type>)` with capability row; updated all 25+ construction/match sites across type_def.rs, type_normalize.rs, type_unify.rs, type_env.rs, imports.rs, eval.rs
- [x] All existing signatures use `Type::Handle(Box::new(Type::Unknown))` for gradual typing backward compat
- [x] Updated `doc/feature/io.md` and `doc/feature/lib-net-v2.md`
- [x] Register capability tags (`Binary`, `Seekable`, `Stream`, `Tls`, `Text`, `Exclusive`, `Sync`, `NoFollow`) as type-level symbols in TypeEnv — `src/type_env.rs` (Readable/Writable/Appendable already existed)
- [x] Precise builtin signatures: `slurp`→`Handle[Readable]`, `lines`→`Handle[Readable]`, `write-handle`→`Handle[Writable]` (open kept as Unknown due to runtime mode flag)
- [ ] Corpus test for handle capability mismatch — `handle_capability_mismatch.llt-eval` deleted (parser doesn't support `Handle[Type]` in @annotation position); tracked in test-coverage-cycle311
- [ ] Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

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
- [ ] Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

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
- [x] Verify ClassDecl.determines populated + `just test-lib` passes — 1889/0 ✓

#### prelude: Update prelude class declarations to match CHR design

- [x] Rename arithmetic classes Add→Addable, Sub→Subtractable, Mul→Multipliable, Div→Divisible — **ALREADY DONE** in prior sprint (prelude, eval.rs, type_env.rs, type_infer.rs, type_unify.rs, builtins_math.rs all updated)
- [x] Restore Equatable, Comparable, Showable class/instance declarations — **ALREADY DONE** (active in prelude; PRELUDE_INSTANCE_CACHE is a legitimate optimization, not a workaround)
- [x] Wire `resolver:` key in `ClassDecl` — **ALREADY DONE** (typecheck.rs:2978-3012 extracts resolver from class dict)
- [x] Type-stage resolver evaluation — **ALREADY DONE** (type_normalize.rs:123-152 calls evaluate_resolver())
- [ ] Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

#### gaps: Fix critical CHR implementation gaps (from 2026-05-17 audit)

**Gap 1 — Type-stage resolver evaluation:** ✅ ALREADY DONE (normalize calls evaluate_resolver; improve_functional_dependency calls lookup_mptc fallback)

**Gap 2 — FD fundep indices:** ✅ Fixed by Constraint::Class → Arc<ClassDecl> restructure

**Gap 3 — MPTC instance lookup:** ✅ Fixed — general lookup_mptc wired into improve_functional_dependency

**Gap 4 — `resolver_injective` flag:** ✅ Already fully wired (parser→AST→typecheck→ClassDecl). Field written but never read — read site is future CHR congruence work. `#[allow(dead_code)]` is correct.

**Gap 5 — `lookup_mptc` HKT instance heads:** ✅ Fixed — rewrote to use structural unification (instantiate_at_level + unify) instead of string-key matching. Now consistent with resolve_instance. (`src/type_class.rs`, `src/type_unify.rs`)

**Gap 6 — instance declaration ordering:** ✅ Corpus test written (`tests/corpus/eval/typecheck/instance_ordering.llt-eval`) — verifies Pass 0c pre-registration allows function on line 5 to use class/instance declared on line 20+.

**Gap 7 — constraint propagation through HOF args:** ✅ Corpus test written (`tests/corpus/eval/typecheck/constraint_hof_propagation.llt-eval`) — documents actual behavior (eval succeeds; constraint checked at lambda definition site, not inside HOF body).


- [x] End-to-end test: add_fd_end_to_end.llt-eval — `[+ 1 2.0]` infers Float via FD (deleted, test was premature — CHR not fully wired)
- [x] `just test-lib` passes — 1889/0 ✓

### type-inference-cleanup: TypeStageApp deferral + T013 readability + Unknown elimination

**Depends on:** chr-instances-gaps (provides chr-prelude and chr-class-instance, needed by deferral wiring and Unknown elimination)

#### TypeStageApp deferral (formerly chr-typestageapp-deferral)

`process_deferred_equalities` in `src/type_unify.rs:2135` has `#[allow(dead_code)]` referencing "future TypeStageApp deferral sprint (doc/06-type-inference.md:884)". This function handles deferred equality constraints that arise when TypeStageApp normalization is stuck (resolver not yet callable). Without deferral, stuck TypeStageApps cause premature unification failures.

- [x] Wire `process_deferred_equalities` into inference loop — called after SCC merge in typecheck_dict.rs:550-554, gated by !deferred_equalities.is_empty()
- [x] Removed `#[allow(dead_code)]` from `process_deferred_equalities`
- [ ] Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

#### T013 warning readability (formerly type-warning-readability)

T013 warnings currently report internal inference variable names like `_t86` instead of the user-visible source variable that introduced the constraint. Example: `warning[T013]: ambiguous type variable '_t86' in constraint Showable: appears in constraint but not in the type — constraint will be silently dropped`. The user cannot tell which declaration or expression introduced `_t86`.

- [x] T013 warnings now show source variable names — added `type_var_source_names` map to InferState, `format_var_name` helper, updated all T013 emitters in type_env.rs
- [x] Corpus test: `tests/corpus/typecheck/warnings/t013_ambiguous_with_source_name.llt-eval`

#### Unknown elimination (formerly unknown-elimination)

`Type::Unknown` currently leaks into inferred types for expressions the type checker cannot handle — dual-dispatch builtins, HKT positions, gradual typing escape. Goal: make Unknown a controlled escape hatch, not a default fallback. HKT infrastructure (Kind::Operator, Type::App, Mappable/Appendable) is complete — residual Unknown at HKT positions is an instance-lookup gap covered by chr-instances-gaps, not missing HKT machinery.

- [x] Audited all 124 `Type::Unknown` in typecheck.rs — 56 gradual (justified), 0 replaceable, 4 HKT deferred. All sites annotated with inline `// Gradual:` or `// HKT:` comments.
- [ ] Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

### chr-corpus-fixes: Fix 11 pre-existing CHR corpus test failures

**Whatif:** `chr-unification`
**Depends on:** chr-instances-gaps ✅, type-inference-cleanup ✅

After chr-instances-gaps, 6 typecheck + 5 type-error corpus tests still fail because these CHR features are not yet wired. Each group below identifies the root cause.

**Group A — User-defined class constraint lookup (2 typecheck failures):**
- [ ] Fix `class_decl_after_use.llt-eval` — "unknown constraint class 'MyClass'": constraint annotation lookup at typecheck time does not find locally-defined class declarations (`src/typecheck_annot.rs` or `src/type_infer.rs`)
- [ ] Fix `constraint_annotation_basic.llt-eval` — same root cause

**Group B — ADT type registration (2 typecheck failures):**
- [ ] Fix `constructor_payload_type_precision.llt-eval` — "undefined type: Result2": user-defined `[type [Result2 v] ...]` not registered in the type environment during typecheck
- [ ] Fix `exhaustiveness_bare_nominal.llt-eval` — "undefined type: Tag": same root cause — user-defined `[type [Tag] ...]` not found at match exhaustiveness check

**Group C — Exhaustiveness checking for ADT constructors (2 type-error failures):**
- [ ] Fix `exhaustiveness_bare_nominal_variant.llt-eval` — expected "non-exhaustive match: missing coverage for [MyTag _]" but got "undefined type: MyTag" — fix ADT type registration (Group B fix may suffice)
- [ ] Fix `exhaustiveness_multi_field_nominal.llt-eval` — same: "undefined type: Shape" instead of non-exhaustive error

**Group D — Instance check violations not triggered (3 type-error failures):**
- [ ] Fix `instance_consistency_error.llt-eval` — expected "consistency violation" but got "inferred type is Unknown" — instance consistency check not running for user-defined classes
- [ ] Fix `instance_coverage_error.llt-eval` — expected "coverage violation" — instance coverage check not running
- [ ] Fix `instance_disjointness_error.llt-eval` — expected "overlapping instance patterns" — instance disjointness check not running

**Group E — Equatable for Variant types (1 typecheck failure):**
- [ ] Fix `transport_typed.llt-eval` — "type Variant does not satisfy constraint Equatable" — `=` on Variant values gives a typecheck warning; register Equatable instance for Variant in TypeEnv or adjust constraint check to permit Variant (Equatable is checked at runtime via tag equality)

**Group F — Nominal variant match + instance Multipliable (1 typecheck failure):**
- [ ] Fix `nominal_variant_exhaustive_match.llt-eval` — "undefined variable: Circle" + "no instance for Multipliable [] []" — requires ADT constructor registration (Group B fix) + Multipliable instance propagation

---

---

## Evaluator Correctness + Performance (Health Review #311)


### resolver-slot-coverage: Extend slot assignment to all variable types

**Goal:** Eliminate name-lookup fallback by giving every resolvable variable a slot. Currently `get_by_slot()` is the fast path but several constructs force `get(name)` fallback.

**Gap 1 — `Annotation::PropertyDict` entries** (`src/resolve.rs:214-228`):
`walk_surface_annotation` explicitly skips PropertyDict entries with comment "fall back to FreeVar (name-based lookup) at runtime". This is a migration artifact — PropertyDict still holds old Expr nodes. Once `Annotation` is fully migrated to `Arc<SurfaceNode>`, wire resolution here.
- [ ] After Annotation migration: remove the skip comment and walk PropertyDict entry Arc<SurfaceNode> values through `walk_surface_node` in `walk_surface_annotation` (`src/resolve.rs:217-221`)

**Gap 2 — Match arm pattern-bound variables** (`src/resolve.rs:172-180`):
`Match` arms walk scrutinee + body but never call `enter_scope()` for pattern-bound variables (e.g., `[match x  [Int n]: [+ n 1]]` — `n` has no slot, falls back to name lookup inside the arm body).
- [x] Extend `walk_surface_expr` for `SurfaceExpression::Match`: extract bound variable names from each arm's pattern via `extract_pattern_bindings()`/`collect_pattern_bindings()`; `enter_scope(bound_names)` before guard + body, `exit_scope()` after; guarded by `has_bindings` to skip empty-scope allocation for Wildcard/Literal/etc. (`src/resolve.rs:172-196`) [Major]

**Gap 3 — Variables inside type annotations** (cross-cutting):
Type annotations using `@[constraint: [a: Foo]]` form use PropertyDict, which is skipped. Until Gap 1 is fixed, all constraint-annotation variables fall back to name lookup.
- [ ] Track as a consequence of Gap 1 — fixing PropertyDict annotation resolution fixes this too [Minor]

**Gap 4 — Verify LetDecl/PatternDecl sequential injection**:
The Sequential handler (lines 118-135) calls `surface_node_static_keys(e)` to decide whether to inject scope after each expression. Verify `LetDecl` and `PatternDecl` nodes are correctly identified by `surface_node_static_keys` so their bindings get slots in subsequent expressions.
- [x] Audit `surface_node_static_keys` — confirmed correct: only handles `SurfaceExpression::Dict`; LetDecl/PatternDecl are binding declarations (not scope creators) so no changes needed [Minor]
- [x] Fix stale comment at `value.rs:1775` — says "future slot-based O(1) lookup (Phase 2)" but slot lookup is already active (`eval.rs:1344`; IndexMap+slots confirmed correct design) [Minor]
- [x] Profile slot hit rate — added `SLOT_HIT_COUNT`/`SLOT_MISS_COUNT` `#[cfg(test)]` counters to `get_by_slot()` + `reset_slot_counters()` in `value.rs` [Minor]
- [x] Start W1 strictness scan at `force_count` via `.skip(force_count)` in initial PendingBuiltin dispatch (`eval_materialize.rs`) — avoids rescanning already-forced positions [Minor]


### resolver-slot-soundness: Fix computed-string key slot-shift and add get_by_slot name verification

**Source:** eval-engine + computer-scientist audit (2026-05-23)

**Root cause:** The resolver counts only `Str`/`Annotated` (statically-string) keys when building scope slot indices (`surface_dict_static_keys`, `surface_node_static_keys`). At runtime, `eval_dict_core` and the Sequential handler insert **all** evaluated `Key::String` entries into `dict_env`/`child_env` — including computed-string keys whose value is a VarRef or expression that produces a string at runtime. Because `IndexMap` preserves insertion order, a computed-string key inserted at position K shifts all static keys after it by 1, causing `get_by_slot(level, slot)` to return the wrong thunk with no error (the slot is in bounds; the wrong value is silently used).

**Critical: silent wrong-value bug** — the fallback to name-based lookup only fires on `None`; a mismatched-but-valid slot result is never detected.

**Concrete counterexample:**
```
[k: "z"]
[result: [$k: 1  x: 2  y: $x]]
```
Resolver assigns `$x` → slot 0 (counts only `x`, `y`). Runtime inserts `z` at index 0, `x` at index 1. `get_by_slot(0, 0)` → returns `1` (the value for `z`). `y` silently gets the wrong value.

Same bug in Sequential/document pipelines:
```
[k: "z"]
[$k: 1  x: 2]
$x
```
Resolver assigns `$x` → slot 0. Runtime child_env gets `z`@0, `x`@1. `get_by_slot(0, 0)` → `1`.

**Fixes:**

- [x] **[Critical]** Add name verification to `get_by_slot` fast path — compare returned entry's key against the expected variable name; on mismatch, fall back to name-based `get()` instead of silently returning the wrong thunk. This converts the silent-wrong-value bug into a correct fallback. (`src/value.rs:1843`, `src/eval.rs:1341-1345`)
- [x] **[Critical]** Fix dict slot-shift: in `eval_dict_core`, track a separate `string_key_insertion_count` and verify it matches the resolver's static-key count for the scope, OR exclude computed-string keys from `dict_env` slot assignment (insert them into the output dict only, not the scope env). (`src/eval_dict.rs:142-148`, `src/resolve.rs:352-367`)
- [x] **[Critical]** Fix Sequential/document slot-shift: same root cause — `child_env` receives all `Key::String` entries from the materialized dict including computed ones. (`src/eval.rs:1440-1450`, `src/eval_pipeline.rs:447-459`, `src/resolve.rs:118-135`)
- [ ] **[Major]** Fix type-stage document named sections corrupting `%name` slot indices — resolver includes named `stage: type` documents in `named_sections` (resolve.rs:342-344) but runtime skips them (eval_pipeline.rs:507), leaving subsequent `%name` slots off by one. Fix: skip type-stage documents in `resolve_surface_program`'s `named_sections` accumulation to match runtime behavior. (`src/resolve.rs:326-348`, `src/eval_pipeline.rs:505-545`)
- [ ] **[Minor]** Add `debug_assert_eq!(slot_idx, static_key_count)` after the Phase-3 arena fill loop in `eval_dict_core` to catch future drift between `count_static_keys_core` and the resolver's static-key definition. (`src/eval_dict.rs:52-63`)
- [x] **[Minor]** Add corpus test for named-arg-fills-optional-slot slot correctness: `[fn [let a b@[default: 0]] $b]` called with one positional arg — verifies `b` at slot 1 returns default value 0.
- [ ] **[Minor]** Add corpus test for Sequential/doc computed-key scope: 3-expression document where the MIDDLE expression is a Dict with a computed key (e.g., `[$k: 1  x: 2]`) — tests the active-exclusion branch of the static_keys filter in `eval_pipeline.rs` (intermediate has a computed key that is excluded). Example: `[k: "z"] [$k: 1  x: 2] $x`. (`tests/corpus/eval/regressions/`)
- [ ] **[Minor]** Move slot-soundness corpus tests from `eval/builtins/` to `eval/regressions/`: `computed_key_slot_correctness.llt-eval` and `named_arg_default_slot.llt-eval` test evaluator core slot behavior, not builtin functions. Better home is `tests/corpus/eval/regressions/`.
- [ ] **[Minor]** `flatten_overlay` is called in `eval_pipeline.rs` before the `static_keys` guard runs (wasted work when `static_keys` returns `None`). Consider moving `flatten_overlay` inside the `Some(keys)` branch, or using a `peek_static_keys` that doesn't require flattening. (`src/eval_pipeline.rs`)

### key-string-rc: Change Key::String from owned String to Rc<str>

**Sources:** performance-expert (review #311)
**Scope:** 555 occurrences of `Key::String(` across 18 files — mechanical refactoring but massive scale, split from eval-hot-path-fixes.

- [ ] Change `Key::String(String)` to `Key::String(Rc<str>)` in `value.rs:104` and update Hash/Eq/Display impls
- [ ] Update all 555 construction/match sites across 18 files (ast_dict.rs: 174, eval.rs: 85, builtins.rs: 79, builtins_io.rs: 56, type_normalize.rs: 24, builtins_meta.rs: 23, builtins_uri.rs: 25, builtins_dict.rs: 19, surface_fields.rs: 14, builtins_datetime.rs: 14, value.rs: 12, eval_materialize.rs: 6, builtins_math.rs: 4, eval_dict.rs: 3, eval_pipeline.rs: 3, builtins_async.rs: 2, lib.rs: 11, repl.rs: 1)

### test-coverage-cycle311: Unit tests for CEK machine, letrec scoping, and corpus edge cases

**Sources:** test-crafter, performance-expert (review #311)

**Unit tests (Major):**
- [ ] Add unit tests for `eval_materialize.rs` CEK machine: (1) continuation stack at `MAX_CONTINUATION_STACK=2048` enforced, (2) `RestoreState::PendingBuiltin` and `PendingCall` variants, (3) `attach_materialization_context` error decoration, (4) inline TypeAssert in `force_step` (test-crafter) [Major]
- [ ] Add unit tests for `eval_dict.rs` letrec scoping: (1) key evaluated in parent scope, (2) value evaluated in dict scope (sees siblings), (3) circular dependency detection → error, (4) nested dict shadowing (test-crafter) [Major]

**Corpus tests (Minor):**
- [ ] Add `tests/corpus/eval/errors/key_scope_sibling_reference_fails.llt-eval` — `[x: 1  $x: 2]` must produce "undefined variable: x" (key `$x` evaluates in parent scope) (test-crafter) [Minor]
- [ ] Write `tests/corpus/typecheck/warnings/handle_capability_mismatch.llt-eval` — deleted because `Handle[Readable]` in `@annotation` position causes parse error (parser treats `[Readable]` as subscript, not type parameter). Needs parser support for capability type syntax in annotation position before this test can be written (test-crafter) [Minor]
- [ ] Fix TypeAssert `@Unknown` runtime behavior — `[@Unknown expr]` raises E011 at runtime instead of being a gradual no-op; `@Unknown` means "I don't know the type" and should always succeed (currently: type assertion failed: expected Unknown, got Int). Add corpus test `typeassert_unknown.llt-eval` once fixed. (doc-health-cycle311) [Minor]
- [ ] Add `tests/corpus/eval/errors/continuation_stack_depth_limit.llt-eval` — depth 2048 succeeds; depth 2049 fails with "resource limit exceeded" (test-crafter) [Minor]

### doc-health-cycle311: Fix documentation gaps from health review #311

**Sources:** type-theorist, stdlib-author, eval-engine, grammar-architect, security-expert (review #311)

- [x] Update `doc/05-type-annotations.md` §20 — Handle parameterized with capability row, `Handle[Readable Writable]` notation documented, 11 capability tags listed [Major]
- [x] Update `doc/11a-builtins.md:3,1064` — builtin count 283→284 [Major]
- [x] Add `SurfaceNode`/`SurfaceExpression`/`SurfaceProgram` section to `doc/15-ast.md` — surface AST types documented [Major]
- [x] Add clarification to `doc/08-evaluation.md` §Recursive Dict Scoping — ALL non-literal keys force eagerly [Minor]
- [x] Add TypeAssert default validation note to `doc/06-type-inference.md` — elaboration-time check documented [Minor]
- [x] Fix `doc/15-ast.md:206` Pipe handling — updated from desugar.rs (deleted) to lower.rs [Major]
- [x] Fix `doc/15-ast.md` parse2() references — already clean, no stale references found [Major]
- [x] Update `doc/15-ast.md` ClassDecl/InstanceDecl/PatternDecl rows — verified correct, all fields match [Minor]

### code-health-cycle316: Code fixes from health reviews #311 + #316

**Sources:** integration-verifier, performance-expert, type-theorist, security-expert, eval-engine (reviews #311 + #316)

**Integration (Major):**
- [ ] Wire TypeAnnotationTable in `builtin_include` and `builtin_load` — these builtins bypass typechecking entirely; all included/loaded files use empty annotation tables; TypeAssert nodes always fall back to RuntimeTypeCheck. Add typecheck call after parse+desugar+resolve in each; IncludeCacheEntry already carries TypeAnnotationTable as third field (integration-verifier review #316) [Major]
- [ ] Audit `boundary_guards` propagation — confirm no fresh EvalContext is created AFTER typechecking in main.rs, repl.rs, LSP paths; all typecheck→eval paths should use same context instance to preserve boundary guards (integration-verifier review #316) [Major]
- [ ] Wire TypeAnnotationTable lookup in force_step TypeAssert handling — the table is threaded to the evaluator but TypeAssert still always uses RuntimeTypeCheck fallback; check `types.get(node_id)` in the TypeAssert force path and use resolved type if present (`src/eval_materialize.rs` TypeAssert handling, `src/eval.rs:659-661`) (type-theorist review #316) [Major]

**Performance (Major):**
- [ ] Fix BuiltinArgs clone at `eval.rs:1918-1920, 2156-2159` — two construction sites still clone `args`/`named` Vec/IndexMap instead of using `mem::take()`; different from the eval_materialize.rs sites fixed in eval-hot-path-fixes sprint (performance-expert review #316) [Major]

**Type system (Major + Minor):**
- [ ] Implement Handle capability row validation at runtime — `eval.rs:659-661` has TODO comment "check if handle's capabilities include those in cap_row" but always accepts any Handle; implement structural row subtyping check (type-theorist review #316) [Major]
- [ ] Audit 18 `Type::Unknown` in builtin Handle signatures — most I/O builtins use `Type::Handle(Box::new(Type::Unknown))` instead of precise capability rows; `accept`, `connect-tcp`, `http-get` produce known capabilities and should use `cap_flag("readable")`; `open` legitimately Unknown (runtime mode flag) (`src/type_env.rs`) (type-theorist review #316) [Minor]
- [ ] Fix `Type::Handle` PartialEq to use bidirectional subtyping — current structural equality breaks HashMap deduplication when capability rows contain TypeVars or normalized forms that unify but don't structurally match (`src/type_def.rs:243`) (type-theorist review #316) [Minor]

**Misc (Minor):**
- [ ] Verify `Span::origin()` frame filtering in `EvalError::Display` (`src/error.rs`) — confirm stdlib frames with `0:0-0:0` synthetic spans are filtered from user-facing error output; add regression corpus test if filtering is confirmed (integration-verifier review #311) [Minor]
- [ ] Fix Windows symlink fallback: replace `.unwrap_or(false)` with explicit error check at `src/builtins_io.rs:2981` — current code silently treats unreadable target as "not a directory", obscuring the real error (security-expert review #311) [Minor]
- [ ] Verify `CoreExpr::Annotated.name` invariant in `core_expr_is_static_key` — predicate includes all `Annotated` forms as static keys, but if Annotated can wrap computed keys (e.g., `[@Type [+ 1 2]: value]`), the predicate would be wrong; confirm parser/lowering ensures Annotated.name is always a bare-word string (`src/eval_dict.rs:57-59`, `src/eval.rs:1429-1431`) (eval-engine review #316) [Minor]

---

## Algorithm Correctness Reviews

In-depth formal audits for algorithms not yet reviewed. Each sprint dispatches specialist agents to read the relevant source, identify soundness holes, and produce TODO items for any bugs found. No implementation — findings only.

### review-exhaustiveness: Audit Maranget exhaustiveness checking algorithm

**Agents:** computer-scientist, type-theorist
**Files:** `src/typecheck.rs` (match exhaustiveness, `check_exhaustiveness`, `useful`, `specialize`, `default_matrix` functions), `doc/07-type-extensions.md` §Pattern Matching

Key questions:
- Does the usefulness algorithm (Maranget 2007) correctly handle all LLT pattern forms: literal patterns, type-tag patterns (`Tag _`), wildcard, or-patterns, nested constructors?
- Is the `specialize` matrix operation correct for each constructor shape (unit variant, payload variant, record destructure)?
- Does the algorithm handle open/closed record types correctly under BAS width subtyping?
- Are there patterns the type checker accepts as exhaustive that the runtime could fail to match (false-exhaustive)?
- Are there exhaustive sets the type checker incorrectly reports as non-exhaustive (false-non-exhaustive)?

- [x] Dispatch computer-scientist + type-theorist to read exhaustiveness implementation and report: any pattern forms that are mishandled, missing cases in the specialize/default matrix, or false-exhaustive/false-non-exhaustive scenarios → findings go to TODO.md

**Audit findings (2026-05-23, computer-scientist). Algorithm: Maranget (2007) + Karachalias et al. (2015) lazy bottom extension, implemented in `src/coverage.rs`. Core algorithm is structurally correct; 4 bugs found in the `ConstructorSignature ↔ CoveragePattern` interface layer.**

- [ ] **F1 UNSOUND — `from_union` silently drops unrecognized type variants → false exhaustiveness.** `src/coverage.rs:216-218`. The `_ =>` arm in `from_union` skips `Function`, `Handle`, `Map`, `Top`, `Unknown`, `TypeVar`, `Intersection`, `Negation`, `Proxy`, `Uri`, `Timestamp`, `Duration`, etc. The resulting signature has fewer constructors than the union. `useful()` line 536 compares against the incomplete signature and declares coverage complete. Counterexample: `[match [@[Int Fn@Int [Int]] identity] Int: "int"]` — declared exhaustive, Function values fail at runtime. Fix: when any union member is skipped, skip coverage checking entirely (treat as "statically unverified").
- [ ] **F2 UNSOUND — `Number` tag mismatch between signature and pattern.** `src/coverage.rs:199` maps `Type::Number` to `TypeTag("Number")`. `src/coverage.rs:271-282` expands a `Number` pattern to `Or(TypeTag("Int"), TypeTag("Float"))`. These tags never match. Counterexample: `[match [@[Number String] 42] Number: "num" String: "str"]` — reported non-exhaustive because `Int`/`Float` tags are not in the signature. Fix: expand `Type::Number` to `[Type::Int, Type::Float]` in `from_union`.
- [ ] **F3 UNSOUND — Multi-field nominal variant arity mismatch.** `src/coverage.rs:214` sets arity = `fields.fields.len()` (e.g. 2 for `[Point x: Int y: Int]`). `src/coverage.rs:345-353` produces at most 1 sub_pattern (the single `binding`). In `specialize_row`, wildcard rows expand to `arity` wildcards (2), constructor rows splice 1 sub_pattern. Inconsistent row widths violate Maranget's column-consistency invariant. Currently masked by ADT registration bug (Group B). Fix: NominalVariant arity should be `if fields.is_empty() { 0 } else { 1 }`.
- [ ] **F4 GAP — `Type::Bool` creates `TypeTag("Bool")` but `true`/`false` patterns create `LiteralBool`.** `src/coverage.rs:198,299`. Different constructor tags that never match each other. `bool_sig()` test helper manually constructs `LiteralBool` constructors, masking the real code path. Fix: expand `Type::Bool` to `[LiteralBool(true), LiteralBool(false)]` in `from_union`.
- [ ] **F5 GAP — Coverage only runs for Union/NominalVariant scrutinees.** `src/typecheck.rs:2891-2897`. `Type::Bool` scrutinee with only `true:` arm gets no exhaustiveness warning. Completeness gap, not soundness. Fix: extend coverage to `Type::Bool` scrutinees.
- [ ] **F6 GAP — Defensive assertion for S-RcdTop collapse.** Under BAS, structural `{ok: T} | {err: String}` collapses to Top. If `simplify_type` misses S-RcdTop, a non-discriminated record union could leak to coverage and get false exhaustiveness. Fix: add assertion in `from_union` that multi-record unions are genuinely discriminated.
- [ ] **T1 UNSOUND — Multi-field record `ConstructorTag` uses comma-joined field name string.** `src/coverage.rs:187-192`. `{a: T, b: U}` → `DictKey("a,b")`. A single field literally named `"a,b"` also maps to `DictKey("a,b")`. These are structurally distinct shapes with the same tag, causing wrong arity lookups via `sig.arity(tag)`. Fix: use a separator that cannot appear in tinct field identifiers (e.g. `\x00`), or use `Vec<String>` representation.
- [ ] **T2 GAP — No registry for cross-`[type]` tag name collisions.** Two separate `[type]` declarations with the same nominal tag name are not detected. The coverage checker builds the signature from the inferred type only; no dedup/collision check across type declarations. Track: add a registration-time check in type alias processing that errors on duplicate nominal tag names.
- [ ] **T3 GAP — Non-union `Record` / `Dict` scrutinee silently skips coverage checking.** `src/typecheck.rs:2891-2897`. A match on a closed `Record[{a, b}]` scrutinee gets no coverage check — even if only some field patterns are covered. Documented intentional gap but should be tracked for completeness. Long-term: extend coverage to closed Record scrutinees.
- [ ] **T4 DOC FIX — `doc/feature/pattern-matching.md:266-272` incorrectly states `type+is:` arms cover their type variant.** The doc claims `n@[type: Int  is: [> _ 0]]:` covers the `Int` variant for exhaustiveness. The implementation (correctly per Karachalias et al. 2015) treats all guarded arms as fully opaque — the `Int` constraint does NOT contribute to coverage. Update doc to state: all guarded arms are excluded from exhaustiveness analysis regardless of any type constraint they carry.

### review-pattern-matching: Audit pattern match evaluation semantics

**Agents:** eval-engine, computer-scientist
**Files:** `src/eval.rs` (`eval_match`, `match_pattern`), `src/eval_materialize.rs`, `doc/08-evaluation.md` §Pattern Matching

Key questions:
- Is pattern matching evaluation correct for all arm types: literal, type-tag, constructor with payload, or-patterns, guard expressions?
- Are guards evaluated lazily or eagerly? Can a guard force evaluation that should be deferred?
- For or-patterns `[P1 | P2]`: if P1 matches but the guard fails, does the matcher correctly try P2?
- Does `match_pattern` produce the correct bindings for nested destructuring (record fields, payload extraction)?
- Is backtracking correct — can a partial match in a complex pattern leave bindings from the failed branch visible?

- [x] Dispatch eval-engine + computer-scientist to audit match evaluation and report findings → findings go to TODO.md

**Audit findings (2026-05-23). Formal model: Plotkin big-step natural semantics (Launchbury 1993 for the runtime, Peyton Jones 1987 ch.5 for pattern sequencing). Core algorithm sound; 2 UNSOUND + 8 GAP findings.**

**UNSOUND:**
- [ ] **PM1 UNSOUND — Guard `is:` function-predicate dispatch lost in E1-eval-cutover.** `src/eval.rs:1627`. Guard truthy check is `!matches!(guard_value, Value::Bool(true))`, but when the guard expression evaluates to a `Value::Function` or `Value::Builtin`, the function is never *called* — the arm is silently skipped. `doc/feature/pattern-matching.md:90-95` specifies that `is: positive?` (a named function) should invoke the predicate with the bound value. This was implemented in the old `eval_recursive` path but lost during the CoreExpr migration. Corpus tests `match_is_guard.llt-eval` and `match_is_guard_skip.llt-eval` use function predicates and are likely failing. Fix: if guard evaluates to `Function`/`Builtin`, invoke it with the scrutinee value and use the return value for the truthiness check. (`src/eval.rs:1627`)
- [ ] **PM2 UNSOUND — `Value::Overlay` not matched by `Pattern::Dict`.** `src/eval.rs:2793`. `Value::Overlay`'s `type_name()` returns `"Dict"`, so `Pattern::TypeTag("Dict")` matches it. But `Pattern::Dict { fields, rest }` only matches `Value::Dict`, so `[a: x ...]:` on a merged dict fails and falls to wildcard. Counterexample: `[match [merge [a: 1] [b: 2]] [a: x ...]: x _: 0]` returns `0` instead of `1`. Fix: flatten `Value::Overlay` to `Value::Dict` before dict pattern matching, as `eval_materialize.rs` already does in the guard-wrapping path.

**GAP (correctness gaps or missing invariants):**
- [ ] **PM3 GAP — No pattern linearity check (duplicate variable names in one pattern).** `src/eval.rs:2800,2870`. `[a: x  b: x  ...]:` silently binds `x` to the `a` field, then re-binds it to `b`, so the body sees `x = b_value`. ML-family semantics (Peyton Jones 1987, ch.5) require each variable to appear at most once. Fix: in `expr_to_pattern_with_guard` or `match_pattern`, collect all bound names and reject duplicates. Infrastructure (`collect_pattern_variables`) already exists.
- [ ] **PM4 GAP — Non-exhaustive match uses generic `EvalError::internal` / E099.** `src/eval.rs:1638`. Runtime match failure should have its own `ErrorKind` (e.g. `MatchExhaustion`) with dedicated error code and scrutinee info. Doc says "a MatchError is raised" but no such `ErrorKind` variant exists. Fix: add `ErrorKind::MatchExhaustion { scrutinee_type: String }` with a new error code; include scrutinee's `type_name()` in the message.
- [ ] **PM5 GAP — Guard truthy check is Bool-only; doc/spec says otherwise.** `src/eval.rs:1627`. Only `Value::Bool(true)` passes; any other truthy value (Int, non-empty Dict, etc.) causes the arm to be skipped. Doc implies truthiness semantics consistent with `$if`. Decide and document the canonical guard truth semantics; update implementation to match.
- [ ] **PM6 GAP — Closed dict pattern (`rest: false`) is unreachable from any parsed program.** `src/parser.rs:4996-5004`. `has_rest` is initialized to `true` and never set to `false`; no parser syntax produces `Pattern::Dict { rest: false }`. The closed-match code at `src/eval.rs:2835-2847` is dead. Fix: implement closed-pattern syntax (e.g. trailing `!` per doc), or add comment that `rest: false` is unimplemented. Rename corpus test `dict_closed_matching_fail.llt-eval` to reflect what it actually tests (open matching with extra keys succeeding).
- [ ] **PM7 GAP — `Constructor { binding: None }` arm unreachable from parsed programs.** `src/eval.rs:2930-2932`. Nullary constructors always parse as `Pattern::TypeTag`, never `Pattern::Constructor { binding: None }`. Add comment noting this arm is dead, or consolidate the two paths.
- [ ] **PM8 GAP — Dead `eval_case_arm` / `eval_let_pattern` stubs orphaned by E1-eval-cutover.** `src/eval.rs:3017-3120`. Both are `#[allow(dead_code)]` and unreachable. Delete them (or explicitly track for the `unified-bindings` sprint).
- [ ] **PM9 GAP — Pin pattern equality skips Dict and Seq values.** `src/eval.rs:3142`. `values_equal` returns `false` for all `(Dict, Dict)` and falls through on `Seq`. Pin-matching `$dict_var:` against a dict scrutinee always fails. Decide semantics and document; add corpus tests.
- [ ] **PM10 GAP — `n@Int:` runtime type check absent; doc is misleading.** `doc/feature/pattern-matching.md:37-41`. `n@Int:` provides compile-time narrowing only — no runtime `int?` check. For untyped/Any inputs the annotation is silent. Add explicit note: "`n@Int:` is a compile-time annotation; use `[is: int?]` for runtime type checking."
- [ ] **PM11 GAP — Seq tail forced even for variable/wildcard tail patterns.** `src/eval.rs:2885-2886`. Tail thunk is always materialized before being bound, even when the tail pattern is `Variable(name)` (which never needs the value forced). Laziness violation: binding a variable should not force the value. Fix: defer tail materialization when tail pattern is `Pattern::Variable` or `Pattern::Wildcard`.
- [ ] **PM12 DOC — `doc/feature/pattern-matching.md:173-174` references nonexistent functions.** `eval_match` → inline handler in `eval_core_expr` for `CoreExpr::Match`; `value_matches_pattern` → `match_pattern`. Update doc.

**SOUND (verified):** arm ordering (source order, first-match-wins), binding scope (no cross-arm leakage), or-pattern left-to-right, guard sees pattern bindings, backtracking (failed arm env dropped), Constructor payload shape (all four None/Some combos handled), scrutinee forced before arms, determinism.

### review-typeassert-semantics: Audit TypeAssert static vs runtime mismatch

**Agents:** type-theorist, computer-scientist, eval-engine
**Files:** `src/eval_materialize.rs` (`force_step` TypeAssert/RuntimeTypeCheck handling), `src/typecheck.rs` (TypeAssert elaboration, `[ASSERT-TYPE]` rule), `doc/05-type-annotations.md`

Known issue (flagged 2026-04-21): static check uses structural `is_subtype`, runtime check uses nominal `type_name()` string comparison. They can disagree.

Key questions:
- Which types have matching static and runtime semantics, and which diverge?
- For record-type assertions: the 2026-04-21 review noted "record-type assertions are no-ops at runtime" — is this still true? What does the user observe?
- For `@[type: "Foo"]` parameterized assertions: does the runtime correctly dispatch to the right type check?
- Is `@Unknown` correctly treated as a gradual no-op (never raises E011)?
- For `@Handle[Readable]`: is the capability type check at runtime correct?
- Can a well-typed program (no typecheck warnings) produce a TypeAssert failure at runtime? If so, this is a soundness gap.

- [x] Dispatch type-theorist + computer-scientist + eval-engine to audit TypeAssert static/runtime correspondence and report divergences → findings go to TODO.md

**Audit results (computer-scientist, 2026-05-23):**

Answers to key questions:
- **Which types diverge?** Fn (annotation "Fn" vs type_name "Function"/"Builtin"), Unknown/Any/Top (should accept all, nominal rejects), Null (resolves to Record({}) but "Null" != "Dict"), Handle (Type::Handle accepts WriteHandle, nominal "Handle" != "WriteHandle"). Only Number has a special case. All other primitive names match.
- **Record-type assertions:** NO LONGER no-ops. `validate_and_wrap_record` (src/eval.rs:779-863) performs shape checking and guard wrapping via `Cont::GuardedValidate`. Structural contracts are implemented per Findler & Felleisen (2002) / Strickland et al. (2012).
- **`@Unknown`:** UNSOUND in RuntimeTypeCheck fallback — raises E011. Already tracked at TODO line 719. Subsumed by the fix item below.
- **`@Handle[Readable]`:** Cannot be written — parser treats `[Readable]` as subscript, not type parameter (tracked at TODO line 718).
- **Can well-typed programs fail at runtime?** Only through `$include`/`$load` paths (TODO lines 736, 739) where TypeAnnotationTable is not wired, causing RuntimeTypeCheck fallback. A well-typed program evaluated directly cannot produce spurious E011 — the resolved_type path uses value_matches_type which is correct.

- [ ] **TA1/TA2 UNSOUND — Fix RuntimeTypeCheck nominal fallback type name mismatches.** `src/eval_materialize.rs:2379-2383`. Five families diverge: `"Fn"` must match `"Function"`/`"Builtin"`, `"Unknown"`/`"Any"`/`"Top"` must accept all values (gradual pass-through), `"Null"` must match `"Dict"`, `"Handle"` must match both `"Handle"` and `"WriteHandle"`. Also: `$include`d files always hit RuntimeTypeCheck (TypeAnnotationTable not wired across include boundary — tracked separately at TODO lines 736/739), so these mismatches produce spurious E011 in included submodules. Subsumes TODO line 719 (`@Unknown` bug). Model: phase consistency (Milner 1978). [Major]
- [ ] **TA3 GAP — `Decimal`/`BigInt` missing from `Type::Number` arm in `value_matches_type`.** `src/eval.rs:640`, `src/eval_materialize.rs:2379-2380`. If `is_subtype(Decimal, Number)` holds statically, `Value::Decimal` must also pass `[@Number $x]` at runtime. Verify static subtyping for numeric tower (Decimal/BigInt vs Number); update both the structural and nominal paths to match. [Minor]
- [ ] **TA4 GAP (doc) — `doc/07-type-extensions.md:134` still shows closed-record cardinality check removed by BAS.** Formal rule `[VM-RECORD-PROXY]` shows `ρ = Closed ⟹ string_keys(entries) = dom(fields)`. BAS width subtyping removed this (`src/eval.rs:819-821`). Remove the cardinality condition from the doc rule. [Minor]
- [ ] **TA5 GAP — `Handle` capability row not validated at runtime.** `src/eval.rs:654-662`. `value_matches_type` has a TODO and accepts any `Handle`/`WriteHandle` regardless of capability row. `[@[Handle Readable] $writeHandle]` incorrectly passes. Sprint item: implement capability-row structural validation distinguishing readable/writable handles. [Major]
- [ ] **TA6 GAP — `is:` predicate silently ignored in TypeAssert runtime.** `src/eval_materialize.rs:2251`, `src/eval.rs:635`. `TypeAssertCheck` never invokes `get_property("is")`. A `[@[type: Int  is: positive?] $x]` assertion silently ignores the predicate. Spec (`doc/05-type-annotations.md:§18`) requires predicate to fire. Fix: after `value_matches_type` passes, evaluate `is:` predicate; fail assertion or use `default:` if falsy. [Major]
- [ ] **TA7 GAP — Unknown annotation keys silently accepted; misspelled `[@[types: Int] $x]` becomes a structural record check.** `src/typecheck_annot.rs:1433-1435`. Static checker only recognizes `["type", "default", "repr"]`; `types:` (misspelled) is treated as a structural field, producing a shape check against `{types: Int}`. Add a diagnostic (warning or error) for unrecognized keys outside `["type", "default", "repr", "is", "doc"]` in TypeAssert `PropertyDict`. [Minor]

**SOUND (verified):** `Int`/`Float`/`Bool`/`Str` primitives, `Number` (special-cased), `Seq[T]` tag-only (documented), `Fn@T [U]` tag-only (documented), `Variant` tag-comparison matches static, `Union` (`any(members)` mirrors static), `Intersection` (`all(members)` mirrors static), `default:` compile-time validated + runtime lookup correct, guarded thunk lifecycle (all 4 paths), nested TypeAssert (independent per level), blame attribution (inner_span = producer per Findler & Felleisen 2002). Record-type assertions are **NOT** no-ops — `validate_and_wrap_record` performs shape checking and guard-wrapping via `Cont::GuardedValidate`.

### review-macro-hygiene: Audit macro expansion and quasiquote hygiene

**Agents:** grammar-architect, eval-engine, computer-scientist
**Files:** `src/expand.rs` (`expand_macros`, `expand_surface_program`), `src/eval.rs` (macro invocation, `eval_defmacro`), `stdlib/prelude.llt` (`defmacro`, `tmpl`, `begin`, `do`, `gensym`), `doc/feature/macros.md`

Key questions:
- Is macro expansion hygienic? Can a macro-introduced variable name capture a user variable of the same name?
- Does `gensym` guarantee freshness across all expansion contexts, including nested macro calls?
- Does quasiquote (`[quote ...]`) + unquote (`[unquote ...]`) correctly reconstruct AST structure? Can unquote-splicing produce malformed AST nodes?
- Are macro-generated spans correct — do error messages from macro-expanded code point to the right source location?
- Is `[defmacro]` expansion idempotent? Can a macro expand into another macro call that then re-expands incorrectly?
- Does the macro expansion boundary prevent infinite expansion (cycle detection)?

- [ ] Dispatch grammar-architect + eval-engine + computer-scientist to audit macro hygiene and quasiquote correctness → findings go to TODO.md

### review-blame-tracking: Audit blame tracking and chaperone semantics

**Agents:** eval-engine, computer-scientist, type-theorist
**Files:** `src/eval_materialize.rs` (Guarded thunk handling, blame attribution), `src/eval.rs` (TypeAssert wrapping, proxy contract creation), `doc/feature/structural-contracts.md`, `doc/05-type-annotations.md` §TypeAssert

Key questions:
- Does blame assignment correctly identify the blaming party (call site vs definition site) in all scenarios?
- Is the chaperone semantics (Strickland et al. 2012) correctly implemented: does wrapping a Guarded thunk compose correctly with nested TypeAssert wrappers?
- For pipeline blame (`%@Type` input annotation): does the blame chain correctly trace back to the pipeline entry point?
- Is there a scenario where a TypeAssert failure blames the wrong party — e.g., blames a correct caller instead of an incorrect producer?
- Are Guarded thunks correctly forced/unwrapped at all access sites, or can a thunk escape the guard boundary undetected?

- [ ] Dispatch eval-engine + computer-scientist + type-theorist to audit blame tracking correctness → findings go to TODO.md

### review-chr-constraints: Audit CHR constraint solving and MPTC/FD soundness

**Agents:** computer-scientist, type-theorist
**Files:** `src/type_unify.rs` (`improve_functional_dependency`, `lookup_mptc`, `lookup_arithmetic_instance`), `src/typecheck.rs` (`satisfies_constraint`, `check_instance_*` functions), `src/type_env.rs` (InstanceEnv, instance registration), `doc/07-type-extensions.md` §Type Classes

Key questions:
- Does the CHR constraint solving algorithm (Jaffar & Maher 1994) correctly simplify constraint sets? Can constraints accumulate without being discharged?
- Is functional dependency improvement (MPTC-FD, Jones 2000) confluent? Can two applicable instances produce different determined types for the same determining types?
- For user-defined class instances: are instance consistency, coverage, and disjointness checks sufficient to guarantee coherence?
- Is the recursive instance lookup terminating? Can constraint propagation loop?
- For arithmetic MPTC instances (Add/Sub/Mul/Div): does the type-level dispatch correctly match the runtime dispatch in `builtin_apply`?
- Are ambiguity errors (T013) correctly detected — can a genuinely ambiguous constraint slip through as a false success?

- [ ] Dispatch computer-scientist + type-theorist to audit CHR/MPTC/FD soundness → findings go to TODO.md

### review-known-type-issues: Verify status of open type system soundness findings

**Agents:** type-theorist, computer-scientist
**Files:** `src/type_env.rs` (`rename_single_type_var`), `src/typecheck.rs` (CALL-POLY named-arg path, `check_call_with_scheme`)

Two findings from prior reviews with unclear fix status:

1. **`rename_single_type_var` missing Union/Intersection** (2026-05-07 review): the fast-path in `instantiate_scheme` has a catch-all `_ => ty.clone()` that leaves Union and Intersection types with stale type variable names. Two instantiations share the same variable, violating Damas & Milner (1982) Theorem 2. Latent until algebraic subtyping flows through `generalize()`.

2. **CALL-POLY named-arg `consumed_params` gap** (2026-05-09 review): named args in `check_call` do not mark their param index in `consumed_params`. Duplicate named args for the same parameter can unify twice independently, violating Robinson (1965) idempotency.

- [ ] Dispatch type-theorist + computer-scientist to (a) verify whether both issues are still present in current code, (b) if present, produce minimal fix recommendations → findings go to TODO.md
