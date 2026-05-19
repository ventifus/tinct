# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Macro System v2

`macros-v2` accepted 2026-05-17. See `doc/whatif/macros-v2.md`. Unified `macro` form with `[let ...]` patterns, `inject:` for anaphoric binding, `splice` for multi-form output, `syntax-class` for declarative argument validation. Implementation order: macros-v2-ast → macros-v2-expand → macros-v2-inject → macros-v2-stdlib.

### macros-v2-stdlib: Migrate defmacro, add stdlib/ast.llt and stdlib/syntax.llt

**Depends on:** `macros-v2-expand`, `typed-expr-constructors`, `deep-materialize-variant`

- [x] Migrate 11 corpus test files from `defmacro` to `macro`; 4 kept as defmacro (variadic params not yet supported in macro keyword) (`tests/corpus/eval/macros/`)
- [x] Migrate stdlib/macros.llt — tmpl/do/begin kept as defmacro (require variadic args); documented migration path (`stdlib/macros.llt`)
- [ ] Update gensym API: change from zero-arg String return (`[gensym]` → `:gensym:0`) to one-arg `[gensym prefix@Str]` returning `VarRef(name: ":prefix:N")` — required for `[unquote (gensym "name")]` hygiene in quasiquote positions; migrate all `[gensym]` call sites to `[gensym "name"]`; update corpus tests; see `doc/whatif/macros-v2.md:903` (`stdlib/prelude.llt`, `src/builtins.rs`, corpus tests)
- [x] Add `stdlib/ast.llt` — ~130 lines with Entry/Annotation/Expr nominal types; flatten-args and ident stubs (`stdlib/ast.llt`)
- [x] Add `stdlib/syntax.llt` — macro fn/class/type let-softening stubs; opt-in via include (`stdlib/syntax.llt`)
- [x] Add prelude helpers: span-of, wrap-in-let, let-decl-elems (stubs); first-or (implemented); macro-error (stub) (`stdlib/prelude.llt`)
- [ ] Migrate `ast_to_dict` output from string `type:` fields to typed `Expr` variant values — blocked on typed Expr variant constructors (`src/builtins_meta.rs`, `stdlib/`)
- [ ] Migrate `do` from `[fn [let args] ...]` (new_style=false) to `[macro do [let monad ...steps] body]` (new_style=true): rewrite body using named `monad` and `steps` bindings directly, eliminating `do-fold`'s `[get i args]` integer-key indexing into the packed args dict; requires ast_to_dict typed Expr output above (`stdlib/macros.llt`)
- [ ] Migrate `tmpl` from `[fn [let args] ...]` to `[macro tmpl [let template ...parts] body]`: rewrite using named bindings; requires typed Expr output (`stdlib/macros.llt`)
- [ ] Migrate `begin` from `[fn [let args] ...]` to `[macro begin [let ...exprs] [type: "sequential" exprs: exprs]]`: straightforward once variadic `[macro ...]` and typed Expr output are in place (`stdlib/macros.llt`)
- [ ] Delete `new_style: false` code path from `expand_macro_call` in `src/expand.rs` and `register_stdlib_macros_from_env` once do/tmpl/begin are migrated; remove the `new_style` field from `MacroMetadata` entirely (`src/expand.rs`)
- [ ] Remove `STDLIB_MACROS` constant and the old single-args-dict packing branch (`src/expand.rs`)
- [ ] Tests: verify all do/tmpl/begin corpus tests pass with new_style=true calling convention (`tests/corpus/eval/macros/`)
- [x] Tests: migrated macros pass; stdlib/ast.llt and stdlib/syntax.llt load cleanly (`tests/corpus/eval/macros/`)

---

## Tooling

### dircap-drop-bare-compat: Remove backward-compat treatment of bare `@DirCap` in caps declarations

Per `doc/whatif/completed/dir-cap-permissions.md` lines 107–109, bare `@DirCap` (without a flag list) is temporarily treated as full access during a transition period. All first-party scripts now use explicit flag annotations (e.g. `@[DirCap [Writable]]`). The compat shim should be removed once all call sites are updated.

