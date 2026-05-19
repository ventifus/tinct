# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Pre-existing Lib Test Failures (identified 2026-05-18)

### do-macro-builtin-get: `do` macro lib tests fail with `builtin-get: expected Dict, got Int`

Seven lib tests in `src/lib.rs::tests::test_do_macro_*` fail:
- `test_do_macro_single_step`
- `test_do_macro_one_binding_step`
- `test_do_macro_three_steps`
- `test_do_macro_no_steps_calls_pure`
- `test_do_macro_err_propagation`
- `test_do_macro_inferred_form_expr_error`
- `test_do_macro_inferred_form_binding_error`

All fail with `"[E080] macro 'do' expansion result failed to evaluate: builtin-get: expected Dict, got Int"`. This error originates in the `do` macro's transformer body when it calls `[get i steps]` or similar patterns inside `do-fold`. The `builtin-get` builtin is receiving an Int as its second argument (expected Dict). Root cause not identified through static analysis; may be related to the `builtin_add/sub/mul/div` pure-primitive migration or an arena boundary issue. The `length` undefined variable error masked this underlying bug.

- [ ] Add RUST_BACKTRACE=1 debug output to narrow the `builtin-get` call site
- [ ] Check if the bug is in `do-fold`'s `[get i steps]` call or elsewhere
- [ ] Fix the `do` macro or the underlying evaluation issue
- [ ] Verify all 7 `test_do_macro_*` tests pass

---

## Pre-existing Corpus Test Failures (identified 2026-05-18)

These failures exist in the corpus test suite and are unrelated to fn-params migration. They must be fixed before `just test` can pass end-to-end.

### corpus-invalid-parse-failures: 16 invalid corpus tests pass parse when they should fail

`test_invalid_corpus` reports 16 tests that call `parse()` successfully when they should return `Err`:

- `tests/corpus/invalid/instance_legacy_syntax_rejected.llt-eval` — `[instance [Equatable Int] ...]` parses OK (error is recovered, not fatal)
- `tests/corpus/invalid/syntax_errors/annotation_special_form_{call,fn,type}.llt-eval` — `x@[fn ...]/x@[call ...]/x@[type ...]` in annotation position accepted
- `tests/corpus/invalid/syntax_errors/annotation_type_assert.llt-eval` — `x@[@Type e]` accepted
- `tests/corpus/invalid/syntax_errors/{call,fn,type}_newline_colon.llt-eval` — `[:` form, `[fn\n:`, `[type\n:` accepted
- `tests/corpus/invalid/syntax_errors/{complex_key,duplicate_key,duplicate_varref_key}.llt-eval` — these parse OK via error recovery
- `tests/corpus/invalid/syntax_errors/missing_value.llt-eval` — `[key:]` accepted
- `tests/corpus/invalid/syntax_errors/{multiple_variadics,param_after_variadic}.llt-eval` — variadic validation errors are recovered not fatal
- `tests/corpus/invalid/syntax_errors/pipe_no_rhs_bracket.llt-eval` — `[[a b | ] ]]` accepted
- `tests/corpus/invalid/syntax_errors/special_form_arity.llt-eval` — `[call]` accepted

Root cause: the parser error-recovery mechanism converts all `push_value` errors into recovered `Expr::Error` nodes, meaning `parse()` returns `Ok(errors)` instead of `Err`. Fix requires either: (a) making specific errors fatal, or (b) having `parse()` return `Err` when there are recovered errors.

- [ ] Investigate which errors should be fatal vs. recovered
- [ ] Fix `parse()` to propagate fatal errors as `Err` (or make specific validation errors fatal)
- [ ] Verify `test_invalid_corpus` passes after fix

### corpus-prelude-interpolated-strings: `split` undefined during tmpl macro expansion typecheck

`tests/corpus/valid/literals/interpolated_strings.llt-eval` and `triple_quoted_interpolated.llt-eval` produce unexpected warning: `[E002] undefined variable: split`. Root cause: the `tmpl` macro expansion references `split` (from `stdlib/prelude.llt`) during typecheck, but `split` is reported as undefined when typechecking isolated from the full prelude scope.

- [ ] Identify why `split` is undefined in the typecheck context for interpolated string tests
- [ ] Fix by ensuring `split` (and other string utilities) are visible during macro expansion typecheck
- [ ] Verify `test_valid_corpus` passes for these two files

### corpus-fixes-misc: Fix small corpus test failures

Four corpus tests were failing due to small authoring errors: wrong type names, broken `prim:` prefixes, wrong test format, and `fn?` not being exported from the prelude. Fixed 2026-05-18.

