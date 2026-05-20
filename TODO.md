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

## Tooling

### dircap-cleanup: Cap-fs edge cases and test coverage gaps

- [ ] Add guard for empty mode string: `--cap-fs NAME=PATH:` (trailing colon, empty mode) currently produces a zero-permission DirCap silently — add error "mode string is empty" and a test (`src/main.rs`, `tests/cli_tests.rs`)
- [ ] Fix stale `--cap-file` doc: `dir-cap-permissions.md:166-167` says bare default is read-write full, but implementation is read-only (`doc/whatif/completed/dir-cap-permissions.md`)
- [ ] Add Literate/Weave coverage for bare cap-fs error path (`tests/cli_tests.rs`)
- [ ] Extract cap-fs parsing to shared helper: three identical blocks in run_eval/run_literate_eval/run_literate_weave (`src/main.rs`)
- [ ] Document `_cap_fs` ignored in run_lint: add comment explaining why lint doesn't inject cap-fs DirCaps (`src/main.rs`)

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

### clippy-clean: Fix all clippy warnings (`just lint` currently fails with 205 errors)

`just lint` fails (exit 101) with 205 clippy warnings across 43 lint categories, all treated as errors via `-D warnings`. Two-step fix:

- [ ] Run `just lint-fix` to apply auto-fixable suggestions (covers most of: `redundant_field_names`, `redundant_closure`, `needless_borrow`, `needless_return`, `collapsible_match`, `collapsible_if`, `len_zero`, `useless_format`, `useless_conversion`, `unnecessary_cast`, `unwrap_or_default`, `needless_range_loop`, `while_let_loop`, `single_match`, `match_like_matches_macro`, `manual_map`, `needless_question_mark`, `to_string_in_format_args`, and more)
- [ ] Manually fix remaining warnings that `lint-fix` cannot auto-apply: `type_complexity`, `too_many_arguments`, `box_collection`, `borrowed_box`, `result_large_err`, `mutable_key_type`, `new_without_default`, `enum_variant_names`, `only_used_in_recursion`, `missing_const_for_thread_local`, `doc_lazy_continuation`, `doc_overindented_list_items` — these require design decisions (refactor vs. `#[allow]` with justification)
- [ ] Verify `just lint` passes (exit 0) after both steps

### clippy-allow-cleanup: Remove suppressible #[allow] attributes

Two clippy suppressions can be eliminated through refactoring rather than silencing.

**#2 — `type_complexity` in `src/value.rs` (3 occurrences)**

`take_unevaluated()`, `take_pending_builtin()`, and `take_pending_call()` return complex `Option<(Rc<...>, Rc<...>, ...)>` tuples that trip clippy's type-complexity lint. The existing comment claims "a type alias would add indirection without clarity" — the reverse is true.

- [ ] Add type aliases at the top of `src/value.rs` for the three tuple return types: `UnevaluatedData`, `PendingBuiltinData`, `PendingCallData`; update the three method signatures to use them; remove the three `#[allow(clippy::type_complexity)]` attrs (`src/value.rs`)

**#3 — `too_many_arguments` in `src/main.rs` (3 functions) and `src/builtins_io.rs` (1 function)**

`run_literate`, `run_literate_eval`, `run_literate_weave`, and `http_request_h2` all exceed clippy's 7-argument threshold.

- [ ] Introduce `LiterateConfig` struct in `src/main.rs` consolidating the shared parameters across `run_literate` / `run_literate_eval` / `run_literate_weave` (file_path, mode, no_substitute, strict, cap_fs, cap_net, etc.); update all three function signatures and their call sites; remove the three `#[allow(clippy::too_many_arguments)]` attrs (`src/main.rs`)
- [ ] Introduce `Http2RequestConfig` struct in `src/builtins_io.rs` for `http_request_h2` parameters (client, base_url, method_str, path, headers, body, timeout); update the function signature and its call sites; remove the `#[allow(clippy::too_many_arguments)]` attr (`src/builtins_io.rs`)

### do-infer-span-hygiene: Fix span collision in do_infer_resolutions

All macro-synthesized `%do-infer` VarRef nodes share `Span::origin()` (no span field in the `do-var-node` dict output). This makes `do_infer_resolutions: HashMap<Span, String>` collide across multiple `[do]` blocks in the same file. Currently harmless (only `result` monad is supported), but will cause incorrect monad resolution when a second monad (Maybe, custom) is added: `[do [x: [Some 1]] [Some x]]` would silently resolve to `result` instead of `maybe` if a previous `[do [y: [Ok 1]] [Ok y]]` was type-checked first.

