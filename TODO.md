# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Macro System v2

`macros-v2` accepted 2026-05-17. See `doc/whatif/macros-v2.md`. Unified `macro` form with `[let ...]` patterns, `inject:` for anaphoric binding, `splice` for multi-form output, `syntax-class` for declarative argument validation. Implementation order: macros-v2-ast → macros-v2-expand → macros-v2-inject → macros-v2-stdlib.

---

## Primitive Privacy

### include-decomp-prelude: Add pipeline functions to prelude.llt ✓ DONE 2026-05-19

**Whatif:** `include-decomposition`
**Spec chapters:** `doc/whatif/include-decomposition.md §Tinct Implementation`
**Depends on:** `include-decomp-eval-primitives`

- [x] Delete `builtin_include` entirely (`src/builtins_meta.rs`) — done in prior sprint; tombstone comment at builtins_meta.rs:1681
- [x] Delete `EvalState::include_guard: HashSet<(u64, u64)>` and old `EvalState::include_cache` (`src/eval.rs`) — done in prior sprint
- [x] Delete `Value::RustRegistry`, `rust_module()`, all module grouping logic (`src/value.rs`, `src/builtins.rs`) — done in prior sprint; tombstone comment at builtins.rs:1607
- [x] Delete `builtin-*` aliases from module group setup (`src/builtins.rs`) — aliases now injected directly in `create_root_env()`; no module grouping remains
- [x] Add `eval-document-runtime` to prelude public dict (`stdlib/prelude.llt`)
- [x] Add `eval-document-pipeline` to prelude public dict (`stdlib/prelude.llt`)
- [x] Add `eval-file` to prelude public dict (`stdlib/prelude.llt`)
- [x] Add `include-cache-success`, `include-cache-failure`, `include-evaluate-and-cache` to prelude public dict (`stdlib/prelude.llt`)
- [x] Add `include` to prelude public dict (`stdlib/prelude.llt`)
- [x] Add `cli-pipeline` to prelude public dict (`stdlib/prelude.llt`)
- [x] Verify `IncludeCacheEntry: [type [Missing] [Pending] [Cached Any]]` in prelude type namespace — added to type-stage section
- [x] Fix `builtin-variant: builtin-variant` self-referential letrec entry (pre-existing bug from working tree) — removed; macros.llt uses parent env chain instead
- [x] Fix `exhaustiveness_multi_field_nominal.llt-eval` broken syntax — moved to `type_errors/` directory with correct format

**Deferred (requires separate sprint or main.rs changes):**
- [ ] Delete `eval_file_with_input`, `eval_document` from `src/eval_pipeline.rs`; delete file entirely — blocked: still used by main.rs, repl.rs, formatter.rs (requires main.rs rewrite to use `cli-pipeline`)
- [ ] Update `src/main.rs` to call tinct `cli-pipeline` function directly — blocked: requires full main.rs rewrite; eval_file_with_input still needed
- [ ] Update formatter and docgen to use `load` directly instead of internal `ast_to_dict`
- [ ] Tests: corpus test for `[include %include-dir "sibling.llt"]` within multi-file include chain
- [ ] Tests: corpus test for circular include detection via `[Pending]` cache state
- [ ] Tests: corpus test for `cli-pipeline` threading `%` across files
- [ ] Verify `just test` passes (known pre-existing `just test-lib` failure from working tree `test_do_infer_corpus_diagnostics` — see below)

### Pre-existing: `just test-lib` fails with exit 101

`cargo test --lib -D warnings` fails in the working tree (pre-existing, not from include-decomp-prelude sprint). The failing test is in `src/typecheck.rs::test_do_infer_corpus_diagnostics` or a nearby unit test. The failure occurs both before and after the include-decomp-prelude changes. Investigation needed to identify the exact test. `just test-one <filter>` and all corpus tests pass.

- [ ] Identify which unit test fails in `just test-lib` and fix it (`src/typecheck.rs` or related module)

### include-decomposition-review: Post-implementation review

**Whatif:** `include-decomposition`
**Depends on:** `include-decomp-prelude`

- [ ] Run `/review-whatif include-decomposition` — verify all sprints complete, implementation matches spec, `doc/08-evaluation.md` and `doc/09-documents.md` updated to describe self-hosted pipeline in present tense, no stubs or de-scoped features

---

## Known Bugs (Type Checker)

- [x] `typecheck::tests::test_dot_access_intersection_found` — Intersection type unification bug: fixed by adding `(Type::Record(..), Type::Intersection(..))` arm to `src/type_unify.rs` that distributes unification across intersection members. Tests pass.
- [x] `typecheck::tests::test_dot_access_intersection_missing_field_returns_unknown` — same Intersection unification bug; fixed by same arm. Tests pass.

---

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class: `∀f a. Filterable f ⇒ (a → Bool) → f a → f a`; compare `Mappable` extension vs separate class using `Data.Witherable` as precedent; include instance examples for `Seq` and `Dict` (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint in TODO.md for `from-json @Schema` schema-directed typed parse (`doc/whatif/schema-directed-from-json.md`)
