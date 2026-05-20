# Implementation Roadmap

See DONE.md for the full history of completed sprints.

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

- [ ] `just test-lib` fails with exit 101. **Investigation (2026-05-19):** 4 failing tests identified: `test_syntax_llt_fn_{no_break,macro_triggered,single_param,already_let_decl}`. All test `[include %libdir "syntax.llt"]`. Root cause: the self-hosted `include` pipeline in `stdlib/prelude.llt` uses `Readable` as a VarRef (`[open cap path Readable]`) but `Readable` is not defined as a runtime variant in the prelude — it exists only as a concept in the `open` Rust builtin. The deeper issue is that `include` calls `builtin_expand` which triggers a **second full stdlib reload** (via `create_stdlib_env_with_arena()` at `EXPAND_MACROS_DEPTH==0`), creating a context mismatch between the first and second stdlib loads. NOT a stack overflow — fails even with `RUST_MIN_STACK=67108864`. **Resolution:** Will be addressed in the `runtime-v2` rebase — that branch redesigns how Rust builtins and the stdlib bootstrap interact, making `Readable` and similar variants available cleanly and eliminating the double-stdlib-load pattern.

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
- [ ] `builtin_load` slot indices latent bug: `resolve_file` populates VarRef slots for the load-time env, but `builtin_eval` re-uses the AST in a new env where slots may differ — strip resolved slots from `builtin_load` output OR document that `builtin_eval` must re-resolve before arena-phase3 activates (`src/builtins_meta.rs:1548`, `src/ast_dict.rs`)

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

- [ ] **MAJOR** `ast-of` builtin (`builtin_ast_of`) is defined in `src/builtins_meta.rs:790` but NOT registered in `standard_builtins()` — inaccessible after deletion of `%rust "meta"`; 3 corpus tests assert broken behavior with `undefined variable: ast-of`; decide: either add `builtin!("ast-of", builtin_ast_of)` to `standard_builtins()` and rewrite corpus tests, OR mark `ast-of` as removed in README.md/STATUS.md and delete the tests (`src/builtins.rs`, `tests/corpus/eval/builtins/ast_of_*.llt-eval`)
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
