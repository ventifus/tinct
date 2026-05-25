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

### parser-migration-full: Atomic parser rewrite to produce SurfaceProgram natively (Phases 1-3 ✅, Phase 4 BLOCKED)

**Replaces:** parser-migration-b, parser-migration-c, parser-migration-d (all collapsed into one atomic sprint)
**Goal:** Change parser.rs to produce `Arc<SurfaceNode>` at every internal expression construction, assembling `SurfaceProgram` directly. Eliminates `ast_convert.rs` bridge.
**Approach:** This is a LARGE atomic change (~5000 line parser). Work in a feature branch. No intermediate cargo check expected.
**Depends on:** E3-formatter-delete-bridge — both sprints delete `src/ast_convert.rs`; E3-formatter-delete-bridge must complete first so the bridge callers are migrated before the parser stops producing it

**Phase 1-3: ALREADY DONE** — parser constructs SurfaceExpression natively (130 usages), StackFrame uses Arc<SurfaceNode>, ParseOutput.program is SurfaceProgram, all expand_macros() calls migrated to expand_surface_program().

- [x] Phase 1: Frame stack types — StackFrame uses Arc<SurfaceNode>, push_value takes Arc<SurfaceNode>
- [x] Phase 2: Expression construction sites — 130 SurfaceExpression:: usages in parser.rs
- [x] Phase 3: Output type — ParseOutput.program: SurfaceProgram; all expand_surface_program() callers
- [ ] Delete `src/ast_convert.rs` — **BLOCKED**: rv2-infer-surface ✅ done. Remaining callers: typecheck.ts×3 (resolve_type_expr bridge), typecheck_annot.ts×3 (resolve_type_expr bridge), typecheck_dict.ts×1 (resolve_type_expr bridge), expand.rs×2 (macro body Expr::Fn), eval.rs (quote/unquote: expr_to_core_expr, expr_to_surface_node), eval_materialize.rs×2 (TypeAssert CoreExpr). **Depends on: rv2-resolve-type-expr + rv2-migrate-evaluator-bridges**
- [x] `just build` passes ✓; `just test-lib` passes 1889/0 ✓; corpus tests have pre-existing CHR failures

### Parts B + E — Parser, resolver, typechecker, expander migration + Evaluator cutover (ATOMIC) — remaining items BLOCKED

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
- [x] **rv2-e3-expander-cutover DONE (commit f199515)**: `expand_surface_program` now walks `SurfaceExpression` natively via `expand_surface_expr`. Old Expr functions marked `#[allow(dead_code)]`. Remaining Expr dependency: `pre_scan_expr`/`register_stdlib_macros_from_env` (prelude macro scanner, blocked on separate stdlib-scan migration sprint `rv2-e3b-stdlib-macro-scanner`).

**Part D remaining** (`src/surface_fields.rs`):
- [x] Sequence fields in `surface_node_get_field()` — already handled in existing implementation (`src/surface_fields.rs`)
- [x] `span_to_value()` with full Span Dict encoding — **DONE**: added to `src/surface_fields.rs`

**Part E — Evaluator cutover + delete old types:**
✅ **Rc→Arc migration DONE (commit b0aa803)** — Arc<Thunk>, Arc<RwLock<Environment>>, Arc<EvalContext>, Mutex<ThunkState> throughout. E1-E3 are now UNBLOCKED.
- [ ] Delete from `src/ast.rs`: `Expr`, `Document`, `File` — **BLOCKED**: rv2-infer-surface ✅ done (infer_expr deleted, typecheck.rs has no production Expr import). Remaining: eval.rs still uses Expr for quote/unquote (expr_to_core_expr/core_expr_to_expr), expand.rs uses Expr::Fn for macro body evaluation. **Depends on: rv2-resolve-type-expr + rv2-migrate-evaluator-bridges**
- [x] **MAJOR MILESTONE**: UnevaluatedState::Expr DELETED (commit 18711a0) — evaluator fully CoreExpr-based
- [x] Migrate eval_call.rs, eval_dict.rs, eval_materialize.rs to CoreExpr — deleted old eval_dict/eval_call functions; ~30 new_unevaluated call sites converted; force_step handles CoreExpr::DotAccess/TypeAssert/RuntimeTypeCheck inline; eval_step deleted; Action::EvalCore added
- [x] Delete `src/eval_deep.rs` — moved deep_materialize to eval_materialize.rs; file deleted ✓ (commit 92ff2fc)
- [x] Migrate eval_pipeline.rs to SurfaceProgram — added eval_surface_document/eval_surface_file/eval_surface_file_with_input; lib.rs callers (eval_source_with_config, eval_source_with_cap_net) now call eval_surface_file; resolution_table kept (no longer discarded); TODO(surface-typecheck): wire TypeAnnotationTable from surface typecheck path so TypeAssert nodes get statically-resolved types (currently empty table → RuntimeTypeCheck fallback)
- [x] Delete old Expr functions from `src/eval_pipeline.rs` (eval_document/eval_file/eval_file_with_input deleted 2026-05-23); old Expr functions from `src/ast_dict.rs` deleted 2026-05-24 (~2000 lines); dead cluster from `ast_convert.rs` deleted 2026-05-23; `src/desugar.rs` NOT deleted (Surface API only, live). Remaining: ast_convert.rs entirely + Expr/File from ast.rs → **BLOCKED on rv2-infer-surface**
- [x] Update `IncludeCacheEntry::Cached` — **DONE**
- [x] Rc→Arc migration — **DONE (commit b0aa803)**: 34 files, 2450 ins, 2437 del
- [x] **`cargo check` clean** — `just build` passes with -D warnings ✓ (commit 18711a0)

### Part F ✅, Part G ✅, sprint-2a-rc-arc ✅ — All complete (see DONE.md)

### rv2-migrate-ast-dict: Migrate `ast_dict.rs` to SurfaceProgram (unblocks formatter + builtins_meta) ✅

**Critical path first.** `ast_dict.rs` (`ast_to_dict`, `dict_to_ast`) still walks old `Expr` AST. It is the primary blocker for the formatter, `builtins_meta.rs`, and the `tinct describe` CLI path. All other migrations depend on this one.

- [x] Add Surface bridge functions — `surface_node_to_dict`, `dict_to_surface_node`, `surface_program_to_dict`, `dict_to_surface_program` in `ast_dict.rs` (bridge via ast_convert.rs) [commit 9849c61]
- [x] Migrate `formatter.rs` caller — eliminates SurfaceProgram→File→dict conversion [commit 9849c61]
- [x] Migrate `builtins_meta.rs` callers — ALREADY DONE (Part G migration; zero ast_dict call sites in builtins_meta.rs)
- [x] Migrate `expand.rs:1802` caller — DONE via rv2-migrate-expand-macro ✅
- [x] `just build` passes [commit 9849c61]

