# Implementation Roadmap

See DONE.md for the full history of completed sprints.

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

### unified-bindings-remove-old-syntax: Remove pre-unified-bindings param syntax from fn, type, and class

`unified-bindings-migrate` (DONE.md) checked off "Remove old param-list parsing paths" prematurely. Old-form detection survives in three places:

- **`fn` / `macro` / `defmacro`:** `parse_param_list` (src/parser.rs:798) treats `let` as optional (skips it if present, lines 820–824); called from `fn` (line 1656), `macro` (line 1869), and `defmacro` (line 1901). Also: `push_expr_to_parent` for `StackFrame::Fn` has an implied-call heuristic (lines 5250–5276) that detects all-lowercase `[a b c]` bracket as a param list.
- **`type` (TypeAlias):** `push_expr_to_parent` for `StackFrame::TypeAlias` (lines 5228–5295) has three cases — Case 1: `Expr::Dict` with auto-indexed lowercase vars (lines 5232–5248), Case 2: implied-call all-lowercase (lines 5250–5277); both are old forms. Case 3: `Expr::LetDecl` (lines 5278–5295) is the new form.
- **`class` (ClassDecl):** `push_expr_to_parent` for `StackFrame::ClassDecl` (lines 5527–5629) handles `Expr::VarRef` (lines 5541–5546), `Expr::Dict` (lines 5548–5570), and `Expr::Call { implied: true }` (lines 5572–5598) as old forms; `Expr::LetDecl` (lines 5600–5623) is the new form.

The goal is complete deletion of old paths, not a fallback parse error. `[let ...]` already works in all three contexts via `Expr::LetDecl` in `push_expr_to_parent` — no new code needed, only deletions.

- [x] Manually rewrite all non-stdlib `.llt` files using old param syntax to `[let ...]` form; known: `scripts/docgen.llt` (all fn params); audit `samples/` for others
- [x] Convert `defmacro` to deferred push_expr_to_parent pattern (receive name as VarRef, then LetDecl params) — currently the last remaining eager caller of `parse_param_list` besides `fn` and `macro`; migrate first, then delete `parse_param_list` and all three call sites
- [x] Delete `parse_param_list` entirely (`src/parser.rs:798`) and all call sites (lines 1656, 1869, 1901) once defmacro is migrated
- [x] Delete `push_expr_to_parent` `StackFrame::Fn` implied-call heuristic (lines 5250–5276)
- [x] Delete `push_expr_to_parent` `StackFrame::TypeAlias` Cases 1 and 2 (Dict and implied-call detection, lines 5228–5277); keep only the `Expr::LetDecl` branch
- [x] Delete `push_expr_to_parent` `StackFrame::ClassDecl` `Expr::VarRef`, `Expr::Dict`, and `Expr::Call { implied: true }` branches (lines 5541–5598); keep only `Expr::LetDecl` — no-param classes use `[class [let Equatable] ...]`; bare-word shorthand belongs in the `macro class` let-softening macro (macros-v2-stdlib)
- [x] Verify `just test` passes after deletions
- [x] Update DONE.md to note the `unified-bindings-migrate` checkbox was completed here (the original was premature)

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
- [x] Audit all `--- caps:` declarations in `scripts/`, `stdlib/`, and `samples/` for bare `@DirCap` and update to explicit flag lists (`scripts/`, `stdlib/`, `samples/`) — Updated `test_permissions.llt` to use `@[[all DirCap Listable Statable]]`; `scripts/docgen.llt` already has `@[DirCap [Writable]]`; no bare `@DirCap` found in `samples/` or `stdlib/`
- [ ] **KNOWN ISSUE**: CLI-level backward-compat at `src/main.rs:1321,2469,2765` — `--cap-fs NAME=PATH` without `:MODE` defaults to `DirPerms::full()`. The type-level compat described in whatif doc lines 107-109 was never implemented. Removing CLI default breaks many tests (`tests/cli_tests.rs:1636,1671,2182,2215,2248,2285,2331`). Deferred until test suite is updated to use explicit modes.
- [x] Update `doc/whatif/completed/dir-cap-permissions.md` to remove the "backward-compat transition period" note (`doc/whatif/completed/dir-cap-permissions.md:107-109`)

---

## Higher-Kinded Types

### hkt-do-inferred-fix: Implement inferred [do] monad form (divergence fix)

