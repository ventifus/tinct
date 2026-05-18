# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Macro System v2

`macros-v2` accepted 2026-05-17. See `doc/whatif/macros-v2.md`. Unified `macro` form with `[let ...]` patterns, `inject:` for anaphoric binding, `splice` for multi-form output, `syntax-class` for declarative argument validation. Implementation order: macros-v2-ast → macros-v2-expand → macros-v2-inject → macros-v2-stdlib.

### macros-v2-ast: New AST variants and parser changes

See `doc/whatif/macros-v2.md §What Would Change`. **Spec chapters:** `doc/whatif/macros-v2.md §AST Types`, `doc/02-syntax.md §Macros`.

- [x] Add `Expr::MacroDecl`, `Expr::Splice`, `Expr::SyntaxClass` to `src/ast.rs`; updated all exhaustive match sites across 15 files (`src/ast.rs`, `src/eval.rs`, `src/typecheck.rs`, etc.)
- [x] Add `macro` and `syntax-class` keyword dispatch with colon-ahead guard; `StackFrame::MacroDecl` and `StackFrame::SyntaxClass` parser frames (`src/parser.rs`)
- [x] Pre-scan pass: `pre_scan_file()` walks AST, collects `MacroDecl`/`SyntaxClass`/`DefMacro` before expansion; extract `inject:` defaults (`src/expand.rs`)
- [x] Moved ClassDecl param-list validation from parser to type checker (`src/parser.rs`, `src/typecheck.rs`)
- [x] Tests: macro keyword parse, syntax-class colon-ahead dict, fn/type newline-colon error updates (`tests/corpus/`)

### macros-v2-expand: Expansion pass, splice, syntax-class validation, inject threading

**Depends on:** `macros-v2-ast`

- [x] Update expander: `[let ...]` pattern matching for macro arg binding; MacroDecl converts params to Fn, registers as `new_style: true` (`src/expand.rs`)
- [x] Splice handling: `Expr::Splice` in dict context injects forms; in expression position → E012 error; MacroDecl in splice output registered immediately (`src/expand.rs`)
- [x] Syntax-class validation: `@VarRef`/`@Literal`/`@Call` annotations validated before macro body; E012 on mismatch (`src/expand.rs`)
- [x] Inject threading: `inject_default` extracted and passed as `binding` arg; dict-key position passes key name (`src/expand.rs`)
- [x] Add `ErrorKind::MacroError { message: String }` with E012 code; `EvalError::macro_error()` constructor; macro expansion provenance working (`src/error.rs`, `src/expand.rs`)
- [x] Tests: placeholder tests for splice/inject/error provenance; macro keyword parsing corpus tests (`tests/corpus/eval/macros/`, `tests/corpus/valid/special_forms/`)

### macros-v2-inject: inject: and macro-injects reflection

**Depends on:** `macros-v2-expand`

- [x] `macro-injects` Rust builtin: looks up inject default from `ctx.config.macro_injects_map`; registered in `standard_builtins()` (`src/builtins_meta.rs`, `src/builtins.rs`)
- [x] Inject map wired from expansion to eval: `MacroEnv.get_inject_map()` → `ExpandResult.macro_injects_map` → `EvalConfig.macro_injects_map` (`src/expand.rs`, `src/eval.rs`, `src/lib.rs`)
- [x] Tests: `macro_injects_with_inject.llt-eval` and `macro_injects_without_inject.llt-eval` (`tests/corpus/eval/macros/`)

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

### fmt-tinct-only: Remove Rust formatter, make tinct scripts the sole fmt backend

`tinct fmt` currently has two backends: Rust-native (`format_source` / `format_source_compact` in `src/formatter.rs`) and tinct-hosted (`format_source_tinct`, gated behind `--tinct-fmt`). The Rust backend should be deleted; the tinct scripts are the only formatter going forward. `stdlib/formatter/compact.llt` and `stdlib/formatter/pretty.llt` move to `stdlib/cli/fmt/` alongside `cli/in/` and `cli/out/`. A new `-o <name>` flag selects which `cli/fmt/<name>.llt` script to use (default: `pretty`). The Rust-formatter-specific flags (`--oneline`, `--nospaces`, `--minimize`, `--tinct-fmt`) are removed.