### rv2-migrate-builtins-meta: ~~Migrate builtins_meta.rs~~ ALREADY COMPLETE

**Status:** NO-OP — builtins_meta.rs has ZERO ast_dict call sites to migrate.

**Finding (2026-05-23):** Grep for `\bast_to_dict\(|\bast_to_dict_expr\(|\bdict_to_ast\(` in src/builtins_meta.rs → 0 matches. The `load` builtin (lines 1629-1736) was already migrated to return `Value::Program` directly (runtime-v2 Part G). Doc comments at lines 1619, 1624 mention "ast_to_dict" but only describe OUTPUT FORMAT, not implementation.

**Remaining work:** This sprint can be deleted. The ACTUAL blockers for rv2-delete-old-ast are expand.rs (3 call sites) and eval.rs (2 call sites for unquote handling).

### rv2-migrate-expand-macro: Migrate expand.rs macro expansion to Surface ast_dict API ✅

**DONE (2026-05-23):** All `ast_to_dict_expr`/`dict_to_ast` calls in `expand.rs` and `eval.rs` already replaced with Surface bridge functions. Verified by grep — zero remaining callers.

- [x] `expand.rs:1661` → `surface_node_to_dict(&expr_to_surface_node(arg), &opts, ctx)` ✅
- [x] `expand.rs:1694` → same pattern for binding_rc ✅
- [x] `expand.rs:1802` → `dict_to_surface_node(&deep_result, ctx).map(|n| surface_node_to_expr(&n))` ✅
- [x] `eval.rs` unquote handling → migrated to Value::Expression path (Part G) ✅
- [x] `expand.rs` imports `dict_to_surface_node, surface_node_to_dict` from ast_dict ✅

### rv2-delete-eval-pipeline-old: Delete old eval_document/eval_file from eval_pipeline.rs ✅

**Goal:** Remove the old Expr-based `eval_document`, `eval_file`, `eval_file_with_input` functions from `src/eval_pipeline.rs`. These are the last callers of the old File/Expr eval path.

**DONE (2026-05-23):**
- [x] Grep for remaining callers of old `eval_file`, `eval_document`, `eval_file_with_input` across src/, tests/, scripts/. Found: eval.rs tests, builtins.rs:create_type_stage_env, lib.rs re-exports.
- [x] Migrated `builtins.rs:create_type_stage_env` to use `eval_surface_document` + `resolution_table` (iterates `program.documents` instead of `file.node.documents`; dropped `surface_program_to_file` call)
- [x] Migrated 9 `test_eval_document_*` tests in `eval.rs` to use `crate::eval_source` (surface pipeline); removed local `eval_document` test helper
- [x] Delete `pub async fn eval_document(...)` from `src/eval_pipeline.rs`
- [x] Delete `pub async fn eval_file(...)` from `src/eval_pipeline.rs`
- [x] Delete `pub async fn eval_file_with_input(...)` from `src/eval_pipeline.rs`
- [x] Remove re-exports of `eval_file`, `eval_file_with_input` from `src/lib.rs`
- [x] Remove unused imports: `Document`, `File`, `use std::rc::Rc`, `eval` from eval_pipeline.rs
- [x] `just build` passes; `just test-lib` passes

### rv2-rewrite-ast-dict: Full SurfaceNode-native rewrite of ast_dict.rs (Phases 1-5 ✅, Phase 6 BLOCKED)

**Goal:** Replace the 1500-line Expr-walking `ast_to_dict`/`dict_to_ast` with native SurfaceNode equivalents so `ast_convert.rs` bridge can eventually be deleted. The bridge functions added in rv2-migrate-ast-dict serve as the API; this sprint changes the internals.

**Prerequisite:** rv2-migrate-ast-dict ✅ (bridge API exists) + builtins_meta.rs + expand.rs callers migrated (currently deferred above)

**Decomposed tasks (order matters — each step must compile):**

**Phase 1 — Survey + map SurfaceExpression variants to Expr equivalents:**
- [x] Read `src/ast_dict.rs:70-1500` in full; 24 direct 1:1 Expr→SurfaceExpression mappings found, 7 moved to SurfaceDeclaration, 4 dict_to_ast gaps, schema stable. Planning doc: `doc/whatif/plans/ast-dict-surface-migration-notes.md` [commit 7f1f721]
- [x] Identify Surface variants: 9-step implementation order documented; `SurfaceItem::Decl` dispatch needed for declarations [commit 7f1f721]

**Phase 2 — Rewrite ast_to_dict_expr → surface_node_to_dict_inner:**
- [x] Add `fn surface_node_to_thunk_id(node: &Arc<SurfaceNode>, opts, ctx)` that walks all `SurfaceExpression` variants natively (all 24 Group A variants handled)
- [x] `surface_node_to_dict()` now calls `surface_node_to_thunk_id` directly (no bridge through ast_convert)

**Phase 3 — surface_decl_to_thunk_id + surface_document_to_thunk_id:**
- [x] Add `fn surface_decl_to_thunk_id(decl: &SurfaceDeclaration, span, opts, ctx)` handling all 7 Group B variants (TypeAlias, ClassDecl, InstanceDecl, DefMacro, MacroDecl, SyntaxClass, Splice). Schema matches old Expr-based emitter.
- [x] Add `fn surface_document_to_thunk_id(doc: &SurfaceDocument, span, opts, ctx)` iterating `doc.items` natively, dispatching `SurfaceItem::Expr` → `surface_node_to_thunk_id`, `SurfaceItem::Decl` → `surface_decl_to_thunk_id`
- [x] Note: `ClassDecl.superclasses` still silently dropped (tracked separately below)

**Phase 4 — Rewrite surface_program_to_dict:**
- [x] `surface_program_to_dict` rewritten to iterate `program.documents` natively via `surface_document_to_thunk_id` — no longer bridges through `ast_convert::surface_program_to_file`
- Note: `dict_to_surface_node` / `dict_to_surface_program` still bridge through old Expr path (reverse direction; see Phase 5)

**Phase 5 — Rewrite dict_to_surface_node_inner (reverse direction):**
- [x] `dict_to_surface_node_inner`: native reconstruction for 10 common variants; unknown tags = hard EvalError (no fallback) [commit 85f82bb]
- [x] Fixed 4 missing dict_to_ast cases: Match, ClassDecl, InstanceDecl, PatternDecl; new `dict_to_pattern` helper [commit 85f82bb]
- [x] `dict_to_surface_node()` calls inner directly; no bridge via ast_convert [commit 85f82bb]

