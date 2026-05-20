# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Macro System v2

`macros-v2` accepted 2026-05-17. See `doc/whatif/macros-v2.md`. Unified `macro` form with `[let ...]` patterns, `inject:` for anaphoric binding, `splice` for multi-form output, `syntax-class` for declarative argument validation. Implementation order: macros-v2-ast → macros-v2-expand → macros-v2-inject → macros-v2-stdlib.

- [ ] Cleanup nits (tack onto next macro sprint): stale doc comment in `register_stdlib_macros_from_env`; `"Let"` vs `"LetDecl"` inconsistency in `validate_syntax_class` allowlist; anonymous-Rest edge case in `fn-binding-to-param`; add corpus test for syntax.llt let-softening path; remove syntax.llt no-op stub once let-softening is removed (`src/expand.rs`, `stdlib/syntax.llt`)
- [ ] `builtin_ast_of` Materialized branch returns `Value::Dict` (with `type:` field) while Unevaluated branch returns `Value::Variant` — inconsistent return type; `[tag-of [ast-of already-forced-val]]` returns empty string not a tag; fix: unify both branches to return Variant (`src/builtins_meta.rs:629-640`)
- [ ] `gensym` returns String but macros-v2 spec says it should return VarRef AST node — spec divergence; decide whether to change gensym or update the spec (`doc/whatif/macros-v2.md:903`, `src/builtins_meta.rs`)
- [ ] `ScopeId::fresh()` allocated but unused (`_scope_id`) in expand.rs — placeholder for future scope-set hygiene; remove or implement (`src/expand.rs:1787`)
- [ ] 6 `do` corpus tests have stale expected output format: `do_minimal.llt-eval` and `do_hardcoded.llt-eval` expect `[Ok 42]` but runtime produces `Variant(Ok, Int(42))`; 4 other do tests (`do_three_step`, `do_nonbinding_step`, `do_no_steps`, `do_err_propagation`) have no `=== out` section at all (`tests/corpus/eval/macros/`)

---

## Primitive Privacy

### include-decomp-eval-primitives: Implement expand/eval/eval-types and delete builtin_include

**Whatif:** `include-decomposition`
**Spec chapters:** `doc/whatif/include-decomposition.md §What Would Change`
**Depends on:** `include-decomp-primitives`

- [ ] Add `dict_to_file(val: &Value, ctx: &Rc<EvalContext>) -> Result<File, AstError>` to `src/ast_dict.rs` — internal, not a registered builtin; file-level inverse of `ast_to_dict`; reconstructs `File` from schema produced by `document_to_dict` (documents → expressions via `dict_to_ast`, name, stage nominal variant, output-type, expects; caps always `None`)
- [ ] Add `type_stage_env: Rc<RefCell<Environment>>` to `EvalConfig` (`src/eval.rs`); build it once at startup using `build_type_stage_env()` from the typechecker; pass into `EvalConfig::new(...)`
- [ ] Extract `eval_expressions(exprs: &[Spanned<Expr>], env: Rc<RefCell<Environment>>, ctx: &Rc<EvalContext>) -> EvalResult<Rc<Thunk>>` helper from `eval_document` — the sequential let\* loop; reused by the `eval` builtin and the remaining bootstrap prelude-load path
- [ ] Implement `expand` builtin: `dict_to_file` → `crate::expand::expand()` → `ast_to_dict`; schema errors from `dict_to_file` surface as user errors (`src/builtins_meta.rs`)
- [ ] Implement `eval` builtin: deserialize `exprs` (positional Dict) via `dict_to_ast`, build env chain (`stdlib_env` + `env:` entries + `"$"` = `%:` thunk), call `eval_expressions`; `caps:` validation skipped (`src/builtins_meta.rs`)
- [ ] Implement `eval-types` builtin: same as `eval` but uses `ctx.config.type_stage_env` as base env; no `%:` or `env:` parameters (`src/builtins_meta.rs`)
- [ ] Register `expand`, `eval`, `eval-types` in `standard_builtins()` (`src/builtins.rs`)
- [ ] Delete `builtin_include` entirely (`src/builtins_meta.rs`) — all 350+ lines
- [ ] Delete `EvalState::include_guard: HashSet<(u64, u64)>` and old `EvalState::include_cache` (`src/eval.rs`)
- [ ] Delete `Value::RustRegistry`, `rust_module()`, all module grouping logic (`src/value.rs`, `src/builtins.rs`)
- [ ] Delete `builtin-*` aliases from module group setup (`src/builtins.rs`)
- [ ] Delete `eval_file_with_input`, `eval_document`, `run_eval` from `src/eval_pipeline.rs`; delete file entirely once empty
- [ ] Tests: unit tests for `dict_to_file` round-trip (`load` output → `dict_to_file` → compare field structure); corpus tests for `expand` and `eval` builtins (`tests/corpus/eval/builtins/`)
- [ ] Verify `just test-lib` passes

### include-decomp-prelude: Add pipeline functions to prelude.llt

**Whatif:** `include-decomposition`
**Spec chapters:** `doc/whatif/include-decomposition.md §Tinct Implementation`
**Depends on:** `include-decomp-eval-primitives`