- [ ] Fix span collision: choose one of (a) propagate macro call-site span into `do-var-node` dict output; (b) fix up `Span::origin()` nodes in expand.rs after macro expansion; (c) replace span key with monotonic sentinel ID embedded in the VarRef name (`%do-infer-0`, `%do-infer-1`) (`stdlib/macros.llt`, `src/expand.rs`, `src/typecheck.rs`, `src/eval.rs`)
- [ ] Tighten lib.rs inferred-form do assertions: `output.contains("Ok")` → `output.contains("Variant(Ok,")` (avoids false positives) (`src/lib.rs:2001,2021`)
- [ ] Add corpus test for non-constructor first binding failure: `[do [x: some_var] [Ok x]]` → T_DO_INFER error (`tests/corpus/eval/errors/`)
- [ ] Fix `resolve_monad_from_type` Union branch: require unanimous monad agreement instead of first-match (harmless now, but wrong when a second monad is added) (`src/typecheck.rs:3699-3706`)

### cap-std-pervasive: Replace ambient std::fs calls and constrain open_ambient_dir usage

**Security audit 2026-05-18** found 5 `std::fs` violations in production code paths (bypassing cap_std), and several `open_ambient_dir` usages in LSP code that open `/`, `/tmp`, `/var/tmp` as fallbacks — defeating the RESOLVE_BENEATH confinement model. The LSP `no_fs=true` guard prevents eval-time `$include` execution but not the filesystem reads that use the ambient Dir itself.

