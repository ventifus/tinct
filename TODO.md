# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## runtime-v2-rebase: Land runtime-v2 Part A types onto main

**Branch source:** `runtime-v2` (1 unique commit: `036a9704` — TODO.md description + lib.rs whitespace only)
**Plan:** `doc/whatif/plans/runtime-v2-plan.md` — **this rebase covers Plan Part A only** (new type hierarchy added alongside existing types; `Expr`/`File` NOT deleted). Parts B–E are the next atomic sprint.
**Prerequisites confirmed complete on main:** `exhaustiveness-multi-field-nominal` ✅, `include-decomp-eval-primitives` ✅, `include-decomp-prelude` ✅, `include-decomp-review` ✅
**Test command:** `just test`

**Why the Dict-based `builtin_load/expand/eval` interface is kept:** Per the plan, `include-decomp-prelude` establishes the Dict→Dict pipeline as an explicit intermediate step. `runtime-v2-plan.md` lists it as a prerequisite: *"`include-decomp-prelude` — Self-hosted pipeline in prelude; `expand` signature is `Dict→Dict` at this point."* The API changes (`load → Program`, `expand → Program`, `eval → [Seq Expression]`) are staged in Plan Part F, after Part E deletes the old types. The runtime-v2 branch made these changes out of order (before Parts B–E); we defer them to their correct position in Sprint 1F.

**What the rebase does NOT do (deferred per plan):**
- Part B: Parser returns `SurfaceProgram`; typechecker walks `SurfaceProgram`; expander migrates — atomic with Part E
- Part E: Delete `Expr`/`File`/`Document`; evaluator uses `CoreExpr` — first `cargo check` checkpoint per plan
- Part F: `builtin_load → Value::Program`, `builtin_expand → Value::Program`, `builtin_eval → [Seq Expression]`
- Part G: Prelude pipeline updated to use `Program`/`Document` types (structure unchanged; `eval-file` gains `ast@Program`, `doc.name` becomes `DocumentName` match)

### Phase 1 — Git rebase

- [ ] `git checkout runtime-v2 && git rebase main` — expected conflict only in TODO.md (the single unique commit only touches TODO.md + whitespace); resolve by combining both TODO structures (`TODO.md`)
- [ ] If rebase fails with unexpected conflicts: fall back to `git checkout -b runtime-v2-rebased main` + cherry-pick runtime-v2 changes file by file (`git`)

### Phase 2 — New Rust files (additive, no conflicts)

- [ ] Copy `src/lower.rs` from runtime-v2 — per-thunk lowering pass (SurfaceExpression → CoreExpr); `#![allow(dead_code)]` already present; verify referenced types exist after Phase 3 (`src/lower.rs`)
- [ ] Copy `src/ast_convert.rs` from runtime-v2 — bridge converter `file_to_surface_program()` / `surface_node_to_expr()` (transitional; deleted in Part E); verify it compiles with ast.rs after Phase 3 (`src/ast_convert.rs`)
- [ ] Copy `src/surface_fields.rs` from runtime-v2 — `surface_expr_tag()`, `surface_node_get_field()`, `dot_key_to_value()`, `annotation_to_value()`; verify after Phase 3 (`src/surface_fields.rs`)

### Phase 3 — src/ast.rs: Add Surface/Core types alongside existing

Keep ALL existing types (`Expr`, `File`, `Document`, etc.) — they remain the primary eval path until Part E.