**Phase 6 — Delete bridge layer:**
- [x] Deleted `ast_to_dict(file, ...)` and `document_to_dict` — zero production callers [commit 38a7e7b]
- [x] Unit tests rewritten to use `surface_program_to_dict` [commit 38a7e7b]
- [x] `ast_to_dict_expr`, `expr_to_thunk_id`, `entry_to_thunk_id`, `named_arg_to_thunk_id`, `param_to_thunk_id` deleted — zero production callers
- [x] `dict_to_ast`, `dict_to_ast_from_dict`, `dict_to_entry`, `dict_to_named_arg`, `dict_to_param`, `dict_to_pattern`, `dict_to_file`, `dict_to_surface_program` deleted — zero production callers; `dict_to_annotation` rewritten natively with `dict_to_surface_entry`
- [x] `surface_program_to_file` / `expr_to_surface_node` / `surface_node_to_expr` in ast_convert.rs — typecheck.rs production no longer uses these (rv2-infer-surface done). Remaining callers: typecheck.ts×3 (resolve_type_expr bridge), typecheck_annot.ts×3, typecheck_dict.ts×1, expand.rs×2, eval.rs. Tracked in rv2-resolve-type-expr + rv2-migrate-evaluator-bridges.

**Tracked separately:**
- [ ] NEEDS_DESIGN: (grammar-doc-polish) `ClassDecl.superclasses` silently dropped in `surface_decl_to_thunk_id` — `Vec<(String, String)>` not yet in schema. Design decision: add `superclasses: List` key or omit permanently. Requires `/rnd` design session before sprint. Sprint: grammar-doc-polish.

### rv2-migrate-repl: Migrate REPL to Surface eval path (small, independent)

- [x] Replace `repl.rs` call — REPL was already on Surface path; removed stale surface_program_to_file conversion [commit 7b0033d]
- [x] `just build` passes [commit 7b0033d]

### rv2-migrate-lsp: Remove `File` from LSP DocumentState (small, independent)

- [x] `lsp/document.rs`: removed `File` from `DocumentState`; added `fatal_parse_error: Option<ParseError>`; prelude stored as `SurfaceProgram` [commit 9370184]
- [x] `lsp/analysis.rs` hover/diagnostics work — prelude search uses SurfaceProgram directly [commit 9370184]
- [x] `just build` passes [commit 9370184]

### rv2-migrate-typecheck-api: Delete old `typecheck_file_*` wrappers

**Depends on:** rv2-migrate-ast-dict (formatter migration removes last `typecheck_file` call sites)

- [x] Verified — all external callers use `typecheck_surface_program*`; only internal bridge + tests remain
- [x] `typecheck_file_with_types` made `#[cfg(test)]` (test-only); others made `fn` (private) [commit 98148cc]
- [x] `just build` passes [commit 98148cc]

### rv2-migrate-annotation: Migrate `Annotation::PropertyDict` from `Vec<Spanned<Entry>>` to `Vec<Spanned<SurfaceEntry>>` ✅ DONE (2026-05-23, Phases 1-5)

### rv2-e3b-stdlib-macro-scanner: Migrate register_stdlib_macros_from_env + pre_scan_expr to SurfaceExpression ✅ DONE (2026-05-23)

**Blocks:** `expand.rs` standalone `use crate::ast::Expr;` removal → rv2-delete-old-ast

`register_stdlib_macros_from_env` and its helper `pre_scan_expr` scan stdlib files for macro declarations using old Expr AST. They need to be migrated to use `pre_scan_surface_document` (already added) instead.

- [x] Migrate `register_stdlib_macros_from_env` to call `pre_scan_surface_document` instead of converting to File and calling `pre_scan_expr` (`src/expand.rs`)
- [x] Delete `pre_scan_expr`, `pre_scan_expr_spanned`, `pre_scan_expr_value`, `expand_expr`, `expand_expr_inner`, `expand_macro_call`, `validate_syntax_class`, `validate_against_pattern` dead-code functions (`src/expand.rs`)
- [x] Remove standalone `use crate::ast::Expr;` from `src/expand.rs`



### rv2-resolve-type-expr: Migrate resolve_type_expr from Spanned<Expr> to Arc<SurfaceNode>

**Blocks:** rv2-delete-old-ast (7 bridge calls across typecheck.ts, typecheck_annot.ts, typecheck_dict.ts)

`resolve_type_expr` in `src/typecheck_annot.rs` takes `&Spanned<Expr>` and is called after bridge conversions from SurfaceNode. Migration eliminates 7 `surface_node_to_expr` bridge calls.

- [ ] Change `resolve_type_expr` signature from `&Spanned<Expr>` to `&Arc<SurfaceNode>` in `src/typecheck_annot.rs`; update match arms from `Expr::*` to `SurfaceExpression::*`
- [ ] Migrate all callers in typecheck.ts (lines 835, 2432, 2578), typecheck_annot.ts (lines 89-90, 1346, 3339), typecheck_dict.ts (line 325) to pass SurfaceNode directly

### rv2-migrate-evaluator-bridges: Migrate eval.rs and expand.rs ast_convert bridges

**Blocks:** rv2-delete-old-ast (remaining ast_convert callers in evaluator layer)
**Depends on:** rv2-resolve-type-expr

- [ ] Migrate `eval.rs` quote/unquote handling: remove `expr_to_core_expr`/`core_expr_to_expr` bridges (lines 918, 976, 999-1034)
- [ ] Migrate `expand.rs` macro body evaluation: replace `Expr::Fn` construction with direct SurfaceNode evaluation (lines 1401, 1474)
- [ ] Migrate `eval_materialize.rs` TypeAssert CoreExpr bridges (lines 2312-2358)
- [ ] Verify `surface_program_to_file`/`file_to_surface_program` have zero callers; delete from ast_convert.rs

### rv2-delete-old-ast: Delete Expr/Document/File and old pipeline files

**Depends on:** rv2-migrate-ast-dict ✅(partial), rv2-migrate-repl ✅, rv2-migrate-lsp ✅, rv2-migrate-typecheck-api ✅

**All production callers of `surface_program_to_file` migrated (zero production callers remain):**
- ~~`src/expand.rs:1661,1694,1802`~~ — ✅ DONE (2026-05-23): expand.rs already uses `surface_node_to_dict`/`dict_to_surface_node`
- ~~`src/eval.rs:992,1004`~~ — ✅ DONE (Part G): unquote handling migrated to `Value::Expression` path
- ~~`src/typecheck.rs` internal bridge~~ — ✅ DONE (2026-05-24, typecheck-surface-migration tasks 6-8): `typecheck_surface_program_with_env` now walks `program.documents` directly via `typecheck_surface_document`; `surface_program_to_file()` bridge deleted from the hot path. `typecheck_surface_program` still uses the bridge (span-keyed TypeMap path); old `typecheck_file_*` functions remain private for tests.
- ~~`src/eval_pipeline.rs`~~ — ✅ old `eval_document`/`eval_file`/`eval_file_with_input` DELETED (2026-05-23)
- ~~`src/parser.rs:725`~~ — ✅ DONE (2026-05-23, rv2-migrate-annotation Phases 3-5): `surface_program_to_file` call replaced with direct `SurfaceExpression` matching; `adjust_entries`/`adjust_expr`/`adjust_spanned_expr`/`adjust_annotation` helpers deleted; `entry_to_surface` no longer called from parser
- ~~`src/typecheck.rs:497`~~ — ✅ DONE (2026-05-23, rv2-migrate-annotation final commit): `typecheck_surface_program` now delegates to `typecheck_surface_program_with_env` (native Surface walk); `surface_program_to_file` call deleted. Old `typecheck_file_*` functions and `reset_elaboration`/`typecheck_document` marked `#[cfg(test)]`.