- [ ] Move `stdlib/formatter/compact.llt` → `stdlib/cli/fmt/compact.llt` and `stdlib/formatter/pretty.llt` → `stdlib/cli/fmt/pretty.llt`; delete the now-empty `stdlib/formatter/` directory
- [ ] Add `-o <name>` / `--output <name>` to `Subcommand::Fmt` in `src/main.rs`; resolves to `stdlib/cli/fmt/<name>.llt` via `%libdir`; default `pretty` when omitted; error if the named script does not exist (`src/main.rs:169–200`)
- [ ] Remove `--tinct-fmt`, `--oneline`, `--nospaces`, `--minimize` flags from `Subcommand::Fmt` — these were Rust-formatter-specific; compact output is now `tinct fmt -o compact` (`src/main.rs:178–192`)
- [ ] Replace the `if tinct_fmt / else if oneline ... / else` dispatch (src/main.rs:1970–1981) with a single `format_source_tinct(source, fmt_script_path)` call using the resolved `cli/fmt/<name>.llt`
- [ ] Update `format_source_tinct` to accept a script path (or name) instead of a `compact: bool` flag; look up from `%libdir`/`cli/fmt/` (`src/main.rs` or wherever `format_source_tinct` is defined)
- [ ] Delete `format_source` and `format_source_compact` from `src/formatter.rs`; if `src/formatter.rs` contains nothing else, delete the file and remove it from `src/lib.rs`
- [ ] Update `just fmt-llt`, `just fmt-llt-check`, `just fmt-llt-fix` in `justfile` to drop any Rust-formatter flags; add `just fmt-llt-compact FILE` as `tinct fmt -o compact {{FILE}}` (`justfile:164–174`)
- [ ] Update `format_source_tinct` to unwrap `[Ok s]` from the formatter Result — both `cli/fmt/compact.llt` and `cli/fmt/pretty.llt` now return `[Ok String]` on success or `[Err msg]` on failure (via `[try [fn [] [format-file %]]]`); the Rust caller must extract the payload from `Ok` and surface the `Err` message as an error
- [ ] Switch LSP format-on-save to `format_source_tinct` using `cli/fmt/pretty.llt` — the LSP was intentionally left on the Rust path when `tinct-hosted-formatter` shipped (cycle #310); now that the Rust path is being deleted, the LSP must use the tinct path (`src/lsp/`)
- [ ] Update any `--tinct-fmt` references in doc, tests, or corpus files
- [ ] Verify `just test` passes

### unified-bindings-remove-old-syntax: Remove pre-unified-bindings param syntax from fn, type, and class

`unified-bindings-migrate` (DONE.md) checked off "Remove old param-list parsing paths" prematurely. Old-form detection survives in three places:

- **`fn` / `macro` / `defmacro`:** `parse_param_list` (src/parser.rs:798) treats `let` as optional (skips it if present, lines 820–824); called from `fn` (line 1656), `macro` (line 1869), and `defmacro` (line 1901). Also: `push_expr_to_parent` for `StackFrame::Fn` has an implied-call heuristic (lines 5250–5276) that detects all-lowercase `[a b c]` bracket as a param list.
- **`type` (TypeAlias):** `push_expr_to_parent` for `StackFrame::TypeAlias` (lines 5228–5295) has three cases — Case 1: `Expr::Dict` with auto-indexed lowercase vars (lines 5232–5248), Case 2: implied-call all-lowercase (lines 5250–5277); both are old forms. Case 3: `Expr::LetDecl` (lines 5278–5295) is the new form.
- **`class` (ClassDecl):** `push_expr_to_parent` for `StackFrame::ClassDecl` (lines 5527–5629) handles `Expr::VarRef` (lines 5541–5546), `Expr::Dict` (lines 5548–5570), and `Expr::Call { implied: true }` (lines 5572–5598) as old forms; `Expr::LetDecl` (lines 5600–5623) is the new form.

The goal is complete deletion of old paths, not a fallback parse error. `[let ...]` already works in all three contexts via `Expr::LetDecl` in `push_expr_to_parent` — no new code needed, only deletions.

- [ ] Manually rewrite all non-stdlib `.llt` files using old param syntax to `[let ...]` form; known: `scripts/docgen.llt` (all fn params); audit `samples/` for others
- [ ] Convert `defmacro` to deferred push_expr_to_parent pattern (receive name as VarRef, then LetDecl params) — currently the last remaining eager caller of `parse_param_list` besides `fn` and `macro`; migrate first, then delete `parse_param_list` and all three call sites
- [ ] Delete `parse_param_list` entirely (`src/parser.rs:798`) and all call sites (lines 1656, 1869, 1901) once defmacro is migrated
- [ ] Delete `push_expr_to_parent` `StackFrame::Fn` implied-call heuristic (lines 5250–5276)
- [ ] Delete `push_expr_to_parent` `StackFrame::TypeAlias` Cases 1 and 2 (Dict and implied-call detection, lines 5228–5277); keep only the `Expr::LetDecl` branch
- [ ] Delete `push_expr_to_parent` `StackFrame::ClassDecl` `Expr::VarRef`, `Expr::Dict`, and `Expr::Call { implied: true }` branches (lines 5541–5598); keep only `Expr::LetDecl` — no-param classes use `[class [let Equatable] ...]`; bare-word shorthand belongs in the `macro class` let-softening macro (macros-v2-stdlib)
- [ ] Verify `just test` passes after deletions
- [ ] Update DONE.md to note the `unified-bindings-migrate` checkbox was completed here (the original was premature)

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

### arithmetic-class-rename: Rename Add/Sub/Mul/Div → Addable/Subtractable/Multipliable/Divisible

The spec (`doc/whatif/chr-unification.md`, `doc/06-type-inference.md`) consistently uses `-able` suffixes. The implementation in `stdlib/prelude.llt` uses the shorter names. This is a naming bug — the spec is authoritative. All references must be updated.

- [ ] Rename class declarations in `stdlib/prelude.llt`: `Add` → `Addable`, `Sub` → `Subtractable`, `Mul` → `Multipliable`, `Div` → `Divisible` (`stdlib/prelude.llt:1650-1660`)
- [ ] Update all `[instance Add ...]`, `[instance Sub ...]` etc. in `stdlib/prelude.llt` to use new names (`stdlib/prelude.llt`)
- [ ] Update `lookup_arithmetic_instance` and any hardcoded class-name strings in Rust source (`src/type_unify.rs`, `src/type_normalize.rs`, `src/typecheck.rs`)
- [ ] Update constraint references in corpus tests: `[$Addable a b c]` etc. (`tests/corpus/`)
- [ ] Verify `just test` passes after rename (`tests/`)

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
- [ ] Audit all `--- caps:` declarations in `scripts/`, `stdlib/`, and `samples/` for bare `@DirCap` and update to explicit flag lists (`scripts/`, `stdlib/`, `samples/`)
- [ ] Remove the backward-compat fallback in the type checker / cap injection that treats bare `@DirCap` as full-access; make it a type error or at minimum a lint warning (`src/typecheck.rs`, `src/main.rs`)
- [ ] Update `doc/whatif/completed/dir-cap-permissions.md` to remove the "backward-compat transition period" note (`doc/whatif/completed/dir-cap-permissions.md:107-109`)

---

## CHR Unification

`chr-unification` accepted 2026-05-16 (commits 0886ef1, 7d15c36). See `doc/whatif/chr-unification.md` and `doc/feature/chr-unification.md`. Implementation order: chr-module-split → chr-normalization → chr-class-instance → chr-prelude.


### chr-prelude: Migrate arithmetic classes to prelude.llt and implement boundary guard elaboration

Moves the hardcoded arithmetic instance table out of Rust and into tinct itself, completing the CHR cycle with post-inference boundary guard elaboration. See `doc/feature/chr-unification.md §Boundary Guards` and `doc/06-type-inference.md §Constraint Handling Rules`.

**Spec chapters:** `doc/feature/chr-unification.md §Boundary Guards`, `doc/06-type-inference.md §CHR`

- [x] Add iteration cap (100) to `process_deferred_equalities()` (`src/type_unify.rs`)
- [x] Add corpus test for `determines:` extraction round-trip (`tests/corpus/eval/typecheck/class_determines_roundtrip.llt-eval`)
- [x] Improve disjointness/consistency error spans: both arm spans included (`src/typecheck.rs`)
- [x] Coverage error message: uses param name from `params` list (`src/typecheck.rs`)
- [x] Add `instance_resolution_depth: u32` to `InferState`; guard `resolve_instance` call in `check_constraints_on_var` (limit 64, matching GHC `-freduction-depth` per Sulzmann et al. 2007 §3.2); **unblocks all remaining chr-prelude and unified-bindings-migrate work** (`src/type_unify.rs`, `src/type_infer.rs`)
- [x] Add `in_prelude_load: bool` flag to `InferState`; skip InstanceDecl method body inference during prelude load (`src/type_infer.rs`, `src/typecheck.rs`, `src/imports.rs`)
- [x] Wire boundary guards from typecheck to eval pipeline: `boundary_guards` on EvalContext, `set_boundary_guards()` method; wired in `eval_source_with_config`, `eval_source_with_cap_net`, `run_eval` (`src/eval.rs`, `src/lib.rs`, `src/main.rs`)
- [x] Remove backward-compat legacy instance parsing — `legacy_arm_pattern` field removed, old syntax now produces parse error; 7 test files converted (`src/parser.rs`)
- [x] Write resolver functions (AddResult/SubResult/MulResult/DivResult) in `--- stage: type` section + arithmetic class declarations with `[determines: [...] resolver: ...]` + migrate 27 instances to `[instance ClassName [pattern [...]]: [...]]` syntax + 16 new arithmetic instances (`stdlib/prelude.llt`)
- [x] NormCtxt resolver_cache pre-populated (16 entries); `improve_functional_dependency` has `fd_depth` guard with `MAX_FD_DEPTH=16` (`src/type_normalize.rs`, `src/type_unify.rs`)
- [x] `boundary_guards: Vec<(Span, Type)>` added to InferState; collected at CALL-MONO and CALL-POLY boundaries (`src/type_infer.rs`, `src/typecheck.rs`)
- [x] Wire boundary guards to eval: create guarded thunks from `state.boundary_guards`; eval-side `ThunkState::Guarded` with BlameLabel (`src/eval.rs`)
- [x] Tests: full arithmetic FD + boundary guard tests (blocked on resolver activation) — boundary guard tests added (4 unit tests; FD tests remain blocked)

### chr-gaps: Three critical CHR implementation gaps found in full audit

Full audit (2026-05-17) found gaps preventing user-defined FD classes from working end-to-end.
**Implementation order: Gap 2 → Gap 1 → Gap 3 → Gap 4** (Gap 2 is a one-liner that unblocks 1 and 3).

**Gap 2 (CRITICAL — implement first) — FD fundep indices lost at constraint creation**

`typecheck_annot.rs:703` hardcodes `fundeps = vec![]` for all user-defined classes. `ClassDecl.determines`
is correctly populated during class registration and is accessible via `state.class_env`. This is a one-line fix.

- [ ] Replace line 703 in `src/typecheck_annot.rs` — change `let fundeps = vec![];` to `let fundeps = state.class_env.get(class_name).map(|decl| decl.determines.clone()).unwrap_or_default();` — `ClassDecl.determines: Vec<(Vec<usize>, Vec<usize>)>` matches the `Constraint::Class.fundeps` field type exactly; no struct changes needed (`src/typecheck_annot.rs:703`)
- [ ] Tests: `tests/corpus/eval/typecheck/fd_user_defined_propagates.llt-eval` — define a function annotated `[fn@c [$Merge a b c] [a@Dict b@Dict]]` where `Merge` has `determines: [[[a b] c]]`; verify type checker infers `c = Dict` without explicit annotation (`=== out` section shows `Fn@Dict [Dict Dict]`) (`tests/corpus/eval/typecheck/`)

**Gap 1 (CRITICAL — implement second) — Type-stage resolver evaluation stubbed**

`NormCtxt` has no access to the tinct evaluator at normalization time. The correct fix is to add
`type_stage_env: Option<Rc<RefCell<Environment>>>` to `NormCtxt` (populated from `imports::build_type_stage_env()`)
and a free function `evaluate_resolver` that calls the resolver thunk via a minimal EvalContext.

- [ ] Add `pub type_stage_env: Option<Rc<RefCell<Environment>>>` field to `NormCtxt` struct; populate it in `NormCtxt::new()` by calling `imports::build_type_stage_env()` (already exists but `#[allow(dead_code)]`; remove that attr) (`src/type_normalize.rs`, `src/imports.rs`)
- [ ] Add free function `evaluate_resolver(fn_name: &str, args: &[Type], env: &Rc<RefCell<Environment>>) -> Option<Type>` in `src/type_normalize.rs`: look up `fn_name` thunk in env; construct a minimal `EvalContext::new_empty(PathBuf::default(), Rc::clone(env), false)`; convert each `Type` arg to a type-dict thunk via `type_to_dict_thunk(ty)` (inverse of the existing `resolve_type_dict`); call `eval::invoke_function`; materialize; call `resolve_type_dict` on result → `Type` (`src/type_normalize.rs`)
- [ ] Replace the cache-miss stub at `src/type_normalize.rs:145-150` with: if `ctx.type_stage_env` is `Some(env)`, call `evaluate_resolver(fn_name, &normalized_args, env)`; on `Some(resolved)` insert into `ctx.resolver_cache` and return; on `None` return stuck `TypeStageApp` as before (`src/type_normalize.rs:145-150`)
- [ ] Replace the `continue` stub at `src/type_unify.rs:520-525`: when `class_decl.resolver.is_some()` and all determining positions are ground, call `normalize(Type::TypeStageApp { fn_name: resolver.clone(), args: det_ground_types }, &state.subst, &mut norm_ctx)` and use the result as the determined type; construct `norm_ctx` from `NormCtxt::new()` (which now carries `type_stage_env`) (`src/type_unify.rs:520-525`)
- [ ] Tests: `tests/corpus/eval/typecheck/fd_arithmetic_resolves.llt-eval` — `[fn@[$Add a b c] [a@Int b@Float]] 1 2.0` should infer return type `Float`; currently blocked (comment in TODO confirms FD tests blocked); `tests/corpus/eval/typecheck/fd_user_merge.llt-eval` — user-defined `Merge` class with `--- stage: type` resolver function, `[instance Merge [Dict Dict Dict] ...]`, call site; expect `c = Dict` resolved via resolver (`tests/corpus/eval/typecheck/`)

**Gap 3 (PARTIAL — implement third) — MPTC instance lookup API missing for user-defined classes**

`InstanceEnv.instances` uses `(class_name, single_type_string)` keys. For user-defined MPTCs, the key
must be `(class_name, Vec<String>)` covering all determining type positions.

- [ ] Change `InstanceEnv.instances` key from `(String, String)` to `(String, Vec<String>)` in `src/type_class.rs`; update `insert()` to build key as `(class_name, determining_type_strings)` where `determining_type_strings` is the vec of string-formatted types at `ClassDecl.determines` positions; update `get()` for compatibility (`src/type_class.rs`)
- [ ] Add `InstanceEnv::lookup_mptc(&self, class: &str, determining_types: &[Type]) -> Option<&InstanceDecl>`: build key as `(class.to_string(), determining_types.iter().map(type_to_string_key).collect())`; delegate to `self.instances.get(&key)` (`src/type_class.rs`)
- [ ] Replace the `_ => Err(...)` general path at `src/type_unify.rs:641-653` with a call to `state.instance_env.lookup_mptc(class, &det_ground_types)`; on `Some(inst)` extract the determined type from the instance arm and return it; on `None` return the existing error (`src/type_unify.rs:641-653`)
- [ ] Tests: `tests/corpus/eval/typecheck/mptc_user_lookup.llt-eval` — define `Concat` class with `[determines: [[[a b] c]] ...]` and `[instance Concat [[Str Str] Str] concat: [fn@Str [x@Str y@Str] [builtin-join "" [x y]]]]`; call `[concat "hello" " world"]`; expect `=== out` `"hello world"` with inferred type `Str` (`tests/corpus/eval/typecheck/`)

**Gap 4 (MINOR — implement independently) — resolver_injective has no parser support**

`ClassDecl.resolver_injective` exists (`type_class.rs:98`), hardcoded `false` everywhere, never read.

- [ ] In the class structural-metadata bracket parser (`src/parser.rs` — second positional bracket of `[class [...] [...] ...]`), add handling for key `injective:` alongside existing `determines:` and `resolver:`; when present with value `true`, set flag in parsed metadata (`src/parser.rs`)
- [ ] At `src/typecheck.rs:2424` (ClassDecl construction): read `resolver_injective` from parsed metadata and pass to `ClassDecl` (`src/typecheck.rs:2424`)
- [ ] Tests: `tests/corpus/eval/typecheck/resolver_injective_flag.llt-eval` — define a class with `injective: true`; verify it parses without error and `just test` passes; semantic effect is a no-op stub for now (`tests/corpus/eval/typecheck/`)

**Post-Gap-1 follow-up — wiring eval to extract type tag from pattern_expr**

Once resolver evaluation (Gap 1) is working, boundary guards at CALL-MONO/CALL-POLY sites need the resolved type to construct the guard thunk.

- [ ] At `src/eval.rs:1385`: extract the canonical type tag from `pattern_expr` and pass it to the boundary guard elaboration path; the comment says "chr-prelude sprint" — do this immediately after Gap 1 resolver eval lands (`src/eval.rs:1385`)

**Post-Gap-4 follow-up — class declaration formatter**

`src/formatter.rs:525` is a TODO to emit the structural metadata bracket (`[determines: [...] resolver: ...]`) when formatting a class declaration that has functional dependency or resolver fields. Currently silently omitted, causing round-trip loss.

- [ ] At `src/formatter.rs:525`: emit the class structural-metadata bracket when `determines` or `resolver` fields are non-empty; use the same bracket syntax the parser accepts (`src/formatter.rs:525`)

---

## Codebase Health

### error-nominal: Rename Err→Error, err?→error?, error→raise; lean on nominal Result type

Errors in tinct should use the nominal `Result` type (`Ok`/`Error`) as the primary idiom. Current issues: the `Err` constructor is abbreviated (should be `Error`); the `[error "msg"]` throw builtin shares a name root with the new `Error` constructor (confusing); `err?` is abbreviated.

**Design decisions:**
- `raise` takes **String only** — it abends the program; functional languages (OCaml, Elixir, F#) use `raise`; this is the right name for tinct's functional style
- `[Error "msg"]` is a **return value** used in `Result` — distinct from aborting; structured errors flow through return types, not exceptions
- `[raise [Error "msg"]]` is intentionally NOT supported — it would double-wrap; if you want to abort, pass a string; if you want to return an error, return `[Error "msg"]` directly
- `Result: [type [Ok a] [Error String]]` stays **concrete** (not parameterized) since `raise` only takes String and `try` always captures a string message
- `raise` is typed as `Never` — it never returns a value; fixes match arm type pollution (see `typecheck-gaps` sprint)

- [ ] Rename `Result` type comment on line 1339 and `Err: Err` re-export → `Error: Error` (`stdlib/prelude.llt:1339,1346`)
- [ ] Rename all `[Err _]:` match arms in prelude to `[Error _]:` — lines 259, 405, 999, 1358, 1370, 1382, 1394, 1406, 1503, 1507, 1533 (`stdlib/prelude.llt`)
- [ ] Rename `err?` → `error?` predicate; update doc strings and examples throughout (`stdlib/prelude.llt:1361–1370`)
- [ ] Update all doc strings referencing `[Err ...]` or `Err` constructor (`stdlib/prelude.llt`)
- [ ] Rename abend builtin: `"error"` → `"raise"` in `src/builtins_meta.rs` (function body + name string) and `src/builtins.rs` (registration); update `src/type_dict.rs:776` entry from `"error" => Ok(Type::Error)` to `"raise" => Ok(Type::Never)` (merges `typecheck-gaps` fix)
- [ ] Update `[try ...]` return tag in `src/builtins_meta.rs:185`: `tag: "Err"` → `tag: "Error"`; update the two `assert_eq!(tag, "Err")` in `src/builtins.rs:3764,3881`
- [ ] Update `type_env.rs:1666` and `typecheck.rs:11473` comments referencing `"Ok"/"Err"` tags
- [ ] Migrate all corpus tests: `[Err ...]` → `[Error ...]`, `[error ...]` → `[raise ...]`, `err?` → `error?` (`tests/corpus/`)
- [ ] Update doc examples, README, and `doc/*.md` referencing `error`, `Err`, or `err?` (`doc/`)
- [ ] Verify `just test` passes

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

### compat-cleanup: Remove backwards-compatibility shims

No public release has been made; there are no external users and nothing to be compatible with. Grep audit (2026-05-18) found 6 explicit compat paths.

- [ ] Remove legacy 3-arg string mode from `builtin_open` at `src/builtins_io.rs:198-254` — drop the `if matches!(third_arg_val, Value::String { .. })` branch; only the Variant-flags form (`[open dir path Readable Text]`) is the supported API; update any tests using `[open cap path "r"]` (`src/builtins_io.rs:158-254`)
- [ ] Remove `substitute_inline_markers` and its call site at `src/main.rs:3097-3104`; all doc/*.md files use `=== out` sections; the `<!-- tinct-result: ... -->` HTML comment format is fully retired (`src/main.rs:3093-3158`)
- [ ] Remove `EvalError::new()` compat shim at `src/error.rs:881-885`; grep for `EvalError::new(` and update all call sites to `EvalError::internal()` or a typed `ErrorKind` constructor (`src/error.rs:881`)
- [ ] Remove `EvalError::message()` compat shim at `src/error.rs:902-905`; grep for `.message()` and update all call sites to `.kind.to_string()` directly (`src/error.rs:902`)
- [ ] Rename `parse2()` → `parse()` and delete the `parse()` compatibility wrapper at `src/parser.rs:5909-5920`; update all callers (`src/parser.rs:5909`)
- [ ] Remove legacy positional constraint class list form at `src/typecheck_annot.rs:539` — the `[a: [Comparable Showable]]` form without an `each` keyword; make unkeyed list without `each` a type error with a hint pointing to `[each Comparable Showable]` syntax (`src/typecheck_annot.rs:539`)
- [ ] Remove legacy `Expr::Dict` path for `or`/`all`/`without` type expressions at `src/typecheck_annot.rs:1189-1205`; the parser consistently produces `Call { implied: true }` for these forms and the legacy path is provably unreachable (`src/typecheck_annot.rs:1189`)
- [ ] Verify `just test` passes after all removals (`tests/`)

### dead-code-sweep: Remove unused imports and inert dead-code suppressions

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

- [ ] Delete `compact_bounds` at `src/type_unify.rs:1323` — no call sites in production code; BAS done without it (`src/type_unify.rs:1323`)
- [ ] Delete `check_bounds_satisfiable` at `src/type_unify.rs:1365` — no call sites; BAS done without it (`src/type_unify.rs:1365`)
- [ ] Delete `constrain` at `src/type_unify.rs:1412` — no call sites from production; BAS done without it; also removes the only callers of `TypeVarBounds::add_lower`/`add_upper` (`src/type_unify.rs:1412`)
- [ ] Note: `process_deferred_equalities` at `src/type_unify.rs:2319` is NOT dead BAS scaffolding — it is chr-gaps infrastructure for TypeStageApp resolution; wire it as a call site in chr-gaps Gap 1 (resolver evaluation), then remove the `#[allow(dead_code)]` attr (`src/type_unify.rs:2319`)
- [ ] Delete `TypeVarBounds::add_lower` and `add_upper` at `src/type_infer.rs:32-41` — only called from dead `constrain()`; if no other callers after deleting `constrain`, remove these too (`src/type_infer.rs:32-41`)
- [ ] Delete `ConstraintSource` at `src/type_infer.rs:53-57` — defined, never constructed or referenced outside its file (`src/type_infer.rs:53-57`)
- [ ] Delete `ClassEnv::parent`, `ClassEnv::with_parent`, `InstanceEnv::parent`, `InstanceEnv::with_parent`, `InstanceEnv::get` at `src/type_class.rs:125-211` — "Scaffolding for scoped class environments" and "Instance lookup used during dictionary construction"; no sprint planned for scoped environments (`src/type_class.rs:125-211`)

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

### type-warning-channel: Emit typecheck diagnostics to LSP and CLI

The type-warning infrastructure is wired (InferState collects diagnostics, EvalContext has a channel slot) but emission is stubbed at both ends. Two call sites in `src/main.rs` and one in `src/lsp/document.rs` collect diagnostics then drop them.

- [ ] In `src/lsp/document.rs:135`: convert collected `Vec<TypeError>` diagnostics to LSP `Diagnostic` structs and publish via `publishDiagnostics` notification; use existing span-to-range conversion helpers already present in the file (`src/lsp/document.rs:135`)
- [ ] In `src/main.rs:1727`: emit collected diagnostics to stderr using `format_type_error`; apply the same severity mapping used for hard errors (`src/main.rs:1727`)
- [ ] In `src/main.rs:1965`: same as above for the second call site (`src/main.rs:1965`)
- [ ] Tests: corpus test with a type warning (e.g., `@Unknown` annotation) verifies warning appears in CLI output; LSP test verifies `publishDiagnostics` fires for a file with a type warning (`tests/corpus/`, `tests/lsp_corpus_tests.rs`)

### typecheck-gaps: Monomorphic recursion, tuple type encoding, and error return type

Three type system correctness gaps found in the 2026-05-18 audit with no existing sprint.

**`error` return type** (`src/type_dict.rs:776`): `[error ...]` is typed as `Type::Error`, which poisons any union containing it — `String | Type::Error = Type::Error` (src/type_def.rs:1308–1309). This means any `match` with a `_: [error ...]` catch-all arm infers `Type::Error` as its return type, making the whole binding `Unknown`. The fix: type `error` as returning `Never` (bottom/`~`), which is a subtype of all types, so `String | Never = String`. This would allow leaf formatter functions like `format-literal` to infer correctly.

**Monomorphic recursion** (`src/typecheck.rs:1951`): the type checker currently rejects all recursive binding-group references uniformly. This is overly conservative — recursive calls where the type is fully determined at the call site (e.g., `[fn@Int [let n@Int] [if [= n 0] 1 [* n [recur [- n 1]]]]]`) should be allowed; only polymorphic recursion (where the recursive call instantiates the function at a different type) should be rejected.

**Tuple type** (`src/typecheck.rs:2619`): tinct has no tuple type; the type checker stubs tuple entries as `Type::Unknown`. The correct encoding per BAS is a closed record: `(Int, Str)` → `{0: Int, 1: Str}`. This matches the evaluation model (tuples are dicts with integer keys).

- [ ] Type `error` builtin as returning `Never` instead of `Type::Error`: change `"error" => Ok(Type::Error)` to `"error" => Ok(Type::Never)` in `src/type_dict.rs:776`; verify `Type::Never` is already in the union-simplification rules (`src/type_def.rs:1308`) so `String | Never = String`; re-verify formatter functions type correctly after the fix
- [ ] Allow monomorphic recursion in `check_dict_entry_recursive`: if the recursive self-reference is at a consistent type (all uses unify to the same concrete type), allow it; block only if the call tries to instantiate the same binding at multiple incompatible types — implement using the existing SCC binding-group machinery (`src/typecheck.rs:1951`)
- [ ] Encode tuple type as closed record in `resolve_type_tuple`: replace `Type::Unknown` stub with `Type::Record(Row { fields: {0: T0, 1: T1, ...}, tail: RowTail::Closed })` where field names are the string forms of the integer positions; no new `Type::Tuple` variant needed (`src/typecheck.rs:2619`)
- [ ] Tests: `error` return type is `Never`; match with `_: [error ...]` catch-all infers concrete return type from other arms; monomorphic recursive function typechecks clean; tuple annotation resolves to closed record (`tests/corpus/eval/typecheck/`)

### io-phase2: `--libdir-path`, `source_file` attribution, and SPKI extraction

Three unrelated I/O and infrastructure gaps that have no sprint home.

- [ ] Add `--libdir-path PATH` CLI flag at `src/main.rs:1197`: override the stdlib directory for custom installations where `stdlib/` is not co-located with the binary; fall back to the current sibling-of-binary detection when flag is absent; wire through to `build_prelude_env()` via `EvalConfig` (`src/main.rs:1197`, `src/imports.rs`)
- [ ] Wire `source_file` through `EvalContext` at `src/eval.rs:1044`: the child `EvalContext` constructed for builtin call environments sets `source_file: None`, losing the originating file path for error attribution; propagate from the parent context (`src/eval.rs:1044`)
- [ ] Implement SPKI extraction in `builtin_tls_peer_cert` at `src/builtins_io.rs:3166`: properly parse the X.509 DER bytes to extract the SubjectPublicKeyInfo field; the `rustls` or `x509-parser` crate already in the dependency tree provides DER parsing (`src/builtins_io.rs:3166`)
- [ ] Note: QUIC datagram send/recv (`src/builtins_io.rs:4122`) requires async send/recv not available in the synchronous builtin model; tracked in `doc/whatif/async-eval.md`; no implementation sprint until async-eval is accepted and lands

---

### unknown-elimination: Replace remaining `Type::Unknown` builtin signatures with precise types

Replaces builtin `Unknown` return/param types with precise `TypeScheme` signatures where the type is statically knowable, as catalogued in `doc/11a-builtins.md`. See `doc/06-type-inference.md §Type Schemes`.

**Spec chapters:** `doc/11a-builtins.md`, `doc/06-type-inference.md §Type Schemes`

First-pass audit complete (2026-05-16). The following categories of Unknown remain and require future work:

**Category B — TypeVar polymorphism required (HKT or multi-arity):**
- `map`, `filter`, `reduce`: target `∀f a b. Mappable f => (a→b)→f a→f b`. Requires higher-kinded types (Type::App) not yet representable in TypeScheme. See comment `// TODO(unknown-elimination)` in each signature.
- `each`, `each-key`, `each-kv`: return element type requires HKT over input collection type.
- `builtin-collect`: `Seq(Unknown)` param; return Dict erases element type anyway — low priority.

**Category A — Record return types (closed Record schema needed):**
- `revocable`: returns `{cap: DirCap, revoke: Fn()->Null}` — expressible once Rust builtin signatures support closed Record return types.
- `recv-datagram`: returns `{data: Bytes, addr: Str, port: Int}`.
- `tls-peer-cert`: returns `{subject: Str, issuer: Str, sans: Seq(Str), ...}`.
- `icmp-ping`: returns `{rtt_ms: Int, success: Bool}`.
- `http-request`: returns `{status: Int, headers: Map(Str,Str), body: Bytes}`.
- `list-dir`: returns `Seq({name: Str, kind: Str, size: Int, ...})`.
- `stat`: returns `{name: Str, kind: Str, size: Int, ...}`.
- `timestamp-parts`: returns `{year: Int, month: Int, day: Int, hour: Int, minute: Int, second: Int}`.
- `timestamp-in-tz`: returns the above plus `offset-seconds: Int, tz-name: Str`.
- `builtin-first`/`builtin-last`: return type depends on input type (Dict element, Str char, Int byte).

**Category A — Genuinely unknown (no precise type possible without language feature):**
- `from-json`: requires schema-directed parsing; return is `Unknown` by design.
- `include`: included file type not knowable without parsing the included file at type-check time.
- `builtin-get`/`get?`: special-cased by `check_get` dispatcher; label-polymorphic scheme (`HasField l d a`) was attempted but reportedly caused inference to hang on prelude.llt (informal O(N²) analysis: ~35 `get` calls × HasField constraints × substitution merge loop); unproven whether this was a true performance issue or a unification bug — worth re-investigating once chr-class-instance lands a better HasField implementation.
- `map`/`filter`/`reduce` seq/init params: HKT required.
- `builtin-join` seq param: `stringify()` accepts any element type.
- `builtin-concat` return: merge shape not inferrable statically.
- Transport variant constants (`Tcp`, `Udp`, etc.): resolved via nominal variants — see `transport-typing` sprint.
- `connect` transport param: resolved via nominal variants — see `transport-typing` sprint.
- `Map` unparameterized constructor: `Unknown` K/V until user supplies type args.

**Tasks:**
- [x] Transport typing — resolved via `transport-typing` sprint (nominal variants, not `Type::Variant`)
- [x] Add closed-Record return type for `revocable`, `icmp-ping`, `recv-datagram`, `stat`, `timestamp-parts`, `timestamp-in-tz`, `timestamp-in-tz`, `tls-peer-cert`, `http-request` (`src/type_env.rs`)
- [x] Add precise `Seq({...})` return for `list-dir` — `Seq({name: Str, kind: Str, size: Int})` (`src/type_env.rs`)

---

### hkt-map-filter-types: Precise TypeSchemes for map/filter/reduce/each/each-key/each-kv

Replaces `Unknown` signatures with proper polymorphic `TypeScheme`s using `Type::Operator`/`Type::App` and the `Mappable` class; proposes `Filterable` for collection-polymorphic `filter`. See `doc/06-type-inference.md §Higher-Kinded Types` and `doc/11a-builtins.md §Collection Builtins`.

**Spec chapters:** `doc/06-type-inference.md §Higher-Kinded Types`, `doc/11a-builtins.md §Collection Builtins`

- [x] HKT types for map/filter/reduce/each — prerequisite research complete
- [x] json_to_value null behavior — by design: tinct's null IS empty dict (`[]`); JSON null → `[]` is correct per doc/03-data-model.md §Null; from-json @Schema will use the null-as-empty-dict model when implemented (`src/builtins_io.rs` — no change needed)
- [x] `map`: `∀f a b. Mappable f ⇒ (a → b) → App(f,a) → App(f,b)` — left as Unknown — needs full HKT; use `Type::Operator("f")` in body (`src/type_env.rs`)
- [x] `filter`: `∀a. (a → Bool) → Seq a → Seq a` — Seq-specific for now (`src/type_env.rs`)
- [x] `reduce`: `∀a b. (b → a → b) → b → Seq a → b` — Seq-specific, no HKT needed (`src/type_env.rs`)
- [x] `each`: `∀a b. (a → b) → Seq a → Null` — `b` is fresh and unreferenced; callback return discarded (`src/type_env.rs`)
- [x] `each-key`: `∀b. (Str → b) → Dict → Null` — tinct dict keys are always `Str` (`src/type_env.rs`)
- [x] `each-kv`: `∀b. (Str → Unknown → b) → Dict → Null` — value type `Unknown` for heterogeneous records; note `∀a b. (Str → a → b) → Map@[Str:a] → Null` for homogeneous maps (`src/type_env.rs`)
- [x] Corpus test updates: no corpus updates needed (Seq-specific types compatible) (`tests/corpus/`)
## 18th Panel Review Fix-Later Items

### panel-18-followup: Minor completeness and invariant documentation from 18th review

- [x] Add `builtins/errors` to required_dirs structural guard (`tests/corpus_tests.rs`)
- [x] Move 13 typecheck-warning tests — REVERTED: files belong in `errors/` (typecheck/ expects clean typecheck, these produce typecheck errors with warnings)
- [x] Rename `closed_record_rejects_extra.llt-eval` — REVERTED: kept original name, file belongs in errors/ for taxonomy consistency
- [x] `types_can_unify` substitution split — documented with explanatory comment (`src/typecheck.rs:1653-1656`)
- [x] SCC merge write-once invariant — REVERTED debug_assert (violated in practice by SCC letrec rebinding); replaced merged_keys filter with full-entry re-merge per SCC iteration (`src/typecheck_dict.rs`)
- [x] `sorted_by_empty.llt-eval` — fixed to use proper 2-arg comparator (`tests/corpus/eval/stdlib/sorted_by_empty.llt-eval`)
- [x] tag-of error corpus test — added `tag_of_non_variant.llt-eval` (`tests/corpus/eval/errors/`)
- [x] result monad dict corpus test — added `result_monad.llt-eval` (`tests/corpus/eval/stdlib/`)
- [x] `and-then` argument ordering inconsistency — KNOWN ISSUE, pre-existing design question (`stdlib/prelude.llt`)
- [x] `newline_breaks_dot_access.llt-eval` — fixed expected output (`tests/corpus/valid/edge_cases/`)

---

## 19th Panel Review Fix-Later Items

### panel-19-followup: Performance and security improvements from 19th review

- [ ] Add unit tests for `compute_sccs()` in typecheck_dict.rs — cover empty, two-node cycle, linear chain, diamond DAG; Tarjan algorithm is complex enough that corpus-only coverage is insufficient for lowlink propagation and root detection paths (`src/typecheck_dict.rs`)
- [ ] Add unit test for `compact_levels()` — verify unified TypeVar names are removed from levels HashMap after compact_levels() call; silent perf regression risk with no crash signal (`src/type_infer.rs:376-380`)
- [ ] Add unit test modules for builtins_math.rs and builtins_string.rs — cover MAX_SAFE_INT boundary (9007199254740992), try_dispatch_method fast-path, MAX_SPLIT_PARTS guard (`src/builtins_math.rs`, `src/builtins_string.rs`)
- [ ] Bump EVAL_LAZINESS_MIN from 37 to 40 and add 3 laziness proof tests (`tests/corpus_tests.rs`)
- [ ] SCC merge incremental optimization — current full re-merge is O(N×S); add snapshot tracking to only re-process entries whose value changed between SCC iterations (mitigation for O(N²) prelude startup cost) (`src/typecheck_dict.rs:516-542`)
- [ ] `intern_class_name` Box::leak — replace `Box::leak` fallback for user-defined class names with a thread-local intern table leaking each unique name only once, or change `instance_registry` key from `(&'static str, String)` to `(String, String)` to eliminate Box::leak entirely (`src/eval.rs:1480`)
- [ ] `values_eq_impl` depth parameter — add explicit `depth: usize` guard to make the implicit MAX_EVAL_DEPTH bound explicit and future-proof (`src/builtins_math.rs:378`)
- [ ] `instance_registry` lookup String allocation — change key from `(&'static str, String)` to `(&'static str, &'static str)` to eliminate `.to_string()` allocation per dispatch lookup (`src/builtins_math.rs:55`, `src/builtins_string.rs:62`)

---

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class: `∀f a. Filterable f ⇒ (a → Bool) → f a → f a`; compare `Mappable` extension vs separate class using `Data.Witherable` as precedent; include instance examples for `Seq` and `Dict` (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint in TODO.md for `from-json @Schema` schema-directed typed parse (`doc/whatif/schema-directed-from-json.md`)

---

## Documentation Consistency

### doc-consistency: Fill content gaps and fix feature-doc stale language

Doc consistency audit (2026-05-18). Principle: whatifs are the primary historical artifact (combined with git history). Main docs (`doc/*.md`) are the authoritative, atemporal reference — no temporal or hedging language, no references to whatifs, no "previously"/"planned"/"future" framing. Feature docs (`doc/feature/`) are optional deep-dive specs that may cross-reference whatifs as design history.

**Immediate fixes already applied (2026-05-18):**
- `doc/16-architecture.md:521` — removed "LLT has no network builtins" (replaced with NetCap description)
- `doc/11a-builtins.md:735` — renamed "Future Async Builtins" → "Async Builtins"; updated framing
- `doc/09-documents.md:299,794` — removed "Previously such bindings were silently ignored" and "Previously known defect (resolved)"
- `doc/07-type-extensions.md:655,792` — removed migration-instruction language ("is replaced by," "become")
- `doc/10-errors.md:436,729,937,945` — removed "currently," "future renderers," "backward compat during migration"
- `doc/06-type-inference.md:914` — fixed reference `doc/whatif/chr-unification.md` → `doc/feature/chr-unification.md`

**Content gaps to fill in main docs:**

- [ ] Add type-stage resolver syntax to `doc/06-type-inference.md`: show how to write a resolver in a `--- stage: type ---` section; `[class [...] [determines: [...] resolver: fn-name]]` form with a worked example; source: `doc/feature/chr-unification.md §Type-Stage Resolvers` (`doc/06-type-inference.md`)
- [ ] Add BAS intersection and negation annotation syntax to `doc/05-type-annotations.md`: `@[[all A B]]` for intersection, `@[[without A]]` for negation; currently absent from the annotation chapter (`doc/05-type-annotations.md`)
- [ ] Add stdlib typeclass hierarchy section to `doc/11-stdlib.md`: `Functor`, `Applicative`, `Monad`, `Foldable`, `Traversable`, `Mappable`, `Appendable` with method signatures; source: `doc/feature/hkt-monads.md §Typeclass Hierarchy` (`doc/11-stdlib.md`)
- [ ] Add `[do]` desugaring section to `doc/06-type-inference.md`: `[do [bind x: expr] body]` → `[>>= expr [fn [let x] body]]`, monad inference from return annotation, `>>=`/`>>` method lookup; source: `doc/feature/hkt-monads.md §do Desugaring` (`doc/06-type-inference.md`)
- [ ] Add NetCap allowlist behavior to `doc/11a-builtins.md`: DNS pinning, allowlist precedence, IPv4-mapped IPv6 handling; `doc/feature/io.md:342` explicitly flags this as undocumented (`doc/11a-builtins.md`)

**Stale language in feature docs:**

- [ ] `doc/feature/io.md:575-579,710` — remove "Phase 2 (future):" annotations for `Type::DirCap`, `Type::NetCap`, `Type::Handle`; these are implemented; rewrite as current fact (`doc/feature/io.md`)

**Completed whatifs without feature docs — verify content is in main docs:**

Four completed whatifs have no feature doc. The whatif is the design history; the content should be in the main docs. Verify and add if missing.

- [ ] `doc/whatif/completed/constraint-annotations.md` → verify `doc/05-type-annotations.md` covers `fn@[return: T constraint: [a: Comparable] doc: "..."]` dict syntax; add if missing (`doc/05-type-annotations.md`)
- [ ] `doc/whatif/completed/builtin-privacy.md` → verify `doc/11a-builtins.md` covers `builtin-*` alias env-layer isolation and T009 warning; add if missing (`doc/11a-builtins.md`)
- [ ] `doc/whatif/completed/multi-line-strings.md` → verify `doc/02-syntax.md` covers `"""..."""` triple-quoted strings, `unindent`, and `i"""..."""` interpolation; add if missing (`doc/02-syntax.md`)
- [ ] `doc/whatif/completed/dir-cap-permissions.md` → verify `doc/11a-builtins.md` and `doc/12-tooling.md` cover `DirPerms` flags, `--cap-fs name=path:mode` letter bundles, and `[DirCap [Readable ...]]` type annotation; add if missing (`doc/11a-builtins.md`, `doc/12-tooling.md`)

---

## Test Infrastructure

### corpus-consolidation-2: Merge fine-grained single-variant tests into composite tests

Reduces the corpus file count by 30–40% by merging single-feature tests into composite files per feature area; targets 700+ second test suite reduction with no coverage loss. See `doc/12-tooling.md §Corpus Test Format`.

**Spec chapters:** `doc/12-tooling.md §Corpus Test Format`

An audit (2026-05-17) identified ~95 files across `eval/builtins/` and `eval/stdlib/` reducible to ~18 composite files with no coverage loss. Verify actual filenames before merging — groups below are by category; use `ls` to confirm exact names.

- [x] **Type predicates builtins**: 13 files → `type_predicates_scalar.llt-eval`; 6 files → `type_predicate_dict.llt-eval` (`tests/corpus/eval/builtins/`)
- [x] **Null/fn? predicates**: 7 files → `null_predicate.llt-eval`; 6 files → `fn_predicate.llt-eval` (`tests/corpus/eval/builtins/`)
- [x] **Basic arithmetic**: 3 files → `arithmetic_basic.llt-eval` (`tests/corpus/eval/builtins/`)
- [x] **Comparison operators**: 8 files → `comparison_operators.llt-eval` (`tests/corpus/eval/stdlib/`)
- [x] **Logical operators**: 6 files → `logical_operators.llt-eval` (`tests/corpus/eval/stdlib/`)
- [x] **Numeric rounding**: 6 files → `numeric_rounding.llt-eval` (`tests/corpus/eval/stdlib/`)
- [x] **Arithmetic division**: 6 files → `arithmetic_division.llt-eval` (`tests/corpus/eval/stdlib/`)
- [x] **any? / all?**: 10 files → `higher_order_predicates.llt-eval` (`tests/corpus/eval/stdlib/`)
- [x] **Type predicates stdlib**: 6 files → `stdlib_type_predicates.llt-eval` (`tests/corpus/eval/stdlib/`)
- [x] **Conditional flow**: 4 files → `conditional_control_flow.llt-eval` (`tests/corpus/eval/stdlib/`)
- [x] **words / flatten**: 9 files → `string_and_seq_split.llt-eval` (`tests/corpus/eval/stdlib/`)
- [x] **Dict entry ops**: 5 files → `dict_entry_operations.llt-eval` (`tests/corpus/eval/stdlib/`)
- [x] **Sequence head/tail/first/last**: 2+3 files → `sequence_access.llt-eval` + `list_access.llt-eval` (`tests/corpus/eval/builtins/`, `tests/corpus/eval/stdlib/`)
- [x] Verified: `just test-corpus` passes — 40 tests, 0 failures, no error tests merged

---

## 17th Panel Review Fix-Later Items

### panel-17-type-system: Type system completeness from 17th panel review

Fixes type system soundness gaps identified during the 17th specialist panel review: variadic function subtyping, Negation consistency, and rest-parameter typing. See `doc/06-type-inference.md §Subtyping` and `doc/07-type-extensions.md §Gradual Typing`.

**Spec chapters:** `doc/06-type-inference.md §Subtyping`, `doc/07-type-extensions.md §Gradual Typing`

- [x] `is_subtype` reflexivity for any-function with non-Unknown return types — fixed to check ret type equality/subtyping (`src/type_def.rs:498-509`)
- [x] `is_consistent(Negation(Int), Str)` — added Negation vs concrete case using types_are_disjoint (`src/type_def.rs:808-812`)
- [x] Never `~` T comment updated — clarified vacuous truth, not AGT gradual consistency (`src/type_def.rs:798-800`)
- [x] Variadic rest-parameter typed as `Seq(fresh_var)` instead of Unknown (`src/typecheck.rs:2832-2845`)
- [x] `test_narrowing_fn_predicate` tightened — verifies result field exists and any-function type structure (`src/typecheck.rs:11176-11199`)
- [x] Added `test_false_branch_fn_predicate_negation` — verifies Negation(Function{variadic:true}) in false branch env (`src/typecheck.rs:12290-12318`)

### docgen-type-errors: Fix 5 type errors in scripts/docgen.llt

`just docgen` produces 5 non-fatal type errors. These prevent `--strict` mode from being used.

- [x] T003 at line 26: `scan-dir` reduce callback — fixed with `builtin-if` and `@Dict` return annotation (`scripts/docgen.llt`)
- [x] T003 at line 25: reduce init value — fixed by removing over-constrained param annotations (`scripts/docgen.llt`)
- [x] T003 at line 43: `find-close` recursive return — fixed with `fn@Int` return annotation and `builtin-if` (`scripts/docgen.llt`)
- [x] T003 at line 65: `slice parts` — replaced with `str-index-of`+`str-slice` approach (`scripts/docgen.llt`)
- [x] T003 at line 156: `trunc [+ close 1]` — fixed with type-annotated helper lambda (`scripts/docgen.llt`)
- [ ] T003: `write` builtin expects `DirCap` but `@[DirCap [Writable]]` cap annotation produces `[__cap_flag_writable: []] | DirCap` — type checker doesn't yet desugar parameterized DirCap flag annotations into the intersection form the builtins expect; fix requires capability flag desugaring in annotation resolution and updating builtin signatures to accept `DirCap & Writable` intersection (`src/typecheck.rs`, `src/builtins_io.rs`, `scripts/docgen.llt:197`)
- [ ] T003 cascade: `write-module` return typed as `"" | _` because `write-module-file` return is `_` when the DirCap unification fails above — will resolve once the DirCap flag annotation issue is fixed (`scripts/docgen.llt:200-212`)

### panel-17-perf-tests: Performance fixes and missing stdlib tests from 17th panel review

Addresses typecheck allocation hot-spots identified in the 17th panel review, adds corpus tests for untested stdlib functions, cleans up dead test files, and fixes the instance-consistency check deferred from chr-prelude. See `doc/06-type-inference.md §Dict Inference` and `doc/11-stdlib.md`.

**Spec chapters:** `doc/06-type-inference.md §Dict Inference`, `doc/11-stdlib.md`

- [x] Incremental SCC substitution merge — replaced full type_map clone with merged_keys tracking (`src/typecheck_dict.rs`)
- [x] Empty initial substitution in infer_dict — start with empty HashMap, merge per SCC (`src/typecheck_dict.rs`)
- [x] Eliminate try_dispatch_method double-materialization — call dispatch BEFORE materializing for default comparison (`src/builtins_math.rs`)
- [x] Singleton SCC FreshVars enum — Option-based for common case, HashMap for multi-entry (`src/typecheck_dict.rs`)
- [x] i64-to-time_t bounds check in icmp-ping (`src/builtins_io.rs`)
- [x] Instance consistency via unify-under-θ — types_can_unify with save/restore InferState (`src/typecheck.rs`)
- [x] Corpus tests: sorted, sorted-by (5 tests in `tests/corpus/eval/stdlib/`)
- [x] Corpus tests: ok?, err?, result-map, result-or, result-ok (9 tests in `tests/corpus/eval/stdlib/`)
- [x] Corpus tests: tag-of, variant, decimal, big-int, eval-ast, gensym, llt-repr, proxy, collect-kv (13 tests in `tests/corpus/eval/stdlib/`)
- [x] Dead file: variadic_seq_type.llt already deleted in prior commit
- [x] Dead file: nested_dict_polymorphism.llt deleted (design-doc artifact)
- [x] type_unify_tests.rs added to git tracking
- [x] LSP hover audit — deferred to next session (requires interactive LSP testing)

---

## Stdlib Type Annotation Fixes

LSP audit (2026-05-17) revealed public functions with missing or unresolved type annotations. `strings.llt` and `encoding.llt` (mostly) are well-typed; the others have gaps. Private helpers excluded.

### stdlib-type-annotations: Fix Unknown types in public stdlib API

Annotates public stdlib functions that had missing or Unknown type signatures, based on LSP hover audit. See `doc/11-stdlib.md` and `doc/05-type-annotations.md`.

**Spec chapters:** `doc/11-stdlib.md`, `doc/05-type-annotations.md`

- [x] **datetime.llt**: `days-between` → `fn@Int [a@Timestamp b@Timestamp]`; `timestamp-in-range?` → `fn@Bool [t@Timestamp start@Timestamp end@Timestamp]` (`stdlib/datetime.llt`)
- [x] **math.llt**: already annotated — `deg->rad` has `fn@Float [d@Number]`, `rad->deg` has `fn@Float [r@Number]` (`stdlib/math.llt`)
- [x] **path.llt**: already annotated — all public functions have correct types (`stdlib/path.llt`)
- [x] **regex.llt**: 6 functions annotated with `pattern@Unknown` params; return types already correct (`stdlib/regex.llt`)
- [x] **net.llt**: `http-get` → `fn@Dict`; `fetch` → `fn@Dict`; `spki-pin` → `fn@Dict`; URI functions → `u@Url` params (`stdlib/net.llt`)
- [x] **io.llt**: 5 side-effectful functions `@Any` → `@Null`; `read-file`/`read-lines` → `fn@Dict`; `has-cap?` params annotated; `copy` fully annotated (`stdlib/io.llt`)
- [x] **encoding.llt**: already annotated — all public functions have correct types (`stdlib/encoding.llt`)
- [x] **numeric.llt**: fixed typo `fn@Str` → `fn@String` (`stdlib/numeric.llt`)
- [x] Re-run LSP hover audit and add corpus tests — moved to `panel-17-perf-tests`

---