**Whatif:** `hkt-monads`
**Spec chapters:** `doc/whatif/completed/hkt-monads.md §[do] Inference`

DONE.md `hkt-do-macro-inferred` has all tasks `[x]` but the inferred form is not implemented. `stdlib/macros.llt:358-363` currently emits `error "inferred [do] not yet supported"` and the `%do-infer` sentinel does not exist anywhere in `src/`. The `expected_return: Option<Type>` field was correctly added to `InferState` (`src/type_infer.rs:151`). This sprint completes the implementation.

- [ ] In `stdlib/macros.llt` `do` macro: replace the inferred-form error branch (currently at line 358-363) with emission of `[do %do-infer steps...]` — emit a `VarRef("%do-infer")` as the monad AST node passed to `do-fold`; the runtime never sees this sentinel — the type checker substitutes it before eval (`stdlib/macros.llt`)
- [ ] In `src/typecheck.rs` `infer_expr` for `Expr::Call` matching `[do %do-infer ...]`: detect when monad arg is `VarRef("%do-infer")`; resolve monad via rule 1: `state.expected_return` unified against `App(m, _)` for a registered Monad class; if rule 1 fails, rule 2: first binding RHS type `App(m, a)` for a known Monad instance; if both fail, emit `TypeError` "cannot infer monad — add explicit monad argument or annotate return type" (`src/typecheck.rs`)
- [ ] Substitute resolved monad name into the desugared `[monad.bind ...]` chain before returning from typecheck — the evaluator must see a concrete monad dict name, not `%do-infer` (`src/typecheck.rs`)
- [ ] Corpus test: `tests/corpus/eval/stdlib/hkt_do_inferred_result.llt-eval` — `fetch-result: [fn@[ok: Int err: Str] [] [do [x: [ok: 42]] [ok: x]]]` — expect inferred monad = MonadResult, output `Dict([ok: Int(42)])` (`tests/corpus/eval/stdlib/`)
- [ ] Corpus test: `tests/corpus/eval/stdlib/hkt_do_inferred_first_binding.llt-eval` — `[do [x: [ok: 1]] [ok: [+ x 1]]]` without return annotation — infer from first binding `App(Result, Int)` (`tests/corpus/eval/stdlib/`)
- [ ] Corpus test: `tests/corpus/eval/errors/hkt_do_inferred_unresolvable.llt-eval` — `[do [x: 42] x]` where `42` is `Int` not a monadic value — expect `TypeError` "cannot infer monad" (`tests/corpus/eval/errors/`)
- [ ] Corpus test: `tests/corpus/eval/stdlib/hkt_do_inferred_maybe.llt-eval` — `lookup: [fn@[or [Some Int] [None]] [] [do [x: [Some 42]] [Some [+ x 1]]]]` — expect inferred monad = MonadMaybe (`tests/corpus/eval/stdlib/`)
- [ ] Update `doc/06-type-inference.md §[do] Inference` to remove any implementation-status notes about inferred form being unavailable (`doc/06-type-inference.md`)

---

## Primitive Privacy

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

### materialize-rename: Rename `eval`→`deep-materialize` and `force`→`materialize`

Both builtins are kept and renamed to accurate names that reflect what they do. The Rust `deep_materialize` function already exists with the right name; the user-callable tinct builtins should match. `materialize` (WHNF) is the common case with the shorter name; `deep-materialize` is the thorough variant. Both remain available to user code — making Rust materialization primitives accessible for novel uses.

- [ ] Rename `builtin_eval` (`src/builtins_meta.rs:56`) and its registration in `standard_builtins` from `"eval"` to `"deep-materialize"`
- [ ] Rename `builtin_force` and its registration from `"force"` to `"materialize"`
- [ ] Update prelude.llt if either is re-exported under the old name
- [ ] Update the 2 corpus test files that reference `eval` directly (`tests/corpus/eval/builtins/eval.llt-eval`, `control_flow.llt-eval`)
- [ ] Verify `just test` passes

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

---

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class: `∀f a. Filterable f ⇒ (a → Bool) → f a → f a`; compare `Mappable` extension vs separate class using `Data.Witherable` as precedent; include instance examples for `Seq` and `Dict` (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint in TODO.md for `from-json @Schema` schema-directed typed parse (`doc/whatif/schema-directed-from-json.md`)

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