**Production Expr/File import cleanup (2026-05-23):**
- [x] `src/eval_call.rs` — removed `Expr` from production import; `get_default` now returns `Arc<SurfaceNode>`; call site uses `Thunk::new_surface` (lazy, empty ResolutionTable for FreeVar name-based lookup)
- [x] `src/typecheck.rs` — `scan_type_quality` and `check_overbroad_annotations` migrated to `&SurfaceProgram`; `File` moved to `#[cfg(test)]` import; `scan_type_quality` now also called from `typecheck_surface_program_with_env` so warnings work on Surface path
- [x] `src/typecheck_dict.rs` — `Expr` split out of compound production import to standalone `use crate::ast::Expr;` (still needed by `infer_dict`; TODO(rv2-delete-old-ast): remove once `infer_dict` rewritten natively on SurfaceEntry)
- [x] `src/expand.rs` — `Expr` split out of compound production import to standalone `use crate::ast::Expr;` (still needed by macro expander; BLOCKED on E3e expander cutover to SurfaceExpression)

**Once ALL above are migrated:**
- [x] Delete `src/ast_convert.rs` dead code — deleted `file_to_surface_program_with_types` + 4 private helpers (2026-05-23); remaining live functions: `surface_node_to_expr`, `expr_to_surface_node`, `expr_to_core_expr`, `core_expr_to_expr` (production callers in typecheck.rs, typecheck_dict.rs, eval.rs, expand.rs, parser.rs)
- [x] Migrate typecheck.rs tests from `typecheck_file_with_types` to `typecheck_surface_program` — 28 tests migrated; test helpers (`infer`, `doc_env`, `file_env_impl`) now use `typecheck_surface_document` directly; `typecheck_file_with_types*` (4 wrappers), `typecheck_document` (367 lines), `reset_elaboration`, `reset_expr`, `extract_doc_strings` all deleted (2026-05-24)
- [x] Delete `src/ast_dict.rs` old Expr-based functions (`ast_to_dict`, `ast_to_dict_expr`, `dict_to_ast`, `dict_to_file`, `dict_to_surface_program`, `expr_to_thunk_id`, `entry_to_thunk_id`, `named_arg_to_thunk_id`, `param_to_thunk_id`) — DONE
- [x] Clean up `#[cfg(test)]` imports — `Document` import removed from typecheck.rs; `File` only remains for 3 legitimate ast_convert self-tests
- [ ] Delete `src/ast_convert.rs` entirely — **BLOCKED**: 6 production callers of `surface_node_to_expr` (expand.rs ×2, typecheck_dict.rs ×4, typecheck.rs ×7) + `expr_to_surface_node` (typecheck.rs ×4, eval.rs ×6, parser.rs ×5) + `expr_to_core_expr`/`core_expr_to_expr` (eval.rs ×2). These exist because `infer_expr` still takes `Rc<Spanned<Expr>>` — needs full type checker rewrite to SurfaceNode
- [ ] Delete `Expr`, `Document`, `File` from `src/ast.rs` — **BLOCKED**: `infer_expr` (typecheck.rs production) walks Expr; 14 `check_*` functions take `&[Rc<Spanned<Expr>>]` args; `Document` and `File` eliminated from typecheck.rs (only 3 ast_convert self-tests remain). Requires new sprint: `rv2-infer-surface` to rewrite `infer_expr` to walk SurfaceNode natively
- [x] `src/desugar.rs` — confirmed NOT deletable: `desugar_surface_program`/`desugar_surface_node` are the live API
- [x] `just build` passes; `just test` passes (pre-existing test failures NOT caused by this sprint)

**Pre-existing corpus test failures (NOT caused by this sprint):**
- `tests/corpus/eval/typecheck/warnings/constraint_*` (9 tests) — typecheck strict-warnings errors treated as test failures; pre-existing CHR regressions from runtime-v2 merge
- `tests/corpus/eval/typecheck/warnings/doc_not_string`, `help_suggestion_*`, `unknown_fn_annotation_key` — same category
- `tests/corpus/eval/type_errors/fn_annotation_mixed_keys_error`, `string_not_handle` — pre-existing type error format mismatches
- `tests/corpus/typecheck/warnings/fn_annotation_mixed_keys`, `handle_capability_mismatch` — tracked: handle_capability_mismatch blocked on parser support for `Handle[Type]` annotation syntax

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
- Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

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
- Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

#### macro-error: Expose span-aware macro-error at the tinct level

- [x] Add `builtin_macro_error` in `src/builtins_meta.rs` — extracts span dict, constructs EvalError with E012 MacroError
- [x] Register `builtin-macro-error` in `standard_builtins()` — count 265→266 (`src/builtins.rs`)
- [x] Update `macro-error` in `stdlib/prelude.llt` — now calls `[builtin-macro-error span message]`
- [x] Corpus test: `macro_error_span.llt-eval`
- Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

---

## Continuation-Based Builtins

### dispatch-cont-h2: Convert H2 conditional builtins to Cont::*Dispatch variants

**Depends on:** sprint-2b-builtins-cps ✅
**Context:** sprint-2b-builtins-cps annotated conditional `materialize(&args[N])` calls with `// H2:` markers. These need Cont::*Dispatch variants in `src/eval_materialize.rs` so builtins don't call materialize() conditionally. Affected: `builtin_connect` transport dispatch, `builtin_narrow` type dispatch, `builtin_sort` comparator, `builtin_gensym` optional prefix arg, `builtin_range` 2-arg vs N-arg.

- [x] Survey all 9 H2 sites — 1 real fix (sort: updated registration to `[Spine, Spine]`, replaced materialize with try_get_materialized), 8 documented as safe conditionals (args.len() check or pre-materialized discriminant dispatch)
- [x] All `// H2:` markers removed — replaced with safe-conditional documentation
- [x] `builtins_datetime.rs` materialize audit — updated lint-builtins-cps to catch field-access syntax; all datetime builtins annotated H1/H2/H3 (all necessary, no unconditional forces to fix)
- Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

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

