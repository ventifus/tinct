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

### include-decomposition-review: Post-implementation review

**Whatif:** `include-decomposition`
**Depends on:** `include-decomp-prelude`

- [ ] Run `/review-whatif include-decomposition` — verify all sprints complete, implementation matches spec, `doc/08-evaluation.md` and `doc/09-documents.md` updated to describe self-hosted pipeline in present tense, no stubs or de-scoped features

---

## Known Bugs

- [ ] `just test-lib` fails with exit 101 (pre-existing). **Investigation (2026-05-19):** 4 failing tests identified: `test_syntax_llt_fn_{no_break,macro_triggered,single_param,already_let_decl}`. All test `[include %libdir "syntax.llt"]`. Root cause: the self-hosted `include` pipeline in `stdlib/prelude.llt` uses `Readable` as a VarRef (`[open cap path Readable]`) but `Readable` is not defined as a runtime variant in the prelude — it exists only as a concept in the `open` Rust builtin. The fix requires adding `OpenFlag: [type [Readable] [Writable] ...]` + re-exports to prelude.llt (and similarly `IncludeCacheEntry`, `Missing`/`Pending`/`Cached`, and `error: raise`). However, these prelude additions cause OOM/SIGKILL in `just test-corpus` due to increased stdlib size per load. The deeper issue is that `include` calls `builtin_expand` which triggers a second full stdlib reload (via `create_stdlib_env_with_arena()` at `EXPAND_MACROS_DEPTH==0`), and the interaction between the first and second stdlib loads creates a context mismatch that needs architectural attention. NOT a stack overflow — fails even with `RUST_MIN_STACK=67108864`. Needs a sprint to: (1) define the missing prelude variants, (2) fix the double-stdlib-load in the include pipeline, or (3) restructure `builtin_expand` to avoid the second stdlib load.

### Known Bugs (Type Checker)

- [x] `typecheck::tests::test_dot_access_intersection_found` — Intersection type unification bug: fixed by adding `(Type::Record(..), Type::Intersection(..))` arm to `src/type_unify.rs` that distributes unification across intersection members. Tests pass.
- [x] `typecheck::tests::test_dot_access_intersection_missing_field_returns_unknown` — same Intersection unification bug; fixed by same arm. Tests pass.

---

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class: `∀f a. Filterable f ⇒ (a → Bool) → f a → f a`; compare `Mappable` extension vs separate class using `Data.Witherable` as precedent; include instance examples for `Seq` and `Dict` (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint in TODO.md for `from-json @Schema` schema-directed typed parse (`doc/whatif/schema-directed-from-json.md`)