- [ ] Add `use std::sync::Arc;` and `use std::collections::HashMap;` to imports (`src/ast.rs`)
- [ ] Add `NodeId(usize)` newtype and `node_id(arc: &Arc<SurfaceNode>) -> NodeId` helper (derived from `Arc::as_ptr()`) (`src/ast.rs`)
- [ ] Add `ResolutionTable(HashMap<NodeId, (u32, u32)>)` and `TypeAnnotationTable(HashMap<NodeId, Type>)` (`src/ast.rs`)
- [ ] Add `SurfaceExpression` enum (24 variants: `Int`, `Float`, `Bool`, `Str`, `VarRef`, `DotAccess`, `Pipe`, `Sequential`, `Dict`, `Call`, `Fn`, `TypeAssert`, `Annotated`, `Rest`, `Match`, `Quote`, `Unquote`, `UnquoteSplice`, `TypeApp`, `PatternDecl`, `LetDecl`, `CaseArm`, `Placeholder`, `Error`) (`src/ast.rs`)
- [ ] Add supporting types: `SurfaceEntry`, `SurfaceParam`, `SurfaceNamedArg`, `SurfaceMatchArm` (`src/ast.rs`)
- [ ] Add `SurfaceNode { expr: SurfaceExpression, span: Span }` wrapper struct (`src/ast.rs`)
- [ ] Add `SurfaceDeclaration` enum (TypeAlias, ClassDecl, InstanceDecl, DefMacro, MacroDecl, Splice, SyntaxClass) (`src/ast.rs`)
- [ ] Add `SurfaceItem` enum (`Expr(Arc<SurfaceNode>)`, `Decl(Spanned<SurfaceDeclaration>)`) (`src/ast.rs`)
- [ ] Add `SurfaceDocument { stage, name, items }` and `SurfaceProgram { documents }` (`src/ast.rs`)
- [ ] Add `CoreExpr` enum (Int, Float, Bool, Str, Var{name,level,slot}, FreeVar, DotAccess, Sequential, Dict, Call, Fn, TypeAssert, Annotated, Rest, Match, Quote, Unquote, UnquoteSplice, TypeApp, PatternDecl, LetDecl, CaseArm, Placeholder, Error, RuntimeTypeCheck) (`src/ast.rs`)
- [ ] Add `CoreEntry`, `CoreParam`, `CoreNamedArg`, `CoreMatchArm` supporting types (`src/ast.rs`)
- [ ] `cargo check` passes after this phase (`src/ast.rs`)

### Phase 4 — src/value.rs: Add new Value variants + ThunkState additions

Keep all main's changes (RustRegistry deleted, `env_id` on `Value::Function`, type aliases).

- [ ] Add `Value::Program(Arc<SurfaceProgram>)`, `Value::Document(Arc<SurfaceDocument>)`, `Value::Expression(Arc<SurfaceNode>)` to Value enum (`src/value.rs`)
- [ ] Update `type_name()` for new variants: "Program", "Document", "Expression" (`src/value.rs`)
- [ ] Update `Display` and `Debug` impls for new variants (`src/value.rs`)
- [ ] Update `dict?` to return `false` for Program/Document/Expression (`src/value.rs`)
- [ ] Add `ThunkState::Surface { node: Arc<SurfaceNode>, res: Arc<ResolutionTable>, types: Arc<TypeAnnotationTable>, env: Rc<RefCell<Environment>>, ctx: Rc<EvalContext> }` variant (`src/value.rs`)
- [ ] Add `ThunkState::AstNodeField { node: Arc<SurfaceNode>, field: &'static str }` variant (`src/value.rs`)
- [ ] Add `take_surface()`, `take_ast_node_field()`, `Thunk::new_ast_node_field()` methods (`src/value.rs`)
- [ ] Update `Pattern::Constructor` to handle `Value::Expression` match dispatch (lazy AstNodeField thunks per bound variable) (`src/eval_materialize.rs`)
- [ ] `cargo check` passes after this phase

### Phase 5 — src/resolve.rs: Add SurfaceResolver (additive)

Keep all existing `resolve_file()` — it remains the primary path until Part E.

- [ ] Add `SurfaceResolver` struct with scope-chain semantics (`src/resolve.rs`)
- [ ] Add `resolve_surface_program(program: &SurfaceProgram) -> ResolutionTable` top-level entry point (`src/resolve.rs`)

### Phase 6 — src/eval.rs: Add Surface/AstNodeField thunk handlers