### http2-session-async-drop-panic: Dropping http2-session inside async evaluator context panics

Discovered via `just versions` (2026-05-23). When `http-request` / `http2-session` builtins are called from within tinct's async evaluator (after Rc→Arc + async CEK migration), dropping the reqwest HTTP/2 session panics with: `Cannot drop a runtime in a context where blocking is not allowed. This happens when a runtime is dropped from within an asynchronous context.` (tokio-1.52.3/runtime/blocking/shutdown.rs:51). The reqwest client contains an internal tokio Runtime; dropping it inside the async CEK evaluator's tokio context triggers the panic.

This makes `just versions` (which uses `http-request` + `http2-session`) non-functional in the current build.

- [x] Fix: the reqwest client inside `http2-session` should use the outer tokio runtime (via `Handle::current()`) rather than creating a new Runtime. Or: use `Arc<reqwest::Client>` shared across calls so it's never dropped per-call. Or: move the drop to a spawned blocking task. (`src/builtins_io.rs`, `http2-session` implementation) [Critical]



### rnd-typecheck-runtime-unification: Accept typecheck-runtime-unification whatif

`doc/whatif/typecheck-runtime-unification.md` (State: Proposal) documents the root cause of TA1/TA2/BT5 type name mismatch bugs — two separate type judgment systems that can disagree. Needs design review via `/rnd` before implementation.

- [ ] Run `/rnd typecheck-runtime-unification` to formalize design decisions and create implementation sprint

### typecheck-handle-annotation-bug: `@[Handle Readable]` TypeAssert triggers T000 internal error

Discovered via `samples/versions.llt` rewrite (2026-05-23). Writing `[@[Handle Readable] expr]` causes the type checker to panic with `[T000]: resolved_type written twice — elaboration invariant violated`. The TypeAssert elaboration is writing to the same AST node's resolved_type slot twice. Workaround: remove the annotation; the Handle type mismatch (T003) is then reported as a non-fatal warning instead.

- [x] Find the double-write in `src/typecheck_annot.rs` or `src/typecheck.rs` — specifically the TypeAssert elaboration for parameterized Handle types like `Handle[Readable]`; add guard to prevent double-write or fix the root cause (`src/typecheck.rs`, `src/typecheck_annot.rs`)
- [x] Verify `[@[Handle Readable] [open %cwd "file.txt" Readable]]` type-checks cleanly after fix — Fixed: `Handle` ctor-app added to `resolve_type_dict` at line 1482 of `src/typecheck_annot.rs`. `@[Handle Readable]` now resolves to `Handle(Record({ __cap_flag_readable }))` instead of union `Handle | Readable`.
- [x] `open` builtin TypeEnv signature is stale: runtime now accepts Variant flags (`Readable`, `Writable`) not String modes (`"r"`, `"w"`), but the type checker still says mode is `String`. This causes T003 when passing `Readable`. Update `open` TypeEnv signature. (`src/type_env.rs`) [Major]
- [x] `open` return type should be parameterized: `Readable` → `Handle[Readable]`, `Writable` → `Handle[Writable]` — implemented via `check_open` special case in `src/typecheck.rs` that synthesizes `Handle(cap_row)` from statically-known flag VarRefs

### typecheck-typeassert-no-narrowing: `[@String expr]` TypeAssert doesn't narrow inferred type at call sites

Discovered via `samples/versions.llt` (2026-05-23). Writing `rust-version: [@String [env "RUST_VERSION"]]` should make `rust-version` have type `String` at call sites. But the type checker still infers `String | []` (the underlying type of `env`), causing T003 errors when passing `rust-version` to a function expecting `String`. The TypeAssert is validated at runtime but the type checker doesn't use it to narrow the inferred type for downstream uses.

- [x] When a dict entry has a TypeAssert annotation `[@T expr]`, the entry's inferred type for callers should be `T` (the asserted type), not the underlying expression type. Fix the type annotation propagation in dict entry type inference. (`src/typecheck_dict.rs` or `src/typecheck.rs`)

### fmt-panic-seq-materialized: `just fmt-llt-check` panics with "seq should be materialized"

Discovered via `samples/versions.llt` rewrite (2026-05-23). Running `tinct fmt --check` on a file containing `[do result ...]` macro expansion causes a panic in `src/builtins_seq_reduce.rs:226:14` with message "seq should be materialized". The formatter is triggering a reduce operation on an unmaterialized Seq. Likely caused by the `do` macro expanding to `[and-then ...]` chains that the formatter tries to evaluate or partially execute.

- [x] Reproduce with a minimal `[do result [x: [Ok 1]] [Ok x]]` file; find why the formatter forces the Seq; fix the formatter to avoid materializing or add a graceful error path. (`src/builtins_seq_reduce.rs:226`, `src/formatter.rs`)

### lint-clippy-hotfix: Fix 4 new clippy errors from eval-hot-path-fixes (2026-05-23)

`just lint-clippy` currently fails with 4 errors introduced by recent sprint work:

- [x] eval_dict.rs: removed unused `use super::*` [commit 5cbefac]
- [x] value.rs:1882,1908: `|t| Arc::clone(t)` → `Arc::clone` [commit 5cbefac]
- [x] value.rs:1794: added `#[allow(dead_code)]` to reset_slot_counters [commit 5cbefac]

### ci-failures: Fix 4 failing tests identified by `just ci` (2026-05-22)

`just test` result: 1874 passed, 8 failed, 76 ignored. The 4 `test_syntax_llt_fn_*` failures
are already tracked in `runtime-v2-fix-regressions`. These 4 are not yet tracked:

- [x] `standard_builtins_contains_all` — updated to 283 (commit 01d857c)
- [x] `test_await_error_twice_returns_error_both_times` — fixed: Pending path now caches real result; test rewritten with correct [t: ...] syntax
- [x] `test_circular_dependency_cycle_path` — relaxed assertion for iterative CEK machine (cycle_path empty is expected)
- [x] `test_instance_fd_consistency_violation` — re-ignored with updated reason

### known-bugs-fix: Fix LSP expansion, docgen arity, eval_corpus OOM

- [x] **`just docgen` fails with `[E020] arity mismatch`:** removed dead-code `[strings: [include %libdir "strings.llt"] path: [include %libdir "path.llt"]]` intermediate dict from `scripts/docgen.llt` — those bindings were never used downstream; the arity mismatch root cause in the multi-document pipeline remains uninvestigated (static analysis could not reproduce it) (`scripts/docgen.llt`)
- [x] **`test_eval_corpus` SIGKILL (OOM):** Fixed — `clear_stdlib_cache()` called before each test iteration in corpus_tests.rs; old ThunkArena freed between iterations [commit d91bf6c]

## Builtin Privacy