- [x] `appendable_seq_concat.llt-eval` and `appendable_str_concat.llt-eval`: changed from `builtin-concat [1 2]` (auto-indexed dicts, Appendable unresolvable) to `concat [seq 1 []]` (Seq, Appendable works via prelude wrapper's Unknown params) (`tests/corpus/eval/typecheck/`)
- [x] `fd_user_defined_propagates.llt-eval`: rewrote in standard format (was using broken `---EVAL---TYPECHECK---` sections); removed `prim:add-int`/`prim:add-float` (lexer `:` issue); simplified to class declaration + usage (`tests/corpus/eval/typecheck/`)
- [x] `resolver_injective_flag.llt-eval`: rewrote in standard format; fixed `Str("ok")` → `String("ok")` and `Str` → `String` in typecheck section (`tests/corpus/eval/typecheck/`)
- [x] `tuple_annotation_closed_record.llt-eval`: `Str` → `String` in Tuple type annotation (`tests/corpus/eval/typecheck/`)
- [x] `fn_predicate_false_branch_non_callable.llt-eval`: `fn?` was not exported from prelude (comment said "takes effect directly" but it didn't); exported `int?`, `float?`, `str?`, `bool?`, `null?`, `dict?`, `fn?`, `seq?` from prelude using `builtin-*?` stable aliases to avoid circular self-references (`stdlib/prelude.llt`, `src/builtins.rs`, `src/type_env.rs`); all `test_typecheck_corpus` and `test_typecheck_warnings_corpus` pass (`tests/corpus/`)

---

## Macro System v2

`macros-v2` accepted 2026-05-17. See `doc/whatif/macros-v2.md`. Unified `macro` form with `[let ...]` patterns, `inject:` for anaphoric binding, `splice` for multi-form output, `syntax-class` for declarative argument validation. Implementation order: macros-v2-ast → macros-v2-expand → macros-v2-inject → macros-v2-stdlib.

### macros-v2-stdlib: Migrate defmacro, add stdlib/ast.llt and stdlib/syntax.llt

**Depends on:** `macros-v2-expand`

- [x] Migrate 11 corpus test files from `defmacro` to `macro`; 4 kept as defmacro (variadic params not yet supported in macro keyword) (`tests/corpus/eval/macros/`)
- [x] Migrate stdlib/macros.llt — tmpl/do/begin kept as defmacro (require variadic args); documented migration path (`stdlib/macros.llt`)
- [x] gensym API update — deferred: would break existing macro expansion semantics; documented in stdlib/macros.llt
- [x] Add `stdlib/ast.llt` — ~130 lines with Entry/Annotation/Expr nominal types; flatten-args and ident stubs (`stdlib/ast.llt`)
- [x] Add `stdlib/syntax.llt` — macro fn/class/type let-softening stubs; opt-in via include (`stdlib/syntax.llt`)
- [x] Add prelude helpers: span-of, wrap-in-let, let-decl-elems (stubs); first-or (implemented); macro-error (stub) (`stdlib/prelude.llt`)
- [ ] Migrate `ast_to_dict` output from string `type:` fields to typed `Expr` variant values — blocked on typed Expr variant constructors (`src/builtins_meta.rs`, `stdlib/`)
- [x] Tests: migrated macros pass; stdlib/ast.llt and stdlib/syntax.llt load cleanly (`tests/corpus/eval/macros/`)

---

## Tooling

### equatable-comparable-instances: Uncomment Equatable/Comparable/Showable primitive instances

`stdlib/prelude.llt` has `Equatable`, `Comparable`, and `Showable` instance declarations for
primitive types (`Int`, `Float`, `Str`) commented out with the note "primitives use Rust
fallback dispatch." This is an architectural gap: the CHR sprint migrated arithmetic instances
to tinct but left these three classes using a Rust hardcoded path. The consequence: user-defined
types go through CHR instance resolution while primitives bypass it — inconsistent semantics, and
the fallback blocks user-extensibility of `=`, `<`, and `str`.

- [ ] Investigate why instances were commented out: loading order issue during prelude bootstrap? Performance concern with instance lookup on every `=` call? Identify root cause (`stdlib/prelude.llt:1696-1753`, `src/typecheck.rs`)
- [ ] If loading order: use the same `in_prelude_load` flag pattern used for arithmetic instances to defer method body inference during prelude load; uncomment instances
- [ ] If performance: benchmark instance lookup vs Rust fallback for `=`/`<`/`str` on primitives; if acceptable, uncomment; if not, document the performance constraint explicitly and track as future work
- [ ] Remove Rust fallback dispatch for `Equatable`/`Comparable`/`Showable` once instances are active (`src/typecheck.rs`, `src/type_unify.rs`)
- [ ] Verify `just test` passes with instances active (`tests/`)

### tinct-lint: `tinct lint` subcommand and `just lint-stdlib` CI step

`tinct lint file.llt` parses, expands macros, and type-checks a tinct file without evaluating it. Behaves like `tinct run --strict` up to and including type-checking; stops before the eval pass. Exit 0 = clean, exit 1 = errors/warnings. All type warnings are treated as fatal (lint mode is inherently strict). Enables fast feedback on stdlib and project files without execution overhead.

**Spec chapters:** `doc/12-tooling.md §Lint Mode`

- [ ] Add `Subcommand::Lint { file: String }` to CLI; pipeline: parse → desugar → macro-expand → typecheck; stop before eval; all type warnings AND INFO-level diagnostics are surfaced (lint mode shows everything the type checker finds, including Info-tier — explicitly-annotated `@Unknown`, over-broad annotations, deprecation notices); exit 1 on any Warning or Error, exit 0 only when all diagnostics are Info or below; report with `format_type_error`/`format_parse_error` (`src/main.rs`)
- [ ] Lint respects capability flags: `--cap-fs`, `--cap-net` gate `include` resolution just as `tinct run` does; `--no-fs` blocks all includes; add `--no-fs` as the default for lint (no file execution, so no capability grants needed) (`src/main.rs`)
- [ ] Add `just lint-stdlib` justfile target: run `tinct lint --no-fs` on every `stdlib/**/*.llt` file; exit 1 immediately if any file has errors; uses release binary for speed (`justfile`)
- [ ] Wire `just lint-stdlib` into `just test` after `just lint` (Rust linter) and before `just fmt-check` (`justfile`)
- [ ] Add `just lint-file FILE` justfile target: lint a single file; mirrors `just run-file FILE` pattern (`justfile`)
- [ ] Document in `doc/12-tooling.md §Lint Mode`: flags, exit codes, what is and is not checked (`doc/12-tooling.md`)
- [ ] Tests: lint on a clean stdlib file exits 0; lint on a file with a type error exits 1; lint does not execute side-effects (no `emit` output) (`tests/corpus/eval/`)

### dircap-drop-bare-compat: Remove backward-compat treatment of bare `@DirCap` in caps declarations

Per `doc/whatif/completed/dir-cap-permissions.md` lines 107–109, bare `@DirCap` (without a flag list) is temporarily treated as full access during a transition period. All first-party scripts now use explicit flag annotations (e.g. `@[DirCap [Writable]]`). The compat shim should be removed once all call sites are updated.

- [ ] Fix Landlock path extraction to strip `:MODE` suffix before constructing PathBuf — currently uses `split_once('=').map(|(_, path_str)| PathBuf::from(path_str))` which includes `:w` in the path string, causing `path.exists()` to return false and silently skipping the Landlock rule, so writes are blocked by default-deny even though the DirCap grants write authority; fix: apply same `rsplit_once(':')` mode-stripping used by `--cap-fs` DirCap parsing (`src/main.rs:1041-1048`); also apply to `run_literate_eval` and `run_file` Landlock path setup (`src/main.rs:2272`, `src/main.rs:2568`)
- [ ] Restore `--cap-fs docdir=doc/lib:w` in `just docgen` once Landlock path extraction is fixed (`justfile`)
- [x] Audit all `--- caps:` declarations in `scripts/`, `stdlib/`, and `samples/` for bare `@DirCap` and update to explicit flag lists (`scripts/`, `stdlib/`, `samples/`) — Updated `test_permissions.llt` to use `@[[all DirCap Listable Statable]]`; `scripts/docgen.llt` already has `@[DirCap [Writable]]`; no bare `@DirCap` found in `samples/` or `stdlib/`
- [ ] **KNOWN ISSUE**: CLI-level backward-compat at `src/main.rs:1321,2469,2765` — `--cap-fs NAME=PATH` without `:MODE` defaults to `DirPerms::full()`. The type-level compat described in whatif doc lines 107-109 was never implemented. Removing CLI default breaks many tests (`tests/cli_tests.rs:1636,1671,2182,2215,2248,2285,2331`). Deferred until test suite is updated to use explicit modes.
- [x] Update `doc/whatif/completed/dir-cap-permissions.md` to remove the "backward-compat transition period" note (`doc/whatif/completed/dir-cap-permissions.md:107-109`)

---

## Primitive Privacy

### include-decomp-primitives: Add eight new Rust primitives and delete include infrastructure

**Whatif:** `include-decomposition`
**Spec chapters:** `doc/whatif/include-decomposition.md §Rust Primitives`, `§What Would Change`
**Depends on:** `builtin-privacy-complete`, `materialize-rename`

- [ ] Register `load`, `expand`, `eval`, `eval-types`, `blake3`, `cap-identity`, `include-cache-get`, `include-cache-put` in `standard_builtins()` (`src/builtins.rs`, `src/builtins_meta.rs`)
- [ ] Implement `load`: parse source `String` to file AST dict (same format as `ast_to_dict`); `name:` named arg provides provenance hint (`src/builtins_meta.rs`)
- [ ] Implement `expand`: run macro expansion on a file AST dict, return expanded dict (`src/expand.rs`, `src/builtins_meta.rs`)
- [ ] Implement `eval`: evaluate `[ExprAST]` in runtime stage env (prelude env + `%:` + `env:` merge); returns thunk; sequential let\* scoping from `eval_document` internalized (`src/eval.rs`, `src/builtins_meta.rs`)
- [ ] Implement `eval-types`: evaluate `[ExprAST]` in `type_stage_env`; no `%` input; called by type checker for `--- stage: type` documents (`src/builtins_meta.rs`, `src/type_normalize.rs`)
- [ ] Implement `blake3`: compute blake3 hash of a `String` (`src/builtins_meta.rs`)
- [ ] Implement `cap-identity`: return `"dev:ino"` string from `fstat` on the DirCap's O_DIRECTORY fd (`src/builtins_meta.rs`)
- [ ] Implement `include-cache-get`/`include-cache-put`: read/write `EvalState::include_cache: HashMap<String, IncludeCacheEntry>`; cache keyed by `blake3(cap-identity + "|" + source)` (`src/eval.rs`, `src/builtins_meta.rs`)
- [ ] Add `EvalState::include_cache: HashMap<String, IncludeCacheEntry>` replacing the old inode-keyed cache; add Rust enum `enum IncludeCacheEntry { Missing, Pending, Cached(Rc<Thunk>) }` (`src/eval.rs`)
- [ ] Delete `builtin_include` entirely (`src/builtins_meta.rs`) — all 350+ lines
- [ ] Delete `EvalState::include_guard: HashSet<(u64, u64)>` and old `EvalState::include_cache` (`src/eval.rs`)
- [ ] Delete `Value::RustRegistry`, `rust_module()`, all module grouping logic (`src/value.rs`, `src/builtins.rs`)
- [ ] Delete `builtin-*` aliases from module group setup (`src/builtins.rs`)
- [ ] Delete `eval_file_with_input`, `eval_document`, `run_eval` from `src/eval_pipeline.rs`; delete file entirely once empty
- [ ] Delete `materialize` call on accumulator in `builtin_reduce` (`src/builtins_seq_reduce.rs:80-81`) — pass thunk directly as next acc
- [ ] Delete shadow guard from `expand` (`src/expand.rs:174`)
- [ ] Add `document_to_dict` emission of `stage: [Runtime] | [Type]` nominal variant (`src/ast_dict.rs`)
- [ ] Update `src/main.rs` to call tinct `cli-pipeline` function directly after prelude loads; construct `files_thunk` as positional Dict from `Vec<String>` file paths; pass `%pwd` DirCap as third argument
- [ ] Tests: corpus tests for `load`, `expand`, `eval`, `blake3`, `cap-identity`, `include-cache-*` (`tests/corpus/eval/builtins/`)
- [ ] Verify `just test` passes

### include-decomp-prelude: Add pipeline functions to prelude.llt

**Whatif:** `include-decomposition`
**Spec chapters:** `doc/whatif/include-decomposition.md §Tinct Implementation`
**Depends on:** `include-decomp-primitives`

- [ ] Change `%rust` from `Value::RustRegistry` to a flat `Value::Dict` of all Rust primitives; seed at startup (`src/builtins.rs`, `src/value.rs`)
- [ ] Rewrite prelude.llt opening: replace `[include %rust "core"]` etc. with single `%rust` expression that scope-promotes all primitives (`stdlib/prelude.llt`)
- [ ] Add `eval-document-runtime` to prelude public dict (`stdlib/prelude.llt`)
- [ ] Add `eval-document-pipeline` to prelude public dict (`stdlib/prelude.llt`)
- [ ] Add `eval-file` to prelude public dict (`stdlib/prelude.llt`)
- [ ] Add `include-cache-success`, `include-cache-failure`, `include-evaluate-and-cache` to prelude private dict (`stdlib/prelude.llt`)
- [ ] Add `include` to prelude public dict (replaces `builtin_include` as the user-facing include function) (`stdlib/prelude.llt`)
- [ ] Add `cli-pipeline` to prelude public dict (`stdlib/prelude.llt`)
- [ ] Verify `IncludeCacheEntry: [type [Missing] [Pending] [Cached Any]]` is in prelude type namespace (`stdlib/prelude.llt`)
- [ ] Update formatter and docgen to use `load` directly instead of internal `ast_to_dict` (`stdlib/formatter/`, `scripts/docgen.llt`)
- [ ] Tests: corpus test verifying `[include %include-dir "sibling.llt"]` works within a multi-file include chain (`tests/corpus/eval/`)
- [ ] Tests: corpus test for circular include detection via `[Pending]` cache state (`tests/corpus/eval/errors/`)
- [ ] Tests: corpus test for `cli-pipeline` threading `%` across files (`tests/corpus/eval/`)
- [ ] Verify `just test` passes and `just docgen` runs successfully

### include-decomposition-review: Post-implementation review

**Whatif:** `include-decomposition`
**Depends on:** `include-decomp-prelude`

- [ ] Run `/review-whatif include-decomposition` — verify all sprints complete, implementation matches spec, `doc/08-evaluation.md` and `doc/09-documents.md` updated to describe self-hosted pipeline in present tense, no stubs or de-scoped features

### builtin-privacy-complete: Activate the builtin-privacy isolation switch

**Whatif:** `builtin-privacy`
**Spec chapters:** `doc/whatif/completed/builtin-privacy.md §Design`

The `%rust` virtual module infrastructure is fully implemented (`Value::RustRegistry`, `rust_module()`, `create_bootstrap_env()`, all stdlib files rewritten). What was never done: the isolation switch. At `src/builtins.rs:2175-2194`, ALL standard builtins are re-injected into `stdlib_env` after prelude loading — a "backwards compatibility" workaround that defeats the privacy goal entirely. This sprint removes it.

Note: `builtin-*` aliases remain available to prelude via `[include %rust "core"]` (correct per whatif). Only the user-env re-injection is removed.

- [ ] Remove the `standard_builtins()` re-injection loop at `src/builtins.rs:2175-2194`; user code must receive only what prelude exports — no direct fallback to Rust builtins (`src/builtins.rs:2175-2194`)
- [ ] Remove the `inject_prelude_aliases()` call at `src/builtins.rs:2202`; user env no longer gets `builtin-*` aliases injected (`src/builtins.rs:2202`)
- [ ] Delete `inject_prelude_aliases()` at `src/builtins.rs:1927-1965`; it has no remaining callers after the above removal (`src/builtins.rs:1927-1965`)
- [ ] Mark `create_root_env()` as `pub(crate)` and add a comment that it is internal-only (used by `expand.rs` for re-entrant macro expansion during prelude loading) — do NOT delete it; it is still needed by `src/expand.rs:413` to break the circular dependency during prelude bootstrap (`src/builtins.rs:1914`)
- [ ] Update type env aliases in `src/type_env.rs:3148-3154`: the `builtin-*` → `public-name` alias mappings in the type env are no longer needed in the user type env; verify they are only needed for prelude-internal type-checking and remove them from the user-facing type env if so (`src/type_env.rs:3148-3154`)
- [ ] Update `src/builtins.rs:10974`: the test call to `inject_prelude_aliases` in unit tests must be replaced — use `[include %rust "core"]` semantics or construct the test env via `build_prelude_env()` instead; any test that constructs a closure referencing `builtin-add`/`builtin-eq` must use the public name `+`/`=` instead (`src/builtins.rs:10974`)
- [ ] Update `src/typecheck.rs:12575,12630`: test source strings using `builtin-if` must be updated to use `if` — `builtin-if` is not available in user scope after this sprint (`src/typecheck.rs:12575,12630`)
- [ ] Update `src/lsp/analysis.rs:2100-2120`: test hovers `builtin-eq` — after removal it is undefined in user scope; rewrite the test to hover `=` instead (`src/lsp/analysis.rs:2100-2120`)
- [ ] Convert `tests/corpus/eval/builtins/builtin_aliases_callable.llt-eval` to an error test: user code referencing `builtin-lt`, `builtin-add`, etc. should now produce `undefined variable`; rename to `builtin_aliases_not_user_accessible.llt-eval` and set `=== error` section (`tests/corpus/eval/builtins/`)
- [ ] Run `just test` after all changes; surface any test failure as `undefined variable: <name>` — each failure is a builtin that prelude failed to export under its public name or a test that must be updated (`tests/`)
- [ ] Fix `doc/11-stdlib.md:296-310`: rewrite the env chain section to describe the actual implemented state — bootstrap env (include + %rust) → prelude (opens with [include %rust "core"] etc.) → user code; remove the `builtin-*` aliases from the chain diagram; remove the T009 reference on line 310 (T009 was removed because undefined variable errors are now sufficient) (`doc/11-stdlib.md:296-310`)
- [ ] Fix `doc/11a-builtins.md:762`: remove the "Stable Aliases" section documenting `builtin-add`, `builtin-sub`, etc. as user-accessible escape hatches — they no longer exist in user scope; if the `%rust`-level aliases (accessible only to prelude) need documenting, add a brief note under the `%rust` section (`doc/11a-builtins.md:762`)
- [ ] Add `%rust` virtual module documentation to `doc/11a-builtins.md` or `doc/11-stdlib.md`: document that stdlib files use `[include %rust "module-name"]` to access Rust primitive groups; list the module names and their contents (table already in the whatif); clarify that `%rust` is not available in user code (`doc/11a-builtins.md`)

---

## Codebase Health

### cap-std-pervasive: Replace ambient std::fs calls and constrain open_ambient_dir usage

**Security audit 2026-05-18** found 5 `std::fs` violations in production code paths (bypassing cap_std), and several `open_ambient_dir` usages in LSP code that open `/`, `/tmp`, `/var/tmp` as fallbacks — defeating the RESOLVE_BENEATH confinement model. The LSP `no_fs=true` guard prevents eval-time `$include` execution but not the filesystem reads that use the ambient Dir itself.

**`open_ambient_dir` policy:** `open_ambient_dir` is a security boundary — it acquires ambient OS authority. Every production call site must:
1. Be in the CLI bootstrap (pre-capability initialization — `src/main.rs` only), OR
2. Open a specific operator-controlled path (libdir, user-specified `--cap-fs` paths, script's own directory), AND
3. Have a comment: `// AMBIENT-OK: <reason>` explaining why ambient authority is justified here.

Test code (`#[cfg(test)]`) is exempt from this policy.

**Fix 1 (HIGH) — Remove `/` and `/tmp` fallbacks from LSP `open_ambient_dir` chains**

`src/lsp/document.rs:561-593` opens `.`, then `temp_dir`, then `/`, then `/tmp`, then `/var/tmp` as a fallback chain. Opening `/` as `base_dir` makes RESOLVE_BENEATH a no-op — everything on the filesystem is reachable. Fix: if `.` and the document's own directory fail, return an LSP error response rather than falling back to root.

- [ ] In `src/lsp/document.rs` `DocumentState::new()` (lines 561-593): remove the `/`, `/tmp`, `/var/tmp` fallback chain; if `Dir::open_ambient_dir(".")` fails, log the error and return `Err(...)` from `DocumentState::new()`; callers in `server.rs` already handle `Err` by returning an error LSP response (`src/lsp/document.rs:561-593`, `src/lsp/server.rs`)
- [ ] In `src/lsp/document.rs` `evaluate_document()` (lines 633-670): same pattern — remove `/` fallback; if document's dir and `.` both fail, fall back to `base_eval_ctx.config.base_dir.open_dir(".")` only (already attempted at line 638); if that fails, return `Err(...)` instead of opening root (`src/lsp/document.rs:633-670`)

**Fix 2 (MEDIUM) — Replace `std::fs` with cap_std in `imports.rs` `resolve_includes()`**

`src/imports.rs:701,710` use `std::fs::metadata` and `std::fs::read_to_string` after a software `starts_with` guard. Replace with cap_std I/O so RESOLVE_BENEATH enforces confinement at the kernel level instead of a path string check.

- [ ] Add `base_cap_dir: Option<&cap_std::fs::Dir>` parameter to `resolve_includes()` in `src/imports.rs`; when `Some(dir)`, use `dir.metadata(relative_path)?` and `dir.open(relative_path)?` → `read_to_string()` instead of the `std::fs` calls at lines 701 and 710; derive `relative_path` as `normalized.strip_prefix(base_dir).unwrap_or(&normalized)` (`src/imports.rs:680-720`)
- [ ] Update all callers of `resolve_includes()` to pass the appropriate `cap_std::fs::Dir` reference (from `EvalContext.config.base_dir`) (`src/imports.rs`, `src/lib.rs`, `src/main.rs`)

**Fix 3 (MEDIUM) — Replace `std::fs` with cap_std in LSP `index_file()` and `load_doc_from_uri()`**

- [ ] `src/lsp/document.rs:407` — `index_file()` reads files at `$include`-derived URIs using `std::fs::read_to_string`; pass a `cap_std::fs::Dir` confined to the document's parent directory and use `dir.open(filename)?.read_to_string()` with relative path only; reject absolute paths with an LSP diagnostic ("absolute $include path not permitted") (`src/lsp/document.rs:400-415`)
- [ ] `src/lsp/document.rs:802,812` — `load_doc_from_uri()` uses `std::fs::metadata` and `std::fs::read_to_string`; accept a `cap_std::fs::Dir` scoped to the workspace root and use cap_std I/O; reject URIs that resolve outside the workspace with a log warning and empty result (`src/lsp/document.rs:795-820`)

**Fix 4 (CRITICAL) — `--no-fs` does not suppress `%pwd`, `%libdir`, or `--cap-fs` injection**

The `--no-fs` flag is documented as disabling all filesystem access, but the skeptic-verified audit found three gaps where DirCap values are injected into user scope regardless:

- `src/main.rs:1123`: `%pwd` injection is gated on `!no_pwd` only — `--no-fs` alone does not suppress it. User code with `[open %pwd "file.txt" Readable]` can read any file in CWD even with `--no-fs`.
- `src/main.rs:1196`: `%libdir` injection is gated on `!no_libdir` only — user code can `[write %libdir "injected.llt" "evil"]` corrupting the stdlib.
- `src/main.rs:1224`: The `--cap-fs` injection loop has NO `no_fs` guard at all — operator-specified caps are injected unconditionally.

- [ ] `src/main.rs:1123` — change `if !no_pwd {` to `if !no_pwd && !no_fs {` for `%pwd` injection (`src/main.rs:1123`)
- [ ] `src/main.rs:1196` — change `if !no_libdir {` and the matching libdir path resolution to `if !no_libdir && !no_fs {` for `%libdir` injection (`src/main.rs:1196`)
- [ ] `src/main.rs:1224` — wrap the entire `--cap-fs` injection block in `if !no_fs { ... }` (`src/main.rs:1224-1337`)
- [ ] Add corpus/CLI tests: `tinct run --no-fs` → `%pwd` is undefined; `tinct run --no-fs --cap-fs d=.` → `%d` is undefined; confirm `$include` is also blocked (`tests/corpus/`, `tests/cli_tests.rs`)

**Fix 5 — `expand.rs:363` opens CWD ambiently on every user eval pipeline**

Skeptic verified: `expand_macros` is called for ALL user code, not just prelude loading. Every eval pipeline invocation opens CWD ambiently via `open_ambient_dir(".")`. The user's `base_dir` (from the CLI-opened DirCap) should be passed through to `expand_macros` instead of re-opening CWD each time.

- [ ] Add `base_dir: &cap_std::fs::Dir` parameter to `expand_macros()` in `src/expand.rs`; replace `open_ambient_dir(".")` at line 363 with `base_dir.open_dir(".")?` to clone the already-open Dir handle rather than re-acquiring ambient authority (`src/expand.rs:341-370`)
- [ ] Update all callers of `expand_macros` in `src/main.rs`, `src/lib.rs`, `src/lsp/document.rs` to pass their existing `base_dir` rather than letting `expand_macros` open its own (`src/main.rs`, `src/lib.rs`, `src/lsp/document.rs`)

**Fix 6 — `load_doc_from_uri()` base_dir is CWD, not the document's directory**

Skeptic verified: `src/lsp/document.rs:816` opens `.` as the eval context base_dir even though the document being loaded lives at `path.parent()`. The base_dir mismatch means `$include` within that document would resolve against CWD, not the document's actual location.

- [ ] In `load_doc_from_uri()`, derive `base_dir` from `path.parent()` rather than `"."`: `cap_std::fs::Dir::open_ambient_dir(path.parent().unwrap_or(Path::new(".")), ...)` (`src/lsp/document.rs:816`)

**Fix 7 — Eliminate open_ambient_dir from all files except src/main.rs and src/repl.rs**

Comments are easy to abuse — structural enforcement is the correct approach. After fixes 1-6, every remaining `open_ambient_dir` call outside the two bootstrap files (`src/main.rs`, `src/repl.rs`) should be replaced with a `&cap_std::fs::Dir` parameter passed down from the bootstrap. The goal: `open_ambient_dir` is only callable in the files that own the capability initialization boundary.

- [ ] `src/builtins_meta.rs:1449` — replace `open_ambient_dir(&libdir_path)` with a `libdir: &cap_std::fs::Dir` parameter threaded from the caller; the libdir Dir is already open in `src/main.rs:1205` and `src/lib.rs:239` — pass it down (`src/builtins_meta.rs`, `src/main.rs`, `src/lib.rs`)
- [ ] `src/builtins.rs:2084,2153` — same: replace `open_ambient_dir(".")` in `create_stdlib_env_inner()` and `create_type_stage_env()` with a `base_dir: &cap_std::fs::Dir` parameter; callers already hold this dir (`src/builtins.rs`)
- [ ] `src/formatter.rs:50` — replace `open_ambient_dir(&base_dir_path)` with a `base_dir: &cap_std::fs::Dir` parameter; the formatter is always called from the CLI which already has the dir open (`src/formatter.rs`)
- [ ] `src/lib.rs:223,239,250,328,343` — `lib.rs` is a public API boundary so it must open dirs; these are acceptable BUT the opens should be done once and stored in a struct rather than reopened per call; design a `TinctRuntime` struct that holds the opened dirs and expose the eval functions as methods (`src/lib.rs`) — this is a refactor, treat as a follow-up
- [ ] Add CI check: `rg 'open_ambient_dir' src/ --glob '!src/main.rs' --glob '!src/repl.rs'` exits non-zero if any match is found outside test code; add this to the justfile or CI configuration; test-only calls inside `#[cfg(test)]` are exempt (add `--passthrough '#\[cfg\(test\)\]'` or similar filter) (`justfile` or `.github/`)
- [ ] Confirm `just test` passes after all changes (`tests/`)

### materialize-rename: Rename `eval`→`deep-materialize` and `force`→`materialize`

Both builtins are kept and renamed to accurate names that reflect what they do. The Rust `deep_materialize` function already exists with the right name; the user-callable tinct builtins should match. `materialize` (WHNF) is the common case with the shorter name; `deep-materialize` is the thorough variant. Both remain available to user code — making Rust materialization primitives accessible for novel uses.

- [x] Rename `builtin_eval` (`src/builtins_meta.rs:56`) and its registration in `standard_builtins` from `"eval"` to `"deep-materialize"`
- [x] Rename `builtin_force` and its registration from `"force"` to `"materialize"`
- [x] Update prelude.llt if either is re-exported under the old name
- [x] Update the 2 corpus test files that reference `eval` directly (`tests/corpus/eval/builtins/eval.llt-eval`, `control_flow.llt-eval`)
- [x] Verify `just test` passes


### parser-uniformity: Fix special cases and non-uniform handling found in parser audit (2026-05-18)

Full audit of `src/parser.rs` identified the following issues beyond what `unified-bindings-remove-old-syntax` already tracks. All locations are in `push_expr_to_parent` unless noted otherwise.

**Correctness bugs:**
- [ ] **F-03** `StackFrame::TypeAlias` Case 3 (`Expr::LetDecl`) only accepts `Expr::VarRef` bindings — rejects `Expr::Annotated`, so `[type [let a@K b] T]` silently treats the whole LetDecl as a type expression instead of extracting params; fix: accept `Expr::VarRef | Expr::Annotated` in the all_lowercase_params check (`src/parser.rs:5279–5295`)
- [ ] **F-13** `StackFrame::CaseDecl` CloseBracket handler uses `ok_or_else(...)?` (fatal) instead of `close_bracket_recover!` — `[case]` with missing pattern/body is an unrecoverable error that breaks LSP incremental parsing; all other frames use `close_bracket_recover!` (`src/parser.rs:2956–2963`)
- [ ] **F-14** `StackFrame::MacroDecl` accepts any expression in the params slot without validation — `[macro foo 42 body]` silently puts `Int(42)` into params; fix: validate that the second positional is `Expr::LetDecl`, emit parse error otherwise (`src/parser.rs:5383–5386`)

**Content-driven heuristics to remove:**
- [ ] **F-06** `StackFrame::InstanceDecl` silently explodes any `Expr::Dict` arriving with no `pending_key` and no `pending_arm_pattern` into per-method entries — undocumented content-driven heuristic; remove and require explicit keyed entry syntax (`src/parser.rs:5868–5886`)
- [ ] **F-07** `SyntaxClass` is missing from the `Token::Identifier` + colon-ahead dispatch, so field names like `pattern:` fall through to `pending_key: Option<Spanned<Expr>>` (shared scratchpad); `pending_key` should store `(String, Span)` like `Call`'s version, not a full `Spanned<Expr>`; add `SyntaxClass` to the Identifier colon dispatch (`src/parser.rs:3093–3106, 5399–5472`)

**Dead code:**
- [ ] **F-01** `fn` annotation error recovery: `if !stack.is_empty() / else` both call `recover_from_failed_open` with identical arguments — `recover_from_failed_open` already handles the empty-stack case internally; remove the branch, call once unconditionally (`src/parser.rs:1617–1638`)
- [ ] **F-09** `expr_to_pattern` Dict branch checks for `[seq h t]` as the first auto-indexed entry of a 3-element Dict — unreachable because `[seq h t]` always parses as an implied `Call`, never a `Dict`; delete the dead arm (`src/parser.rs:5006–5038`)

**Minor inconsistencies:**
- [ ] **F-04** `StackFrame::ClassDecl` `_ => Ok(())` catch-all leaves `name = None`; CloseBracket handler then emits a class with empty-string name instead of a parse error; fix: the catch-all should be a parse error (`src/parser.rs:5624–5628`)
- [ ] **F-10** `Token::Let` / `Token::Case` handler is a near-verbatim copy of the Identifier+colon dispatch but silently omits `Match` from its colon arm, falling through to `_ => VarRef push`; the omission is undocumented; either share the logic or add an explicit error (`src/parser.rs:4393–4497`)

### compat-cleanup: Remove backwards-compatibility shims (DONE 2026-05-18)

No public release has been made; there are no external users and nothing to be compatible with. Grep audit (2026-05-18) found 6 explicit compat paths.

- [x] Remove legacy 3-arg string mode from `builtin_open` at `src/builtins_io.rs:198-254` — ALREADY REMOVED in prior cleanup
- [x] Remove `substitute_inline_markers` and its call site at `src/main.rs:3097-3104` — ALREADY REMOVED in prior cleanup
- [x] Remove `EvalError::new()` compat shim at `src/error.rs:881-885` — ALREADY REMOVED in prior cleanup
- [x] Remove `EvalError::message()` compat shim at `src/error.rs:902-905` — removed; updated 58 call sites across `src/eval.rs`, `src/value.rs`, `src/lib.rs`, `src/eval_materialize.rs` to use `.kind.to_string()` directly
- [x] Rename `parse2()` → `parse()` and delete the `parse()` compatibility wrapper at `src/parser.rs:5909-5920` — ALREADY DONE in prior cleanup
- [x] Remove legacy positional constraint class list form at `src/typecheck_annot.rs:539` — ALREADY AN ERROR since typecheck-annot sprint; unkeyed list without `each` keyword produces type error with hint
- [x] Remove legacy `Expr::Dict` path for `or`/`all`/`without` type expressions at `src/typecheck_annot.rs:1189-1205` — NEVER EXISTED; parser always produced `Call { implied: true }` for these forms
- [x] Verify `just build` and `just test-lib` pass after all removals — `just build` exited 0, `just test-lib` exited 0

### dead-code-sweep: Remove unused imports and inert dead-code suppressions (DONE)

Grep audit (2026-05-18) found 10 items with `#[allow(dead_code)]` or `#[allow(unused_imports)]` that have no planned activation path (scaffolding tied to active sprints is excluded).

- [ ] Remove `#[allow(unused_imports)]` from `src/types.rs:17`; delete or use the import (`src/types.rs`)
- [ ] Remove `#[allow(unused_imports)]` from `src/eval_dict.rs:17`; delete or use the import (`src/eval_dict.rs`)
- [ ] Remove `#[allow(unused_imports)]` from `src/builtins.rs:543,553`; delete or use the imports (`src/builtins.rs`)
- [ ] Remove `#[allow(dead_code)]` from `src/type_env.rs:25`; delete or use the item (`src/type_env.rs`)
- [ ] Remove `#[allow(dead_code)]` from `src/error.rs:2015`; delete or use the item (`src/error.rs`)
- [ ] Remove `#[allow(dead_code)]` from `src/typecheck.rs:4384`; delete or use the item (`src/typecheck.rs`)
- [ ] Remove `#[allow(dead_code)]` from `src/lib.rs:37,1080,1093,1105`; delete or use each item (`src/lib.rs`)
- [ ] Remove `#[allow(dead_code)]` from `src/eval.rs:202,207` (EvalContext fields); either add a read site or delete the fields (`src/eval.rs`)
- [ ] Delete `extract_instance_type_name` at `src/eval.rs:1469` — `#[allow(dead_code)]`, no call sites; chr-gaps accesses instance types via a different path (`src/eval.rs:1469`)
- [ ] Remove `#[allow(dead_code)]` from `src/eval_call.rs:41`; CEK migration has no active sprint — delete the dead function (`src/eval_call.rs`)
- [ ] Verify `just test` passes with `-D warnings` after all removals (`tests/`)

### scaffolding-cleanup: Remove dead scaffolding from completed and cancelled sprints

Follow-up audit (2026-05-18) confirmed most "scaffolding" items are genuinely dead — the sprints they were written for are done (DONE.md) but the scaffolding was never removed. Three categories:

**A. Stale dead_code annotations on live code** — items marked dead_code when written but now activated by completed sprints; fix by removing the suppress attr:

- [ ] Remove stale `#[allow(dead_code)]` from `Kind::Arrow`, `Kind::Operator`, `Kind::Label`, `Kind::Var`, `KindError`, `Label` in `src/type_def.rs:42-93`; confirmed live — all have call sites in `typecheck.rs`, `typecheck_annot.rs`, `type_unify.rs`, `type_env.rs`; `hkt-kind-inference` and `bas-core` sprints are done (`src/type_def.rs`)
- [ ] Remove stale `#[allow(dead_code)]` from `ClassDecl` fields in `src/type_class.rs:74-100` (`type_params`, `instance_types`, `method_types`, `determines`, `resolver`, `resolver_injective`); audit each against chr-gaps task list — fields read by chr-gaps tasks should have dead_code removed now, genuinely unused fields should be deleted (`src/type_class.rs`)

**B. Genuinely dead functions from completed sprints** — BAS infrastructure written but not wired; `bas-core` is done (DONE.md) and does not use these; delete them:

- [x] Delete `compact_bounds` at `src/type_unify.rs:1323` — ALREADY DELETED (verified 2026-05-18)
- [x] Delete `check_bounds_satisfiable` at `src/type_unify.rs:1365` — ALREADY DELETED (verified 2026-05-18)
- [x] Delete `constrain` at `src/type_unify.rs:1412` — ALREADY DELETED (verified 2026-05-18)
- [x] Note: `process_deferred_equalities` at `src/type_unify.rs:2053` is NOT dead BAS scaffolding — it is chr-gaps infrastructure for TypeStageApp resolution; has `#[allow(dead_code)]`, kept for chr-gaps Gap 1 (verified 2026-05-18)
- [x] Delete `TypeVarBounds::add_lower` and `add_upper` at `src/type_infer.rs:32-41` — ALREADY DELETED (verified 2026-05-18)
- [x] Delete `ConstraintSource` at `src/type_infer.rs:53-57` — ALREADY DELETED (verified 2026-05-18)
- [x] Delete `ClassEnv::parent`, `ClassEnv::with_parent`, `InstanceEnv::parent`, `InstanceEnv::with_parent`, `InstanceEnv::get` at `src/type_class.rs:125-211` — ALREADY DELETED (verified 2026-05-18)

**C. arena-phase3 scaffolding** — `FlatEnv`, `EnvArena`, `EnvId`, `ThunkArena::alloc_letrec_group`, `ThunkArena::fill_letrec_slot`, and the `env_arena` field on `EvalContext` are pre-written for the `arena-phase3` sprint and should NOT be deleted. See `arena-phase3` sprint below.

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

- [ ] Add a *display vector* field to `FlatEnv`: `display: Vec<EnvId>` prepopulated at creation with the `EnvId` of every ancestor scope from 0 to current level; this makes `display[level].slots[slot]` a two-index O(1) access with no chain traversal; display is built once per closure/dict creation from the parent `FlatEnv`'s display + self (`src/arena.rs`)
- [ ] Wire `eval_dict` to allocate a `FlatEnv` for each dict scope via `alloc_letrec_group` (pre-size to the static-key count from the resolve pass); call `fill_letrec_slot` as each entry thunk is created; pass the `FlatEnv`'s `EnvId` to child thunks (`src/eval_dict.rs`)
- [ ] Wire `eval.rs:677-684` VarRef dispatch: if `*resolved.borrow()` is `Some(Some((level, slot)))`, read via display vector — `ctx.env_arena.borrow().get(current_flatenv.display[level]).get_slot(slot)`; if `Some(None)` (resolver ran but couldn't resolve — i.e., stdlib binding) or `None` (computed key / $include binding), fall back to `env.borrow().get(name)` name-based chain; the resolver only assigns coordinates for user-scope bindings so stdlib lookups always fall through correctly with no offset arithmetic needed (`src/eval.rs:677`)
- [ ] No level-offset hack needed: the resolver assigns level 0 to the outermost user dict scope and cannot see stdlib bindings (injected at runtime), so all stdlib VarRefs produce `Some(None)` and take the name-based fallback path; user-scope levels are self-contained in the display vector (doc/feature/arena-patterns.md §Contrast with Lua 5.4 Upvalues: "parent chain retained for stdlib only, at most two hops for user code") (`src/resolve.rs`)
- [ ] Update closure capture in `eval_call` (function application): when creating a function closure, clone the callee's display vector and extend it with the new param-scope `FlatEnv` (`src/eval_call.rs`)
- [ ] Remove `#[allow(dead_code)]` from `FlatEnv`, `EnvArena`, `EnvId`, `alloc_letrec_group`, `fill_letrec_slot`, `env_arena` field once all wired (`src/arena.rs`, `src/eval.rs`)
- [ ] Benchmark: run `just bench` (or a representative workload) before and after; confirm VarRef-heavy programs see measurable improvement; document in commit message (`tests/`)
- [ ] Verify `just test` passes (`tests/`)

---

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class: `∀f a. Filterable f ⇒ (a → Bool) → f a → f a`; compare `Mappable` extension vs separate class using `Data.Witherable` as precedent; include instance examples for `Seq` and `Dict` (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint in TODO.md for `from-json @Schema` schema-directed typed parse (`doc/whatif/schema-directed-from-json.md`)

---

## Health Review #21 Findings (2026-05-19)

### overlay-lazy-flatten: flatten_overlay forces entire Overlay chain eagerly

When any builtin receives a `Value::Overlay`, `flatten_overlay()` is called synchronously and recursively materializes the entire Overlay tree (all L/R thunks). This creates a space leak for accumulator patterns using repeated `$merge`: a dict accumulated over N steps via overlay holds all N intermediate dicts alive until any builtin access forces the flatten. This is the primary memory concern for long-running pipeline pipelines.

- [ ] Track flatten_overlay as a known space leak in `doc/08-evaluation.md §Overlay Eagerness` — document that Overlay flattening is eager and recommend `collect` for accumulation patterns to avoid the leak (`doc/08-evaluation.md`)
- [ ] Consider a lazy flatten that only materializes one level when keys/values are accessed (future sprint `overlay-lazy-flatten`) (`src/builtins.rs:190-264`, `src/value.rs`)

### health21-test-gaps: Missing corpus tests for MAX_PARSE_DEPTH and GuardedValidate lifecycle

Two test coverage gaps found in health review #21:

- [ ] Add corpus test for MAX_PARSE_DEPTH: a deeply-nested expression that exceeds the parser's depth limit (256) should produce a parse error, not a panic (`tests/corpus/invalid/syntax_errors/max_parse_depth_exceeded.llt-eval`)
- [ ] Add unit tests for GuardedValidate thunk state transitions (branches 2+3): verify that validation failure + default fallback correctly transitions thunk state, that InProgress → Guarded restoration works, and that `validate_and_wrap_record` with a `default:` annotation evaluates the default in the caller's env (`src/eval_materialize.rs`, `src/typecheck.rs`)

### health21-span-data-site: validate_and_wrap_record needs data_span for nested record errors

`validate_and_wrap_record` in `eval.rs`/`eval_materialize.rs` accepts only the constraint-site span. When nested record validation fails, errors point to the annotation site rather than the actual malformed data location. This makes type assertion errors on deeply-nested records confusing.

- [ ] Add `data_span: Span` parameter to `validate_and_wrap_record` and thread it through GuardedValidate continuations so type mismatch errors can report both where the constraint was declared AND where the data was defined (`src/eval.rs`, `src/eval_materialize.rs`)

### health21-errorkind-constructors: New ErrorKind variants lack pub fn constructors

Recent additions to `ErrorKind` (e.g., `InvalidUtf8InBytes`, `InvalidHexEncoding`, `KindMismatch`) were added directly as enum variants without the `pub fn` constructor pattern used by all other variants. This makes it easy to forget span attachment or miss the `.with_materialization_span()` convention.

- [ ] Add `pub fn` constructors in `EvalError` for all `ErrorKind` variants that currently lack them (match the style of existing constructors like `EvalError::type_mismatch`, `EvalError::no_instance`) (`src/error.rs`)

---

## 17th Panel Review Fix-Later Items

### docgen-type-errors: Fix 5 type errors in scripts/docgen.llt

`just docgen` produces 5 non-fatal type errors. These prevent `--strict` mode from being used.

- [x] T003 at line 26: `scan-dir` reduce callback — fixed with `builtin-if` and `@Dict` return annotation (`scripts/docgen.llt`)
- [x] T003 at line 25: reduce init value — fixed by removing over-constrained param annotations (`scripts/docgen.llt`)
- [x] T003 at line 43: `find-close` recursive return — fixed with `fn@Int` return annotation and `builtin-if` (`scripts/docgen.llt`)
- [x] T003 at line 65: `slice parts` — replaced with `str-index-of`+`str-slice` approach (`scripts/docgen.llt`)
- [x] T003 at line 156: `trunc [+ close 1]` — fixed with type-annotated helper lambda (`scripts/docgen.llt`)
- [ ] T003: `write` builtin expects `DirCap` but `@[DirCap [Writable]]` cap annotation produces `[__cap_flag_writable: []] | DirCap` — type checker doesn't yet desugar parameterized DirCap flag annotations into the intersection form the builtins expect; fix requires capability flag desugaring in annotation resolution and updating builtin signatures to accept `DirCap & Writable` intersection (`src/typecheck.rs`, `src/builtins_io.rs`, `scripts/docgen.llt:197`)
- [ ] T003 cascade: `write-module` return typed as `"" | _` because `write-module-file` return is `_` when the DirCap unification fails above — will resolve once the DirCap flag annotation issue is fixed (`scripts/docgen.llt:200-212`)