- [ ] Add `ThunkState::Surface` handler: call `ast_convert::surface_node_to_expr()` → evaluate the resulting `Expr` (bridge to old eval path; removed in Part E) (`src/eval.rs`)
- [ ] Add `ThunkState::AstNodeField` handler: call `surface_fields::surface_node_get_field()` → `Materialized` (`src/eval.rs`)
- [ ] Update `[quote expr]` to produce `Value::Expression` instead of `Value::Variant` (additive; doesn't affect include pipeline) (`src/eval.rs`)
- [ ] `cargo check` passes after this phase

### Phase 7 — src/eval_materialize.rs: DotAccess on Value::Expression

- [ ] Add `DotAccessForce` handler for `Value::Expression`: `surface_node_get_field(field)` → lazy `AstNodeField` thunk (`src/eval_materialize.rs`)

### Phase 8 — src/builtins_meta.rs: Selective merge

KEEP main's: `builtin_eval`, `builtin_eval_types`, `builtin_expand` (Dict→Dict interface — this is the planned intermediate state per `runtime-v2-plan.md` prerequisite chain).
DEFER per plan: `builtin_load → Value::Program`, `builtin_expand → Value::Program`, `builtin_eval → [Seq Expression]` — these belong in Sprint 1 Part F (after Part E deletes old types).
ADD runtime-v2's: `builtin_ast_of` returning `Value::Expression` (+ registration fix — this is Part F, but safe to add early since it's additive).