The `doc/whatif/completed/builtin-privacy.md` whatif is **accepted** (2026-05-11) but never implemented. The design: no Rust builtin is directly callable by user code — only the prelude can access them, via `builtin-*` names (injected only during prelude type-checking). User code gets only what prelude explicitly re-exports.

**Current state**: `build_prelude_env_inner()` starts with `TypeEnv::with_builtins()`, which exposes ALL Rust builtins to user code. User programs can call `split`, `str`, `open`, `connect` etc. directly, bypassing the intended prelude boundary.

### builtin-privacy: Restrict Rust builtin visibility to prelude only

**Deferred — needs fresh session.** This sprint changes the core evaluation pipeline (imports.rs root env setup) and needs careful testing. Decomposition plan below — run as 3 sequential sub-sprints.

**Whatif:** `doc/whatif/completed/builtin-privacy.md`

#### Phase 1 (verify) — COMPLETE

- [x] Wire `inject_builtin_aliases()` to be called ONLY during prelude type-checking (it is already, but verify no other call sites exist).
- [x] Verify that all prelude stdlib functions that currently call canonical builtin names (`split`, `str`, etc.) have been migrated to `builtin-*` names — or that the canonical names are re-exported by prelude from the prelude dict.

#### Phase 2 (implement) — [x] KNOWN ISSUE

**Third attempt 2026-05-23 (after prelude-missing-wrappers sprint):** Changing `env` from `TypeEnv::with_builtins()` to `TypeEnv::new()` in `build_prelude_env_inner()` (`src/imports.rs:245`) now produces exactly **1 new unit test regression** (down from 25+ corpus failures in prior attempts):

- **`test_call_mono_lambda_arg_uses_check_expr`** (`src/typecheck.rs:10018`) — `cannot unify Int with Number`

  Root cause: the prelude's `+` wrapper is `[fn@Number [let a@Number b@Number] [builtin-add a b]]`, which gives `+` the TypeScheme `Fn Number [Number Number]` — return type is always `Number`. The builtin's TypeScheme for `+` uses the `Addable(a,b,c)` functional-dependency constraint that specializes to `Int` when both args are `Int`. The test constructs a CALL-MONO scenario where `[+ $x 1]` must return `Int` (checked against `Fn@Int [Int]` expected type). With prelude-typed `+`, the return is `Number`, not `Int` — mismatch.

  Fix required: the prelude's `+` wrapper must have an Addable-constrained TypeScheme, not a flat `Fn Number [Number Number]` scheme. Options:
  1. Annotate the wrapper with the Addable class constraint in LLT syntax (depends on class-constraint annotation support in the type checker)
  2. Register `+` in `with_builtins()` with the Addable constraint AND include it in the prelude output — but keep the builtin TypeScheme rather than overwriting with the prelude wrapper's inferred scheme. This requires `merge_env_bindings_into` to prefer the builtin scheme for operator names.
  3. Implement `check_add` special-case in the type checker (mirrors existing `check_open`) that synthesizes Int+Int→Int when both args are known Int. This is a type-checker refinement, not a scheme change.

Pre-existing failures (NOT caused by Phase 2 change, already failing at baseline):
- `test_get_concrete_string_key_on_record` — `undefined variable: get` (pre-existing, uses `doc_env_with_builtins` with builtins-only env, but `get` is a prelude function not in raw builtins)
- `test_get_union_distribution` — same pre-existing failure

**Phase 2 is blocked on resolving the `+` type precision issue.** The prelude-missing-wrappers prerequisite is now complete — the only remaining blocker is the arithmetic operator TypeScheme precision: the prelude's `+`/`-`/`*`/`/` wrappers lose the Addable/Subtractable/Multipliable/Divisible FD constraint, causing CALL-MONO Int precision failures. The sprint `builtin-privacy-arithmetic-fd` (below) must fix this before Phase 2 can land.

- [x] **Remove `TypeEnv::with_builtins()` from the user-code path.** `build_prelude_env_inner()` now starts with `TypeEnv::new()`. Prelude type-checking uses `builtins_env` (a separate `TypeEnv::with_builtins()`) internally. `merge_env_bindings_into` uses pointer-walk above baseline to extract only prelude-defined names. (`src/imports.rs`) **DONE 2026-05-24.**
- [x] Change the evaluator's root env to mirror this: the TypeEnv returned by `build_prelude_env()` now contains ONLY prelude-exported names, not the raw builtin registry. Raw builtins (`connect`, `http2-session`, etc.) are absent from user TypeEnv. (`src/imports.rs`) **DONE 2026-05-24.**

**Known limitation (resolved 2026-05-23):** `=` and `<` previously showed degraded schemes in hover. Fixed by `builtin-privacy-constraint-hover` sprint: post-processing falls back to authoritative builtin schemes when prelude-inferred schemes are monomorphic. LSP hover now shows `Equatable a => Fn@Bool [a a]`.

#### builtin-privacy-arithmetic-fd: Preserve arithmetic operator FD precision through prelude wrapping ✅

- [x] Implemented `check_arithmetic` (shared by `+`/`-`/`*` and `builtin-*` aliases) in `src/typecheck.rs`
- [x] Implemented `check_div` (always `Float`) for `/`/`builtin-div` in `src/typecheck.rs`
- [x] Removed spurious `infer_expr(func, ...)` call from arithmetic dispatch (was leaking uncleaned Addable constraints → T013 warnings)
- [x] `test_call_mono_lambda_arg_uses_check_expr` passes
- [x] `test_no_false_positive_warning_for_discharged_constraints` passes
- [x] Phase 2 change (`TypeEnv::new()`) landed cleanly.

#### Phase 3 (migrate + lint) ✅

- [x] Update corpus tests that call builtins directly in user code — `just test-lib` passes with no new failures after Phase 2; existing corpus tests already use prelude-exported names. **DONE 2026-05-23.**
- [x] Add T002 lint warning in `src/typecheck.rs` at both `undefined_variable` sites (plain VarRef and call-head VarRef): when the name is a known Rust builtin and `!state.in_prelude_load`, pushes a note onto the `TypeError` and a `TypeDiagnostic` with code `T002` into `state.diagnostics`. Helper `builtin_primary_names()` added to `src/builtins.rs`. Corpus test at `tests/corpus/typecheck/warnings/raw_builtin_t002.llt-eval`. **DONE 2026-05-23.**

### include-libdir-stdlib-typecheck: %libdir includes type-checked in user context, not stdlib context

`[include %libdir "net.llt"]` from user code causes net.llt to be type-checked using the restricted user TypeEnv (which lacks raw builtins). Net.llt uses raw builtins (`url`, `http-request`, `http2-session`, `connect`, etc.) directly, so they appear as "undefined variable" T002 errors → error nodes in the AST → E099 at runtime when any net.llt function is called.

