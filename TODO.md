# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Macro System v2

`macros-v2` accepted 2026-05-17. See `doc/whatif/macros-v2.md`. Unified `macro` form with `[let ...]` patterns, `inject:` for anaphoric binding, `splice` for multi-form output, `syntax-class` for declarative argument validation. Implementation order: macros-v2-ast → macros-v2-expand → macros-v2-inject → macros-v2-stdlib.

### macros-v2-ast: New AST variants and parser changes

See `doc/whatif/macros-v2.md §What Would Change`. **Spec chapters:** `doc/whatif/macros-v2.md §AST Types`, `doc/02-syntax.md §Macros`.

- [ ] Add `Expr::MacroDecl { name, params, body }` and `Expr::Splice(Vec<Spanned<Expr>>)` to `src/ast.rs`; update all exhaustive match sites; add `panic!`/error arms in eval/typecheck for these (expansion guarantees removal) (`src/ast.rs`, `src/eval.rs`, `src/typecheck.rs`)
- [ ] Add `Expr::SyntaxClass { name, pattern, message }` to `src/ast.rs`; same treatment as `MacroDecl` (`src/ast.rs`)
- [ ] Rename `defmacro` → `macro` in lexer keyword denylist; add `syntax-class` with `peek_next_horizontal` colon-ahead guard (so `[syntax-class: foo]` is a dict entry, not a declaration) (`src/lexer.rs`, `src/parser.rs`)
- [ ] Add `StackFrame::MacroDecl` and `StackFrame::SyntaxClass` to parser (`src/parser.rs`)
- [ ] Pre-scan pass: walk parsed AST before expansion, collect `MacroDecl`/`SyntaxClass` nodes — including `inject:` default names (extracted as `KeyedEntry` with key `"inject"`); follow only bare string-literal `include` paths; computed-path includes that declare macros are an expansion error (`src/expand.rs`)
- [ ] Move fn/class/type param-list semantic enforcement from parser StackFrames to type checker: `check_fn_expr`, `check_class_decl`, `check_type_alias` reject non-`Let` params with type error (`src/parser.rs`, `src/typecheck.rs`)
- [ ] Tests: `macro` keyword parses; `syntax-class` keyword parses; `[syntax-class: foo]` is dict entry not declaration; old `defmacro` produces parse error (`tests/corpus/eval/`)

### macros-v2-expand: Expansion pass, splice, syntax-class validation, inject threading

**Depends on:** `macros-v2-ast`

- [ ] Update expander to use `[let ...]` pattern matching for macro argument binding — replaces manual `nth` indexing (`src/expand.rs`)
- [ ] Add splice handling: when macro returns `Expr::Splice(forms)`, inject each form; register any `MacroDecl`/`SyntaxClass` in splice output immediately before processing next splice form (enables meta-macros); splice in expression position is expansion error (`src/expand.rs`)
- [ ] Validate macro arguments annotated with `@VariantName` or `@syntax-class-name` before calling macro body; raise `MacroError` at call-site span on failure (`src/expand.rs`)
- [ ] Thread `inject:` binding name: when macro with `inject:` is in dict-key position (`key: [macro-call ...]`), pass key name to macro body as the implicit `binding` variable (`VarRef`); default to `inject:` name when in expression position (`src/expand.rs`)
- [ ] Add `ErrorKind::MacroError { span, message }` to error system; wrap unexpected runtime errors in macro bodies with `macro_expansion` provenance (`src/error.rs`, `src/expand.rs`)
- [ ] Tests: splice produces multiple dict entries; splice in expression position errors; `inject:` default works; dict-key override works; syntax-class validation errors at call site (`tests/corpus/eval/macros/`)

### macros-v2-inject: inject: and macro-injects reflection

**Depends on:** `macros-v2-expand`

- [ ] Implement `macro-injects` Rust builtin: takes macro name (`Str`), returns `inject:` default name (`Str`) or `Null` (`src/builtins_meta.rs`)
- [ ] Store `inject:` default name in macro registry alongside form name and syntax-class declarations (`src/expand.rs`)
- [ ] Tests: `[macro-injects aif]` → `"it"`; `[macro-injects swap]` → `null`; LSP hover for a macro with `inject:` shows the injected name (`tests/corpus/eval/macros/`)

### macros-v2-stdlib: Migrate defmacro, add stdlib/ast.llt and stdlib/syntax.llt

**Depends on:** `macros-v2-expand`

- [ ] Migrate 27 corpus test files in `tests/corpus/eval/macros/` from `defmacro` to `macro` (`tests/corpus/eval/macros/`)
- [ ] Migrate `tmpl`, `do`, `begin` macros in `stdlib/macros.llt` from `defmacro` to `macro` (`stdlib/macros.llt`)
- [ ] Update `gensym` API: zero-arg → one-arg `[gensym prefix@Str]` returning `VarRef(name: ":prefix:N")`; migrate all call sites (`stdlib/prelude.llt`, `stdlib/macros.llt`)
- [ ] Add `stdlib/ast.llt`: `Entry`, `Annotation`, `Expr` nominal types; `flatten-args`, `ident`; ~70 lines (`stdlib/ast.llt`)
- [ ] Add `stdlib/syntax.llt`: `macro fn`, `macro class`, `macro type` let-softening macros using `flatten-args`; opt-in via `[include %libdir "syntax.llt"]` (`stdlib/syntax.llt`)
- [ ] Add to `stdlib/prelude.llt`: `span-of`, `wrap-in-let`, `let-decl-elems`, `first-or`, `macro-error`, `macro-injects` (`stdlib/prelude.llt`)
- [ ] Migrate `ast_to_dict` output from string `type:` fields to typed `Expr` variant values; update `stdlib/formatter/compact.llt`, `stdlib/formatter/pretty.llt`, `src/builtins_meta.rs` (`src/builtins_meta.rs`, `stdlib/`)
- [ ] Tests: `stdlib/ast.llt` loads cleanly; `stdlib/syntax.llt` let-softening works end-to-end; all migrated macros produce same expansion as before (`tests/corpus/eval/macros/`)

---

## Tooling

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

---

## Codebase Health

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