- [ ] Change `%rust` from `Value::RustRegistry` to a flat `Value::Dict` of all Rust primitives; seed at startup (`src/builtins.rs`, `src/value.rs`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Rewrite prelude.llt opening: replace `[include %rust "core"]` etc. with single `%rust` expression that scope-promotes all primitives (`stdlib/prelude.llt`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Add `eval-document-runtime` to prelude public dict (`stdlib/prelude.llt`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Add `eval-document-pipeline` to prelude public dict (`stdlib/prelude.llt`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Add `eval-file` to prelude public dict (`stdlib/prelude.llt`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Add `include-cache-success`, `include-cache-failure`, `include-evaluate-and-cache` to prelude private dict (`stdlib/prelude.llt`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Add `include` to prelude public dict (replaces `builtin_include` as the user-facing include function) (`stdlib/prelude.llt`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Add `cli-pipeline` to prelude public dict (`stdlib/prelude.llt`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Verify `IncludeCacheEntry: [type [Missing] [Pending] [Cached Any]]` is in prelude type namespace (`stdlib/prelude.llt`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Update `src/main.rs` to call tinct `cli-pipeline` function directly after prelude loads; construct `files_thunk` as positional Dict from `Vec<String>` file paths; pass `%pwd` DirCap as third argument (`src/main.rs`) — blocked: requires include-decomp-prelude to be complete first
- [ ] Update formatter and docgen to use `load` directly instead of internal `ast_to_dict` (`stdlib/formatter/`, `scripts/docgen.llt`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Tests: corpus test verifying `[include %include-dir "sibling.llt"]` works within a multi-file include chain (`tests/corpus/eval/`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Tests: corpus test for circular include detection via `[Pending]` cache state (`tests/corpus/eval/errors/`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Tests: corpus test for `cli-pipeline` threading `%` across files (`tests/corpus/eval/`) — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement
- [ ] Verify `just test` passes and `just docgen` runs successfully — **SKIPPED**: blocked: requires deleting builtin_include which needs include pipeline replacement

### include-decomposition-review: Post-implementation review

**Whatif:** `include-decomposition`
**Depends on:** `include-decomp-prelude`

- [ ] Run `/review-whatif include-decomposition` — verify all sprints complete, implementation matches spec, `doc/08-evaluation.md` and `doc/09-documents.md` updated to describe self-hosted pipeline in present tense, no stubs or de-scoped features — **SKIPPED**: blocked: requires include-decomp-prelude complete

---

## Codebase Health

### strings-char-access: Add str-at, str-slice, str-length to strings.llt

Character-level string access needed to implement `from-json` in tinct (recursive descent JSON parser). Currently `from-json` is a Rust primitive; once these are available it moves to `stdlib/codecs/json.llt`.

- [ ] `str-at@[Fn [n@Int s@String] String]` — character at position `n` (single-char string); negative indices count from end
- [ ] `str-slice@[Fn [start@Int len@Int s@String] String]` — substring starting at `start` of length `len`
- [ ] `str-length@[Fn [s@String] Int]` — length in characters (codepoint count, not bytes; tinct strings are UTF-8)
- [ ] Add to `strings.llt` alongside existing `str-find`, `str-split`, etc.
- [ ] Audit all of `strings.llt`: Rust built-ins (`%rust.*`) are only accessible to prelude. Any function in `strings.llt` that currently calls Rust built-ins directly must be rewritten using prelude wrappers. `strings.llt` is a stdlib module that includes prelude — it must use only prelude-exported functions and its own helpers.
- [ ] Corpus tests covering empty string, out-of-bounds, multi-byte codepoints
- [ ] Once available: migrate `from-json` from Rust primitive to tinct in `stdlib/codecs/json.llt`
- [ ] Decide names for character-position string parsing: `parse-int`/`parse-float` (new names, used in `codecs/json.llt` deserializer) vs `to-int`/`to-float` (existing builtins). Either add `parse-int`/`parse-float` as aliases or rename all calls in `codecs/json.llt` to use `to-int`/`to-float`.
- [ ] DESIGN: `str-slice` planned as `(start, len, string)` but existing `builtin-str-slice` is `(string, start, end)` — adding to `strings.llt` would shadow/conflict. Adding the new `str-slice` to `strings.llt` constitutes a breaking change for any file that includes `strings.llt` and calls `[str-slice s start end]`. Decide on naming before implementing: options are (a) use distinct name `str-substr` in `strings.llt`, (b) rename Rust builtin to `str-slice-range` and update all call sites, (c) accept shadowing (only affects files that include `strings.llt`).

### exhaustiveness-multi-field-nominal: Fix exhaustiveness checking for multi-field nominal variant payloads

`coverage.rs:ConstructorSignature::from_union` (line 182–190) builds constructor tags from the field key set of each union member rather than from the declared variant name. For single-field variants (`[Ok T]`, `[Some a]`) this is unambiguous — the single key is the tag. For multi-field named-payload variants (`[IntLiteral value: Int span: AstSpan]`) the combined-key path produces `"span,value"` rather than `"IntLiteral"`, which does not match the `Pattern::Constructor` tag produced by the parser. Exhaustiveness checking silently fails to fire for multi-field nominal variants.

Currently latent — no existing nominal type uses multi-field named payloads. Will be triggered when `AstExpr` is declared (runtime-v2) or when any user-defined multi-field nominal variant is matched.

The fix noted at `coverage.rs:205` ("Future: Type::NominalVariant — not yet in Type enum"): union members need to carry their declared constructor name, not just their field set.

- [ ] Add `Type::NominalVariant { tag: String, fields: Row }` (or equivalent discriminant) to `src/type_def.rs` so union members produced by `[type [Tag field: T ...] ...]` declarations carry the variant name
- [ ] Update `coverage.rs:ConstructorSignature::from_union` to use the variant name as the constructor tag for nominal variants, falling back to the current field-key path for structural ADTs
- [ ] Add corpus tests: a multi-field nominal variant with exhaustive match (should pass exhaustiveness), and a non-exhaustive match (should warn)

---

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class: `∀f a. Filterable f ⇒ (a → Bool) → f a → f a`; compare `Mappable` extension vs separate class using `Data.Witherable` as precedent; include instance examples for `Seq` and `Dict` (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint in TODO.md for `from-json @Schema` schema-directed typed parse (`doc/whatif/schema-directed-from-json.md`)