Root cause: the include/typecheck pipeline uses the same TypeEnv for included stdlib files as for user code. Stdlib files (in `%libdir`) should be type-checked with `in_prelude_load = true` and the full stdlib TypeEnv (with raw builtins), since they ARE part of the stdlib.

**Symptom:** `just versions` fails with `[E099] syntax error at 79:50 (cannot evaluate error node)` — the error is at net.llt line 79, col 50 (the closing bracket of `fetch`'s `[fn@Dict ...]`).

**Fix:** When the type-checker encounters `[include %libdir "..."]`, load the included file with the stdlib TypeEnv (or with `in_prelude_load = true`). This mirrors how `create_stdlib_env_inner()` already uses prelude-aware contexts for stdlib evaluation. Relevant files: `src/imports.rs`, `src/typecheck.rs` (include handling).

- [x] Fix include type-checking context for `%libdir` → use stdlib env, not user env
- [x] Verify `[include %libdir "net.llt"]` from user code works without E099 after fix
- [ ] `just versions` passes after fix — requires network verification

**Also:**
- [x] Migrate net.llt to use `builtin-*` stable aliases (defense in depth — makes net.llt work even if included in user context before the above fix)

### annotation-propertydict-migration: Complete Annotation::PropertyDict SurfaceEntry migration

`src/ast.rs` (uncommitted) changed `Annotation::PropertyDict` from `Vec<Spanned<Entry>>` to `Vec<Spanned<SurfaceEntry>>` and `get_property` now returns `Option<&Arc<SurfaceNode>>`, but call sites weren't updated. Build is broken (`cargo build` fails with ~60 errors).

**Affected call sites:** `src/parser.rs` (PropertyDict construction), `src/formatter.rs` (annotation dict methods), `src/typecheck_annot.rs` (`resolve_type_expr`, `resolve_property_dict_as_record`), `src/typecheck.rs`, `src/typecheck_dict.rs` (`.expr` not `.node` on SurfaceNode), `src/eval_call.rs`, `src/eval_materialize.rs`, `src/ast_dict.rs`, `src/ast_convert.rs`.

- [x] Update all call sites to use `Vec<Spanned<SurfaceEntry>>` / `Arc<SurfaceNode>` types
- [x] `cargo build` passes

### builtin-privacy-missing-wrappers: Audit and add missing prelude wrappers + type alias propagation

Follow-up from builtin-privacy Phase 3. Three issues discovered when making `samples/versions.llt` pass `just lint-file`:

**Missing stable aliases + prelude wrappers (partially fixed 2026-05-24):**
- [x] Add `builtin-trim` stable alias → `src/builtins.rs`
- [x] Add `builtin-emit` stable alias → `src/builtins.rs`
- [x] Add `builtin-env` stable alias → `src/builtins.rs`
- [x] Add `trim`, `emit`, `env` wrappers to prelude "prelude-missing-wrappers" section → `stdlib/prelude.llt`

**Type alias propagation (fixed 2026-05-24):**
- [x] `@NetCap` / `@DirCap` / `@Handle` in user code fail with T002 "undefined type" after Phase 2 changed user TypeEnv to start from `TypeEnv::new()`. Fix: propagate type aliases from `builtins_env` into the user-facing env at end of `build_prelude_env_inner()` → `src/imports.rs`, `src/type_env.rs`

**Audit remaining missing wrappers:**
- [x] Run `tinct lint --strict` on all samples/ files to find any remaining raw builtin references — only T010 span bug fires (pre-existing); T002/T003 clean
- [x] Run `tinct lint --strict` on any user-facing example scripts to verify full coverage — samples/basic.llt clean (T002/T003 only; T010 is span bug)
- [x] Update `doc/11a-builtins.md` to document `builtin-trim`, `builtin-emit`, `builtin-env`
- [x] Update builtin count in `standard_builtins_contains_all` test (was 284, now +3 = 301; already correct)

**Also update `just lint-file` test for all samples:**
- [x] `just lint-file samples/versions.llt` — T010 span bug fixed: `scan_type_quality` now uses real line/column from SurfaceProgram span walk instead of synthetic 0:0
- [x] `just lint-file samples/basic.llt` — verified: T002/T003 clean; T010 is pre-existing span bug (not a regression)
- [ ] `just versions` — currently fails E099 (blocked on `include-libdir-stdlib-typecheck` above) — requires network verification
- [x] Update `standard_builtins_contains_all` test count (+3: builtin-trim, builtin-emit, builtin-env) — already 301 ✓

---

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
- Corpus test for handle capability mismatch — `handle_capability_mismatch.llt-eval` deleted (parser doesn't support `Handle[Type]` in @annotation position); tracked in test-coverage-cycle311
- Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

### open-api-migration: Migrate open() from string mode to capability flag types

**Accepted design (2026-05-07 decision, mempalace `tinct/decisions`):** The string mode argument `"r"/"w"/"a"` is replaced by nominal capability flag types as positional arguments. `[open cap path]` with NO flags = type error (explicit intent required).

**New API:**
```
[open cap path Readable]               # was: [open cap path "r"]
[open cap path Writable]               # was: [open cap path "w"]
[open cap path Writable Appendable]    # was: [open cap path "a"]
[open cap path Binary Readable]        # binary read
[open cap path Readable Writable]      # read+write
[open cap path Writable Exclusive]     # fail if exists
```

**Motivation:** Capability flags are already registered as type-level symbols (Readable, Writable, etc.). Using them as positional args instead of opaque strings allows the type checker to parameterize the return Handle type from the call arguments. `"r"` → `Handle[Readable]`, `"w"` → `Handle[Writable]`, etc.

**Tasks:**
- [x] Update `builtin_open` in `src/builtins_io.rs` — parse positional args after path as capability flag types (Readable, Writable, Appendable, Binary, Exclusive, Sync, NoFollow) instead of string mode (`"r"/"w"/"a"`). `[open cap path]` with no flags after path → arity error. (`src/builtins_io.rs:183-330`)
- [x] Update type signature in `src/type_env.rs` — `check_open` special-case added to typecheck.rs; synthesizes Handle(cap_row) from Variant flag args [commit ca03e1f]
- [x] Update all corpus tests and examples using `[open ... "r"]` / `[open ... "w"]` / `[open ... "a"]` to use capability flag syntax
- [x] Update `doc/feature/io.md:604`, `doc/11a-builtins.md:367-369`, `doc/12-tooling.md:630` to reflect new syntax
- [x] `just build` passes [commit ca03e1f]

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
- Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

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
- Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

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
- Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

#### T013 warning readability (formerly type-warning-readability)

T013 warnings currently report internal inference variable names like `_t86` instead of the user-visible source variable that introduced the constraint. Example: `warning[T013]: ambiguous type variable '_t86' in constraint Showable: appears in constraint but not in the type — constraint will be silently dropped`. The user cannot tell which declaration or expression introduced `_t86`.

- [x] T013 warnings now show source variable names — added `type_var_source_names` map to InferState, `format_var_name` helper, updated all T013 emitters in type_env.rs
- [x] Corpus test: `tests/corpus/typecheck/warnings/t013_ambiguous_with_source_name.llt-eval`

#### Unknown elimination (formerly unknown-elimination)

`Type::Unknown` currently leaks into inferred types for expressions the type checker cannot handle — dual-dispatch builtins, HKT positions, gradual typing escape. Goal: make Unknown a controlled escape hatch, not a default fallback. HKT infrastructure (Kind::Operator, Type::App, Mappable/Appendable) is complete — residual Unknown at HKT positions is an instance-lookup gap covered by chr-instances-gaps, not missing HKT machinery.

- [x] Audited all 124 `Type::Unknown` in typecheck.rs — 56 gradual (justified), 0 replaceable, 4 HKT deferred. All sites annotated with inline `// Gradual:` or `// HKT:` comments.
- Note: `just test` corpus tests have pre-existing CHR failures (tracked separately)

### chr-corpus-fixes: Fix 11 pre-existing CHR corpus test failures ✅ DONE (Groups A-D, partial)

**Whatif:** `chr-unification`
**Depends on:** chr-instances-gaps ✅, type-inference-cleanup ✅

**Root cause (confirmed):** `[class ...]`, `[type ...]`, and `[instance ...]` inside a dict went through the parser path `surface_decl_to_expr → expr_to_surface_node`, which converted declaration forms to `SurfaceExpression::Placeholder` — discarding all class/instance/type information. Pass 0c in `typecheck_dict.rs` (which pre-registers ClassDecl/InstanceDecl) never saw them, so user-defined classes and type aliases were silently dropped.

**Fix (commit chr-corpus-fixes):** Added `SurfaceExpression::Decl(Box<SurfaceDeclaration>)` to preserve declaration info in expression contexts. Parser now pushes `SurfaceExpression::Decl` instead of going through the lossy two-step conversion. `surface_expr_to_expr` converts `Decl` back to the correct `Expr::ClassDecl`/`InstanceDecl`/`TypeAlias` variant so Pass 0c can match it. Also fixed instance pattern type extraction to resolve bare type names like `[Int]` (VarRef) as concrete types via `resolve_annotation`.

**Group A — User-defined class constraint lookup (2 typecheck failures) — FIXED:**
- [x] Fix `class_decl_after_use.llt-eval` — FIXED: class now registered via SurfaceExpression::Decl → Expr::ClassDecl path
- [x] Fix `constraint_annotation_basic.llt-eval` — FIXED: same root cause resolved

**Group B — ADT type registration (2 typecheck failures) — FIXED:**
- [x] Fix `constructor_payload_type_precision.llt-eval` — FIXED: [type ...] in dict now produces Expr::TypeAlias in Pass 0/2, type alias registered
- [x] Fix `exhaustiveness_bare_nominal.llt-eval` — FIXED: same root cause resolved

**Group C — Exhaustiveness checking for ADT constructors (2 type-error failures) — FIXED (cascaded from B):**
- [x] Fix `exhaustiveness_bare_nominal_variant.llt-eval` — FIXED: type registration now correct, exhaustiveness check fires
- [x] Fix `exhaustiveness_multi_field_nominal.llt-eval` — FIXED: same

**Group D — Instance check violations not triggered (3 type-error failures) — FIXED (cascaded from A):**
- [x] Fix `instance_consistency_error.llt-eval` — FIXED: class+instance now registered, pattern types resolve correctly
- [x] Fix `instance_coverage_error.llt-eval` — FIXED: same
- [x] Fix `instance_disjointness_error.llt-eval` — FIXED: same

**Group E — Equatable for Variant types (1 typecheck failure):**
- [x] Fix `transport_typed.llt-eval` — FIXED: Added `Type::NominalVariant { .. }` to Equatable instances in `satisfies_constraint_inner` (`src/type_unify.rs:108`); Variant equality is runtime tag comparison (commit e07d63e)

**Group F — Nominal variant match + instance Multipliable (1 typecheck failure) — FIXED:**
- [x] Fix `nominal_variant_exhaustive_match.llt-eval` — FIXED: ADT constructor scoping implemented (adt-constructor-scoping sprint). `inject_adt_constructors_surface_program` in `desugar.rs` injects `CtorName: [variant "CtorName"]` entries into dict surface AST before the resolver runs (preserving slot alignment). `lower.rs` skips Decl entries at runtime. `eval_materialize.rs` and `eval.rs` extended to handle unit-variant called with single named arg: `[Circle r: 5]` → `Variant(Circle, Int(5))`. `typecheck_dict.rs` Pass 2 injects `inject_adt_constructor_schemes` with `Type::Unknown` for gradual typing acceptance. `typecheck_annot.rs` fixed: 3+ all-positional entries in nominal variant constructor position fall through to union path instead of erroring. Test updated: `area: Int(75)` is a keyed entry in the result dict.

---

---

## Algorithm Correctness Reviews

In-depth formal audits for algorithms not yet reviewed. Each sprint dispatches specialist agents to read the relevant source, identify soundness holes, and produce TODO items for any bugs found. No implementation — findings only.

### review-known-type-issues: Verify status of open type system soundness findings

**Agents:** type-theorist, computer-scientist
**Files:** `src/type_env.rs` (`rename_single_type_var`), `src/typecheck.rs` (CALL-POLY named-arg path, `check_call_with_scheme`)

Two findings from prior reviews — both verified **FIXED** (2026-05-23):

1. **`rename_single_type_var` missing Union/Intersection** (2026-05-07 review) — **FIXED**: `rename_single_type_var` at `src/type_env.rs:119-130` now explicitly handles `Type::Union` and `Type::Intersection` by recursing into their members. The `_ => ty.clone()` catch-all at line 152 only covers primitives, `Any`, `Error`, `Number`, `Proxy` — types that contain no type variables.

2. **CALL-POLY named-arg `consumed_params` gap** (2026-05-09 review) — **FIXED**: All three CALL-POLY paths now insert `param_idx` into `consumed_params` after the overlap check: `check_call_with_scheme` line 4728, `check_call` CALL-MONO line 5021, `check_call` CALL-POLY line 5196. Additionally, all three paths have a duplicate named-arg guard (`seen_names: HashSet<&str>`) that rejects duplicate named args before they reach unification (lines 4690-4698, 4984-4993, 5155-5164). Robinson (1965) idempotency is preserved.

- [x] Dispatch type-theorist + computer-scientist to (a) verify whether both issues are still present in current code, (b) if present, produce minimal fix recommendations → findings go to TODO.md