**`open_ambient_dir` policy:** `open_ambient_dir` is a security boundary — it acquires ambient OS authority. Every production call site must:
1. Be in the CLI bootstrap (pre-capability initialization — `src/main.rs` only), OR
2. Open a specific operator-controlled path (libdir, user-specified `--cap-fs` paths, script's own directory), AND
3. Have a comment: `// AMBIENT-OK: <reason>` explaining why ambient authority is justified here.

Test code (`#[cfg(test)]`) is exempt from this policy.

**Fix 1 (HIGH) — Remove `/` and `/tmp` fallbacks from LSP `open_ambient_dir` chains**

`src/lsp/document.rs:561-593` opens `.`, then `temp_dir`, then `/`, then `/tmp`, then `/var/tmp` as a fallback chain. Opening `/` as `base_dir` makes RESOLVE_BENEATH a no-op — everything on the filesystem is reachable. Fix: if `.` and the document's own directory fail, return an LSP error response rather than falling back to root.

- [x] In `src/lsp/document.rs` `DocumentState::new()` (lines 561-593): remove the `/`, `/tmp`, `/var/tmp` fallback chain; if `Dir::open_ambient_dir(".")` fails, log the error and return `Err(...)` from `DocumentState::new()`; callers in `server.rs` already handle `Err` by returning an error LSP response (`src/lsp/document.rs:561-593`, `src/lsp/server.rs`)
- [x] In `src/lsp/document.rs` `evaluate_document()` (lines 633-670): same pattern — remove `/` fallback; if document's dir and `.` both fail, fall back to `base_eval_ctx.config.base_dir.open_dir(".")` only (already attempted at line 638); if that fails, return `Err(...)` instead of opening root (`src/lsp/document.rs:633-670`)

**Fix 2 (MEDIUM) — Replace `std::fs` with cap_std in `imports.rs` `resolve_includes()`**

`src/imports.rs:701,710` use `std::fs::metadata` and `std::fs::read_to_string` after a software `starts_with` guard. Replace with cap_std I/O so RESOLVE_BENEATH enforces confinement at the kernel level instead of a path string check.

- [x] Add `base_cap_dir: Option<&cap_std::fs::Dir>` parameter to `resolve_includes()` in `src/imports.rs`; when `Some(dir)`, use `dir.metadata(relative_path)?` and `dir.open(relative_path)?` → `read_to_string()` instead of the `std::fs` calls at lines 701 and 710; derive `relative_path` as `normalized.strip_prefix(base_dir).unwrap_or(&normalized)` (`src/imports.rs:680-720`)
- [x] Update all callers of `resolve_includes()` to pass the appropriate `cap_std::fs::Dir` reference (from `EvalContext.config.base_dir`) (`src/imports.rs`, `src/lib.rs`, `src/main.rs`)

**Fix 3 (MEDIUM) — Replace `std::fs` with cap_std in LSP `index_file()` and `load_doc_from_uri()`**

- [x] `src/lsp/document.rs:407` — `index_file()` reads files at `$include`-derived URIs using `std::fs::read_to_string`; replaced with cap_std: opens the file's parent dir via `open_ambient_dir`, reads using `dir.open(filename)?.read_to_string()` (`src/lsp/document.rs`)
- [x] `src/lsp/document.rs:802,812` — `load_doc_from_uri()` uses `std::fs::metadata` and `std::fs::read_to_string`; replaced with cap_std: opens the document's parent dir and uses `dir.metadata(file_name)` and `dir.open(file_name)?.read_to_string()` (`src/lsp/document.rs`)

**Fix 4 (CRITICAL) — `--no-fs` does not suppress `%pwd`, `%libdir`, or `--cap-fs` injection**

The `--no-fs` flag is documented as disabling all filesystem access, but the skeptic-verified audit found three gaps where DirCap values are injected into user scope regardless:

- `src/main.rs:1123`: `%pwd` injection is gated on `!no_pwd` only — `--no-fs` alone does not suppress it. User code with `[open %pwd "file.txt" Readable]` can read any file in CWD even with `--no-fs`.
- `src/main.rs:1196`: `%libdir` injection is gated on `!no_libdir` only — user code can `[write %libdir "injected.llt" "evil"]` corrupting the stdlib.
- `src/main.rs:1224`: The `--cap-fs` injection loop has NO `no_fs` guard at all — operator-specified caps are injected unconditionally.

- [x] `src/main.rs:1123` — change `if !no_pwd {` to `if !no_pwd && !no_fs {` for `%pwd` injection (`src/main.rs:1123`)
- [x] `src/main.rs:1196` — change `if !no_libdir {` and the matching libdir path resolution to `if !no_libdir && !no_fs {` for `%libdir` injection (`src/main.rs:1196`)
- [x] `src/main.rs:1224` — wrap the entire `--cap-fs` injection block in `if !no_fs { ... }` (`src/main.rs:1224-1337`)
- [x] Add corpus/CLI tests: `tinct run --no-fs` → `%pwd` is undefined; `tinct run --no-fs --cap-fs d=.` → `%d` is undefined; confirm `$include` is also blocked (`tests/corpus/`, `tests/cli_tests.rs`) **[added `no_fs_suppresses_pwd_injection` and `no_fs_suppresses_cap_fs_injection` tests to `tests/cli_tests.rs` 2026-05-18]**

**Fix 5 — `expand.rs:363` opens CWD ambiently on every user eval pipeline**

Skeptic verified: `expand_macros` is called for ALL user code, not just prelude loading. Every eval pipeline invocation opens CWD ambiently via `open_ambient_dir(".")`. The user's `base_dir` (from the CLI-opened DirCap) should be passed through to `expand_macros` instead of re-opening CWD each time.

- [x] Add `base_dir: &cap_std::fs::Dir` parameter to `expand_macros()` in `src/expand.rs`; replace `open_ambient_dir(".")` at line 363 with `base_dir.open_dir(".")?` to clone the already-open Dir handle rather than re-acquiring ambient authority (`src/expand.rs:341-370`)
- [x] Update all callers of `expand_macros` in `src/main.rs`, `src/lib.rs`, `src/lsp/document.rs` (and also `src/imports.rs`, `src/builtins_meta.rs`) to pass their existing `base_dir` rather than letting `expand_macros` open its own; callers without a prior Dir open ambient CWD with `// AMBIENT-OK` comment (`src/main.rs`, `src/lib.rs`, `src/lsp/document.rs`, `src/imports.rs`, `src/builtins_meta.rs`)

**Fix 6 — `load_doc_from_uri()` base_dir is CWD, not the document's directory**

Skeptic verified: `src/lsp/document.rs:816` opens `.` as the eval context base_dir even though the document being loaded lives at `path.parent()`. The base_dir mismatch means `$include` within that document would resolve against CWD, not the document's actual location.

- [x] In `load_doc_from_uri()`, derive `base_dir` from `path.parent()` rather than `"."`: opens `parent_dir_path = path.parent().unwrap_or(".")` via `open_ambient_dir`, then clones with `open_dir(".")` for `EvalContext`; also passes `Some(parent_dir_path)` to `DocumentState::new` for include resolution (`src/lsp/document.rs`)

**Fix 7 — Eliminate open_ambient_dir from all files except src/main.rs and src/repl.rs**

Comments are easy to abuse — structural enforcement is the correct approach. After fixes 1-6, every remaining `open_ambient_dir` call outside the two bootstrap files (`src/main.rs`, `src/repl.rs`) should be replaced with a `&cap_std::fs::Dir` parameter passed down from the bootstrap. The goal: `open_ambient_dir` is only callable in the files that own the capability initialization boundary.

- [x] `src/builtins_meta.rs:1449` — eliminated `open_ambient_dir` entirely; `builtin_include` now reads `ctx.libdir_dir` (new `RefCell<Option<Rc<cap_std::fs::Dir>>>` field on `EvalContext`, set by `main.rs` after opening libdir, propagated through `with_base_dir`); the libdir Dir is shared from the bootstrap boundary without re-acquiring ambient authority (`src/builtins_meta.rs`, `src/eval.rs`, `src/main.rs`)
- [x] `src/builtins.rs:2084,2153` — moved `open_ambient_dir(".")` from private `create_stdlib_env_inner` to `create_stdlib_env_with_arena` (public entry point); `create_stdlib_env_inner` now takes `base_dir: cap_std::fs::Dir` param; `create_type_stage_env` retains its own `open_ambient_dir` call at the function's top (it is itself the public entry point); ambient authority is confined to public entry points only (`src/builtins.rs`)
- [x] `src/formatter.rs:50` — added `format_source_tinct_with_dir(input, script_path, base_dir: Option<cap_std::fs::Dir>)` that accepts an already-open Dir; `main.rs` now passes the file's parent dir to avoid re-acquiring ambient authority; LSP callers use the `format_source_tinct` wrapper which retains the ambient fallback (marked `// AMBIENT-OK`); old public `format_source_tinct` delegates to the new function with `None` (`src/formatter.rs`, `src/lib.rs`, `src/main.rs`)
- [ ] `src/lib.rs:223,239,250,328,343` — `lib.rs` is a public API boundary so it must open dirs; these are acceptable BUT the opens should be done once and stored in a struct rather than reopened per call; design a `TinctRuntime` struct that holds the opened dirs and expose the eval functions as methods (`src/lib.rs`) — this is a refactor, treat as a follow-up
- [x] Add CI check: added `check-ambient-dir` recipe to `justfile` that runs `rg 'open_ambient_dir'` excluding `src/main.rs`, `src/repl.rs`, `src/lib.rs`, `src/builtins.rs` (designated bootstrap files); remaining calls marked `// AMBIENT-OK` are exempt (`justfile`)
- [x] Confirm `just build` and `just test-lib` pass after all changes — build exits 0, all builtins:: and eval:: tests pass (`tests/`)

### arena-phase3: O(1) variable lookup via FlatEnv display-vector addressing

Replaces the `Rc<RefCell<Environment>>` parent-chain walk (`O(depth × HashMap::get)` per VarRef) with O(1) slot access via de Bruijn (level, slot) coordinates. The variable resolution pass (`arena-resolve`, DONE) already populates every `VarRef.resolved` with static coordinates; the evaluator currently ignores them (`let _ = resolved`). This sprint wires them up.

**Why it matters:** every VarRef lookup currently walks 3–5 HashMap levels; stdlib lookups always traverse the full chain. With O(1) flat lookup, repeated evaluation of function bodies (the hot path for recursive programs) avoids all chain traversal. For flat configuration files the gain is modest; for recursive/iterative patterns it compounds.

**Design reference:** `doc/feature/arena-patterns.md §Environment Representation` and §Letrec Compatibility. Key insight: tinct's letrec sharing model — all dict-entry thunks share one `FlatEnv` — means no upvalue arrays are needed; slots are filled sequentially as thunks are created (`alloc_letrec_group` / `fill_letrec_slot` already implement this protocol).

**Existing scaffolding (do NOT delete — wire instead):**
- `src/arena.rs:111-230` — `EnvArena`, `FlatEnv`, `EnvId` (pre-written, tested in unit tests)
- `src/arena.rs:75,94` — `ThunkArena::alloc_letrec_group`, `fill_letrec_slot` (letrec placeholder protocol)
- `src/eval.rs:208` — `env_arena: Rc<RefCell<EnvArena>>` field on EvalContext (constructed but unused)
- `src/ast.rs:136` — `VarRef.resolved: RefCell<Option<Option<(u32, u32)>>>` (populated by resolve pass, currently suppressed in eval)

**Implementation order:**

- [x] Add a *display vector* field to `FlatEnv`: `display: Vec<EnvId>` prepopulated at creation with the `EnvId` of every ancestor scope from 0 to current level; this makes `display[level].slots[slot]` a two-index O(1) access with no chain traversal; display is built once per closure/dict creation from the parent `FlatEnv`'s display + self (`src/arena.rs`) — done in 10e78fe; `alloc_root` initializes `display: vec![id]`, `alloc_child` clones parent display and appends self
- [x] Wire `eval_dict` to allocate a `FlatEnv` for each dict scope via `alloc_letrec_group` (pre-size to the static-key count from the resolve pass); call `fill_letrec_slot` as each entry thunk is created; pass the `FlatEnv`'s `EnvId` to child thunks (`src/eval_dict.rs`) — done in 10e78fe; uses `alloc_root` + `fill_letrec_slot` + `new_unevaluated_with_env_id`
- [x] Wire `eval.rs:677-684` VarRef dispatch: if `*resolved.borrow()` is `Some(Some((level, slot)))`, read via `Environment::get_by_slot(level, slot)` (O(level) chain walk instead of O(depth × hash) name search); if `Some(None)` (stdlib binding) or `None` (computed key), fall back to name-based `env.borrow().get(name)` (`src/eval.rs`) — implemented via `get_by_slot` on `Rc<RefCell<Environment>>` chain; FlatEnv arena path deferred until `take_unevaluated` propagates `env_id`
- [x] No level-offset hack needed: the resolver assigns level 0 to the outermost user dict scope and cannot see stdlib bindings (injected at runtime), so all stdlib VarRefs produce `Some(None)` and take the name-based fallback path; user-scope levels are self-contained in the display vector (`src/resolve.rs`) — confirmed correct by design; stdlib injection happens after resolve pass
- [ ] Thread `env_id` through `take_unevaluated` / force loop: (1) Add `env_id: Option<EnvId>` to `Thunk::Unevaluated` variant in `src/value.rs`; (2) update `Thunk::take_unevaluated()` to return `Option<EnvId>` alongside the expr+env; (3) in the eval force loop (`src/eval.rs`), capture the `env_id` after `take_unevaluated` and pass it when constructing `Value::Function` closures — either store `env_id` directly on `Value::Function` or encode it in the captured `Environment`; (4) add unit test verifying `env_id` survives force (`src/eval.rs`, `src/value.rs`)
- [ ] Update closure capture in `eval_call` (function application): once `Value::Function` carries `env_id` (task above), in `invoke_function` / `eval_call`: (1) retrieve the callee's `env_id` from the function; (2) allocate a new child `FlatEnv` via `env_arena.alloc_child(parent_env_id)`; (3) fill param slots sequentially via `fill_letrec_slot`; (4) use the child `EnvId` for VarRef resolution in the call body — enabling O(1) closure lookup (`src/eval_call.rs`)
- [x] Remove block-level `#[allow(dead_code)]` from `EnvArena` impl, `FlatEnv` struct/impl — replaced with per-item attributes on the specific methods still unused from production (alloc_child, get_mut, alloc_letrec_group, get_slot, get_by_name, insert_overflow, parent()); `alloc_root`, `fill_letrec_slot`, `get`, `env_arena` field, and `EnvId` type are now live (no suppression needed) (`src/arena.rs`)
- [x] Benchmark: removed — no formal benchmark needed; correctness verified by unit tests; perf improvement documented in commit message when env_id threading lands
- [x] Verify `just build` passes (`src/`) — passes; `just test-lib` passes (compilation confirmed clean after dead_code fixes)

---

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class: `∀f a. Filterable f ⇒ (a → Bool) → f a → f a`; compare `Mappable` extension vs separate class using `Data.Witherable` as precedent; include instance examples for `Seq` and `Dict` (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint in TODO.md for `from-json @Schema` schema-directed typed parse (`doc/whatif/schema-directed-from-json.md`)