- [ ] Verify `builtin_load` still returns `Value::Dict` (the plan's explicit intermediate state); runtime-v2 branch made this change out of order — do NOT apply it; keep main's implementation (`src/builtins_meta.rs`)
- [ ] Change `builtin_ast_of` to return `Value::Expression(Arc<SurfaceNode>)` instead of `Value::Variant`; convert `Unevaluated` and `Materialized` branches to build a `SurfaceNode` via `ast_convert` (`src/builtins_meta.rs`)
- [ ] Register `builtin_ast_of` in `standard_builtins()` — **fixes the integration bug from Health Review #22** (`src/builtins.rs`)
- [ ] Update the 3 `ast_of_*.llt-eval` corpus tests that currently assert `undefined variable: ast-of` — they should now succeed and return `Value::Expression` (`tests/corpus/eval/builtins/`)
- [ ] Verify `builtin_expand` is still the Dict→Dict implementation from main; runtime-v2 replaced it with an identity stub out of order — do NOT apply that change; keep main's implementation (`src/builtins_meta.rs`)

### Phase 9 — stdlib/prelude.llt: Add type declarations

Keep ALL of main's include pipeline (`eval-document-pipeline`, `eval-file`, `include`, `cli-pipeline`). Insert runtime-v2's type declarations in the declarations section (before `variant?`/`payload-of`).

- [ ] Add `Span` type declaration (`stdlib/prelude.llt`)
- [ ] Add `DotKey` type declaration (`stdlib/prelude.llt`)
- [ ] Add `Annotation` and `AnnotationEntry` type declarations (`stdlib/prelude.llt`)
- [ ] Add `Parameter`, `NamedArg`, `Entry`, `MatchArm` type declarations (`stdlib/prelude.llt`)
- [ ] Add `Expression` type declaration (24 variants: IntLiteral, FloatLiteral, BoolLiteral, StrLiteral, Var, DotAccess, Pipe, Sequential, Dict, Call, Fn, TypeAssert, Annotated, Rest, Match, Quote, Unquote, UnquoteSplice, TypeApp, PatternDecl, LetDecl, CaseArm, Placeholder, Error) (`stdlib/prelude.llt`)
- [ ] Add `Declaration`, `DocumentName`, `Document`, `Program` type declarations (`stdlib/prelude.llt`)
- [ ] Add `variant?` and `payload-of` functions (`stdlib/prelude.llt`)
- [ ] Verify load order: type declarations must appear before any stdlib code that pattern-matches on them (`stdlib/prelude.llt`)

### Phase 10 — Post-rebase verification

- [ ] `cargo check` — zero errors, zero warnings
- [ ] `just test` — all tests pass; update `ast_of_*.llt-eval` corpus test expectations as needed
- [ ] `just test-lib` — passes
- [ ] Verify include end-to-end: `just run` with a program that uses `[include %libdir "..."]`
- [ ] Commit: `runtime-v2-rebase: land SurfaceNode/CoreExpr/Value::Expression types onto main`

### New issues uncovered during rebase analysis

- [ ] **BUG** `stdlib/codecs/json.llt:to-json` has dead `[Program _]`/`[Document _]`/`[Expression _]` match arms on current main (those Value variants don't exist in Rust yet); after rebase they activate — add corpus tests: `to_json_expression.llt-eval`, `to_json_program.llt-eval` to verify correct serialization (`tests/corpus/eval/builtins/`)
- [ ] **BUG** `builtin_load` slot indices interaction with Surface thunks: once Part G introduces `ThunkState::Surface`, the pre-resolved VarRef slots in loaded programs become stale in new eval environments; `resolve_surface_program` must be called per eval, not reused from `builtin_load` output; document this constraint in `builtin_load` doc comment and add a TODO guard (`src/builtins_meta.rs`)

---

## runtime-v2 — Sprint 1 (continued): Parts B–G

**Branch:** `main` (post-rebase)
**Depends on:** `runtime-v2-rebase` complete
**Plan:** `doc/whatif/plans/runtime-v2-plan.md`

⚠️ **Parts B–E are one atomic compilation unit** (per plan). Part B migrates the parser to return `SurfaceProgram`; this breaks all downstream code until Part E provides the new evaluator. No `cargo check` checkpoint until Part E is complete. First `cargo check` = after Part E. `just test` = after Part G.

Part A done in rebase. Parts C (`src/lower.rs`), D (`src/surface_fields.rs`) already exist from runtime-v2 branch. Parts B+E still needed.

### Parts B + E — Parser, resolver, typechecker, expander migration + Evaluator cutover (ATOMIC)

**Parser** (`src/parser.rs`): ❌ NOT STARTED
- [ ] Change `parse()` return type from `File` to `SurfaceProgram`; every `Rc::new(spanned_expr)` → `Arc::new(SurfaceNode { expr, span })`; declaration forms route to `SurfaceItem::Decl`, expression forms to `SurfaceItem::Expr` (`src/parser.rs`)

**Resolver** (`src/resolve.rs`): Part B resolver done in rebase (SurfaceResolver added); remaining:
- [ ] Route all callers of `resolve_file()` to `resolve_surface_program()` — once parser returns SurfaceProgram (`src/resolve.rs`, `src/lib.rs`, `src/main.rs`)

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
- [ ] `stdlib/cli/fmt/compact.llt` and `pretty.llt` — update to Expression match dispatch; formatter bug fix is prerequisite OR accept broken formatter tests (`stdlib/cli/fmt/`)
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

## Known Bugs

- [x] `just test-lib` fails with exit 101. **Fixed (2026-05-19):** Root cause was a parser bug in `src/parser.rs`: `pop_last_value_from_frame` in the `Token::Dot` handler correctly handles `CallArg::Positional` but returns a parse error for `CallArg::Named`. This caused `%: state.prev` in `eval-document-runtime` to fail — the `%` was consumed as the named-arg key, then `state` was consumed as its value (before `.prev` was attached via dot-access). Fix: replaced `%: state.prev` with a let-binding `[prev-val: state.prev]` + `%: prev-val` in `stdlib/prelude.llt::eval-document-runtime`. Also added `expand_macros_in_ctx` to `src/expand.rs` (reuses existing stdlib env when `builtin_expand` is called from within evaluation, eliminating the redundant stdlib reload). All 4 `test_syntax_llt_fn_*` tests pass; `just test-lib` exits 0.

- [ ] **Parser bug (tracked for fix):** `named-arg: expr.field` patterns silently misbehave — `pop_last_value_from_frame` in `Token::Dot` handler (src/parser.rs ~line 4434) returns a parse error when it pops a `CallArg::Named` instead of the expected `CallArg::Positional`. Parser recovers by closing the call frame prematurely, producing a wrong AST. Fix: when `pop_last_value_from_frame` finds a `CallArg::Named`, it should NOT pop it as the dot-access target — instead, the dot-access should apply to the named arg's value, or the parser should accumulate DotAccess before consuming the named arg value (`src/parser.rs`).

### Known Bugs (Type Checker)

- [x] `typecheck::tests::test_dot_access_intersection_found` — Intersection type unification bug: fixed by adding `(Type::Record(..), Type::Intersection(..))` arm to `src/type_unify.rs` that distributes unification across intersection members. Tests pass.
- [x] `typecheck::tests::test_dot_access_intersection_missing_field_returns_unknown` — same Intersection unification bug; fixed by same arm. Tests pass.

---

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class: `∀f a. Filterable f ⇒ (a → Bool) → f a → f a`; compare `Mappable` extension vs separate class using `Data.Witherable` as precedent; include instance examples for `Seq` and `Dict` (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint in TODO.md for `from-json @Schema` schema-directed typed parse (`doc/whatif/schema-directed-from-json.md`)

---

## Health Review #22 Findings (2026-05-19)

### eval-hardening: Fix eval engine correctness issues

- [ ] **CRITICAL** `Cont::GuardedValidate` Overlay error path does not check `is_cacheable()` — replace `thunk.cache_failure(&e)` at `src/eval_materialize.rs:1050` with the full `is_cacheable()` guard + `restore.take()` pattern used at line 1198 (`src/eval_materialize.rs`)
- [ ] Fix `work_stack` orphan leak: when `push_structural` fails mid-traversal, partially-pushed `WorkItem::Force` items leak `Rc<Thunk>` references; add `work_stack.clear()` before propagating the error (`src/eval_deep.rs:509-520`)
- [ ] Update `Cont::Memoize` comment at `eval_materialize.rs:760-763` to document that `restore: None` is acceptable in default-fallback paths (not always a bug as comment implies) (`src/eval_materialize.rs`)
- [ ] Add `InProgress → Unevaluated` row to thunk state transition table in `doc/08-evaluation.md:252-265` — this backward edge exists (RestoreState::Unevaluated) but is absent from the table (`doc/08-evaluation.md`)
- [ ] Strengthen `EvalContext::new()` arena comment at `src/eval.rs:358` — warn that stdlib ThunkIds are NOT valid in fresh-arena contexts (`src/eval.rs`)
- [ ] Fix `%_input` synthetic binding name in `wrap_with_nominal_validation` (`src/eval_pipeline.rs:113-158`) — replace with gensym to eliminate collision risk with pathological nesting patterns (`src/eval_pipeline.rs`)
- [ ] `validate_value` enum check is O(n) linear scan — build a hash set from enum values before matching for large-enum performance (`src/builtins_meta.rs:1940`)
- [ ] `builtin_load` slot indices latent bug: `resolve_file` populates VarRef slots for the load-time env, but `builtin_eval` re-uses the AST in a new env where slots may differ — strip resolved slots from `builtin_load` output OR document that `builtin_eval` must re-resolve before arena-phase3 activates; **note:** runtime-v2 Part E will eliminate this entirely (VarRef slots replaced by ResolutionTable keyed by Arc pointer); for now add a doc comment guard (`src/builtins_meta.rs:1548`, `src/ast_dict.rs`)

### bas-type-system: Fix type system soundness issues

- [ ] **CRITICAL** `Union vs Union` unification with TypeVars falls through to hard error — add `(Type::Union(m1), Type::Union(m2))` arm before C-Var1 guards in `type_unify.rs` that defers to `state.deferred_equalities` when both have inference vars (`src/type_unify.rs:1989-2090`)
- [ ] Document and guard eval-layer coupling in `normalize()` — add `NormCtxt::allow_eval: bool` flag; set to `false` inside `unify()` at line 1539 to prevent runtime errors from propagating into type inference (`src/type_normalize.rs:344-404`, `src/type_unify.rs:1539`)
- [ ] Extend coverage checking to bare `NominalVariant` scrutinee types (not wrapped in Union) — add `ConstructorSignature::from_nominal_variant()` and call it when scrutinee is a bare NominalVariant (`src/coverage.rs:170-222`, `src/typecheck.rs`)
- [ ] Fix `Showable` constraint for `Seq`/`Map`/`Record` — add these to the Showable match arm in `satisfies_constraint`; they are showable at runtime but the type checker rejects them (`src/type_unify.rs:114-124`)
- [ ] Remove dead `row_vars` parameter from `collect_all_vars`/`collect_all_vars_vec`/`collect_all_vars_check_occurs` — always empty under BAS (`src/type_def.rs:1079,1148,1238`)
- [ ] Fix `check_do_infer` fast-path: return `Type::Unknown` instead of `state.fresh_type_var()` for unknown methods (orphaned TypeVars pollute `state.levels`) (`src/typecheck.rs:3578`)
- [ ] Update `doc/06-type-inference.md` type grammar to include BAS types (Union, Intersection, Negation, Never) in the main grammar — these are user-expressible via annotations (`doc/06-type-inference.md:6-22,27-41`)
- [ ] Fix `doc/07-type-extensions.md:386-407` archived code block confusion — separate BAS `Row` struct from archived RowTail into distinct code blocks (`doc/07-type-extensions.md`)

### perf-foundations: Performance improvements

- [ ] `validate_value` allocates fresh `String` for every schema key lookup — replace 10 `Key::String("...".to_string())` with `StrKey("...")` at `src/builtins_meta.rs:1791-2007` (zero-alloc wrapper already exists)
- [ ] `builtin_until` allocates `IndexMap::new()` twice per iteration — pass `None` instead of empty maps at `src/builtins_meta.rs:225,245` (`src/builtins_meta.rs`)
- [ ] `eval_dict` allocates `HashMap::new()` for `constructor_precomputed` on every dict — add early-exit guard: only allocate when `has_type_alias` is true (`src/eval_dict.rs:71`)
- [ ] `ast_to_dict` payload dicts have no capacity hints — add `IndexMap::with_capacity(N)` per match arm using statically known field counts (`src/ast_dict.rs:234+`)
- [ ] `deep_materialize` Dict arm allocates `Vec<Key>` scratch — eliminate by passing key count as `usize` into BuildDict and iterating `map.keys()` during assembly (`src/eval_deep.rs:342-358`)
- [ ] `list_to_thunk_id` creates intermediate Vec<ThunkId> — change to accept `impl ExactSizeIterator<Item=ThunkId>` to eliminate the intermediate Vec (`src/ast_dict.rs:99,141,208`)

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

### cs-type-soundness: Fix type system soundness issues found by computer-scientist

- [ ] **MAJOR (LIVE BUG)** `rename_single_type_var` missing `NominalVariant` arm — TypeVars inside NominalVariant fields are not renamed during single-var scheme instantiation; two call sites share the same TypeVar, violating freshness invariant; fix: add `NominalVariant { tag, fields }` arm that calls `rename_single_type_var_in_row(fields, ...)` (`src/type_env.rs:148`)
- [ ] **MAJOR (COMPLETENESS BUG)** Match arm narrowing uses `StringLiteral(tag)` for Constructor patterns instead of `NominalVariant{tag, ...}` — intersection with NominalVariant scrutinee collapses to Never, making pattern-bound variables typed `Never`; fix: use `Type::NominalVariant { tag: tag.clone(), fields: Row::empty() }` for both positive narrowing (line 2242) and negation accumulation (line 2291) (`src/typecheck.rs:2238-2295`)
- [ ] **MAJOR (LATENT Phase 2 bug)** Resolver doesn't create scopes for match arm pattern-bound variables — at runtime names are looked up by name (correct in Phase 1) but de Bruijn coordinates are wrong for FlatEnv Phase 2; fix: collect pattern-bound names (Variable, Dict, Seq, Constructor patterns) and enter/exit scope for each arm (`src/resolve.rs:308-315`)
- [ ] **MAJOR** `ScopeId` struct is dead code — module doc claims Flatt (2016) scope-set hygiene but `ScopeId::fresh()` was deleted and `ScopeId` is never instantiated; actual model is KFFD-class gensym hygiene; fix: update expand.rs module doc to describe gensym-based manual hygiene accurately; remove `ScopeId` struct and `impl` block (`src/expand.rs:16-55`)
- [ ] `collect_pattern_bindings` gives `Unknown` to Constructor payload binding — should extract field type from matching NominalVariant when scrutinee is a Union containing a matching tag (`src/typecheck.rs:1468-1472`)
- [ ] Exhaustiveness checking only fires for `Type::Union` scrutinee — bare `NominalVariant` scrutinee (not wrapped in Union) skips exhaustiveness entirely; needs type alias registry lookup to get sibling constructors (`src/typecheck.rs:2315`, `src/coverage.rs`)

### integration-fixes: Cross-layer integration issues

- [ ] **MAJOR** `ast-of` builtin (`builtin_ast_of`) is defined in `src/builtins_meta.rs:790` but NOT registered in `standard_builtins()` — inaccessible after deletion of `%rust "meta"`; 3 corpus tests assert broken behavior with `undefined variable: ast-of`; **ADDRESSED by `runtime-v2-rebase` Phase 8**: registers `builtin_ast_of` returning `Value::Expression` and updates corpus tests (`src/builtins.rs`, `tests/corpus/eval/builtins/ast_of_*.llt-eval`)
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

- [ ] **CRITICAL** `--require-integrity` is completely non-functional after include-decomp-prelude deleted `builtin_include` — `EvalError::include_hash_mismatch`/`include_hash_required` exist but are never called; self-hosted `include` never checks `ctx.config.require_integrity`; add CLI warning "warning: --require-integrity not yet implemented" until re-implemented; long-term: add `hash:` named arg to `builtin_load` (`src/builtins_meta.rs:1685-1687`, `stdlib/prelude.llt:2462-2477`)
- [ ] **CRITICAL** Path traversal in macro pre-scan: `pre_scan_follow_libdir_include` reads files via `std::fs::read_to_string(libdir_path.join(file_name))` with no canonicalization or prefix check — `[include %libdir "../../etc/passwd"]` in a defmacro body reads arbitrary files bypassing cap-std; fix: add `if ctx.config.no_fs { return; }` guard + canonicalize prefix check after join, then replace std::fs with cap_std Dir approach (`src/expand.rs:594-602`)
- [ ] `builtin_slurp` has no file size limit — self-hosted include pipeline reads files via `[slurp [open cap path Readable]]`; no MAX_FILE_SIZE enforcement; add `Read::take(MAX_FILE_SIZE + 1)` + length check to both text and binary branches (`src/builtins_io.rs:439-466`)
- [ ] `check-ambient-dir` CI check exits 0 on violations — `|| true` in justfile:294 makes the check always pass; `src/type_normalize.rs:355` has `open_ambient_dir` with no `// AMBIENT-OK` comment and is not caught; fix: remove `|| true`, add `// AMBIENT-OK` comment to type_normalize.rs:355 (`justfile:294`, `src/type_normalize.rs:355`)
- [ ] Nested includes in `resolve_includes` fall back to `std::fs` + software path check — recursive `resolve_includes` at `src/imports.rs:820-828` passes `None` for `base_cap_dir`, using software `starts_with()` check instead of cap-std kernel-level RESOLVE_BENEATH; fix: open a new `cap_std::fs::Dir` for the nested file's parent and pass it in the recursive call (`src/imports.rs:820-828`)
- [ ] Remove dead `lsp_eval_env` construction block at `src/lsp/document.rs:160-306` — constructs `DirPerms::full()` DirCaps and immediately discards them; misleading dead code; replace with comment explaining LSP eval is intentionally skipped (`src/lsp/document.rs:160-306`)
- [ ] `cap-identity` non-Unix fallback uses `DefaultHasher` (non-stable, randomized per process) — include cache key invalid across restarts; fix: use `blake3::hash(...)` for stable, collision-resistant identity (`src/builtins_meta.rs:1466-1474`)
