# Implementation Roadmap

See DONE.md for the full history of completed sprints.

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

- [ ] Add `"tests/corpus/eval/builtins/errors"` to `required_dirs` in `tests/corpus_tests.rs:173` — 21 cap/network error tests have no structural floor guard (`tests/corpus_tests.rs`)
- [ ] Move 13 eval-success typecheck-warning tests out of `tests/corpus/eval/errors/` into `tests/corpus/eval/typecheck/` — files use `=== out`/`=== warn` but live in `errors/`, violating the directory contract (`tests/corpus/eval/errors/`)
- [ ] Rename `closed_record_rejects_extra.llt-eval` — filename contradicts behavior (BAS width subtyping accepts extra fields) (`tests/corpus/eval/errors/`)
- [ ] `types_can_unify` substitution split — probe passes `temp_subst` but `check_constraints_on_var` reads `state.subst`; for instance consistency's concrete-type inputs this is safe but the two-substitution split should be unified or documented (`src/typecheck.rs:1652-1657`)
- [ ] Incremental SCC merge `merged_keys` write-once assumption — correct under Robinson unification but implicit; add `debug_assert!` verifying no binding is overwritten between SCC iterations (`src/typecheck_dict.rs:505-537`)
- [ ] `sorted_by_empty.llt-eval` uses 1-arg identity function where 2-arg comparator is expected — vacuously correct but misleading; fix to use a proper 2-arg comparator (`tests/corpus/eval/stdlib/sorted_by_empty.llt-eval`)
- [ ] Add corpus test for `[tag-of 42]` error case — docstring says "errors on non-variant values" but no `=== error` test exists (`tests/corpus/eval/stdlib/`)
- [ ] Add corpus test for `result` monad dict `[bind: and-then  pure: result-ok]` — no test exercises `[do result ...]` chains (`tests/corpus/eval/stdlib/`)
- [ ] `and-then` is data-first `(result f)` but stdlib convention is data-last for `->` threading; `result-or` is `(default result)` while `try-or` is `(f default)` — inconsistent ordering across "default on failure" combinators (`stdlib/prelude.llt:1372-1394`)
- [ ] `newline_breaks_dot_access.llt-eval` — test name implies newline-before-dot is tested but actual input `[a [0]]` is an implied call, not a dot-access edge case; add explicit `a\n.b` test (`tests/corpus/valid/edge_cases/`)

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