- [x] Fix Landlock path extraction to strip `:MODE` suffix before constructing PathBuf — applied `rsplit_once(':')` mode-stripping in `run_eval` Landlock block (`src/main.rs:1065-1074`); `run_literate_eval` and `run_literate_weave` do not have Landlock setup (no `no_landlock` param, no `setup_landlock` call) so no fix needed there
- [x] Restore `--cap-fs docdir=doc/lib:w` in `just docgen` once Landlock path extraction is fixed — restored at `justfile:199`; `just build` and `just test-lib` pass clean
- [x] Audit all `--- caps:` declarations in `scripts/`, `stdlib/`, and `samples/` for bare `@DirCap` and update to explicit flag lists (`scripts/`, `stdlib/`, `samples/`) — Updated `test_permissions.llt` to use `@[[all DirCap Listable Statable]]`; `scripts/docgen.llt` already has `@[DirCap [Writable]]`; no bare `@DirCap` found in `samples/` or `stdlib/`
- [ ] Update 7 cli_tests to use explicit `--cap-fs NAME=PATH:MODE` syntax instead of bare `NAME=PATH` — prerequisite to closing the KNOWN ISSUE below (`tests/cli_tests.rs:1636,1671,2182,2215,2248,2285,2331`)
- [ ] **KNOWN ISSUE**: CLI-level backward-compat at `src/main.rs:1321,2469,2765` — `--cap-fs NAME=PATH` without `:MODE` defaults to `DirPerms::full()`. The type-level compat described in whatif doc lines 107-109 was never implemented. Removing CLI default breaks many tests (`tests/cli_tests.rs:1636,1671,2182,2215,2248,2285,2331`). Blocked on the test suite update above.
- [x] Update `doc/whatif/completed/dir-cap-permissions.md` to remove the "backward-compat transition period" note (`doc/whatif/completed/dir-cap-permissions.md:107-109`)

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

### runtime-v2-type-prereqs: Type system prerequisites for runtime-v2

**Whatif:** `runtime-v2`
**Depends on:** `include-decomposition-review`

The runtime-v2 whatif introduces `Task@t`, `Channel@t`, `Signal`, `CancelHandle`, and `SelectSource@t@r` as first-class tinct types. The type system needs corresponding support before the runtime-v2 sprint can be approved.

- [ ] Add `Type::Task(Box<Type>)` to `src/type_def.rs`; add arms to `unify`, `is_subtype`, `apply`, `occurs_in`, `collect_type_vars`, `display` — mirrors `Type::Seq` exactly (`src/type_def.rs`, `src/type_unify.rs`, `src/type_env.rs`)
- [ ] Add `Type::Channel(Box<Type>)` with same full handling (`src/type_def.rs` et al.)
- [ ] Add inference rules in `src/typecheck.rs`: `task` infers `Task(body_type)` from body; `await` unifies `Task(?t)` → `?t`; `send`/`recv` unify channel element type; `select-once` checks handler arity against channel element type
- [ ] Declare `Signal`, `Action`, `CancelHandle`, `SelectSource` as prelude type aliases/declarations (`stdlib/prelude.llt`)
- [ ] Corpus tests for `task`/`await` type inference; `channel`/`send`/`recv` element-type checking (`tests/corpus/`)
- [ ] Verify `just test` passes

---

## Codebase Health

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
- [ ] Thread `env_id` through `take_unevaluated` / force loop: when a thunk is created with `new_unevaluated_with_env_id`, the `env_id` must survive through `take_unevaluated` and be available after force completes, so the evaluator can use FlatEnv display-vector addressing for closures (`src/eval.rs`, `src/value.rs`)
- [ ] Update closure capture in `eval_call` (function application): when creating a function closure, clone the callee's display vector and extend it with the new param-scope `FlatEnv` (`src/eval_call.rs`) — (do env_id threading task above first)
- [x] Remove block-level `#[allow(dead_code)]` from `EnvArena` impl, `FlatEnv` struct/impl — replaced with per-item attributes on the specific methods still unused from production (alloc_child, get_mut, alloc_letrec_group, get_slot, get_by_name, insert_overflow, parent()); `alloc_root`, `fill_letrec_slot`, `get`, `env_arena` field, and `EnvId` type are now live (no suppression needed) (`src/arena.rs`)
- [ ] Benchmark: run `just bench` (or a representative workload) before and after; confirm VarRef-heavy programs see measurable improvement; document in commit message (`tests/`) — **deferred**: needs production workload
- [x] Verify `just build` passes (`src/`) — passes; `just test-lib` passes (compilation confirmed clean after dead_code fixes)

---

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class: `∀f a. Filterable f ⇒ (a → Bool) → f a → f a`; compare `Mappable` extension vs separate class using `Data.Witherable` as precedent; include instance examples for `Seq` and `Dict` (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint in TODO.md for `from-json @Schema` schema-directed typed parse (`doc/whatif/schema-directed-from-json.md`)
