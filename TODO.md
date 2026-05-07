# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Infrastructure

### `rust-modernize`

Adopt new Rust 1.87–1.95 language and stdlib features available at the new MSRV. These are independent follow-on refactors; each is a quality-of-life improvement, not a correctness fix.

- [ ] **Let-chains** (stable 1.88, edition 2021): replace `if let Some(x) = foo { if cond { … } }` patterns with `if let Some(x) = foo && cond { … }` throughout `src/eval.rs` and `src/typecheck.rs` (`src/eval.rs`, `src/typecheck.rs`)
- [ ] **`Result::flatten()`** (stable 1.89): replace `match { Ok(Ok(x)) => …, Ok(Err(e)) => … }` and `.map(…).unwrap_or_else(…)` double-result patterns in `src/eval.rs` and `src/builtins.rs` (`src/eval.rs`, `src/builtins.rs`)
- [ ] **`File::lock()` / `try_lock()`** (stable 1.89): add advisory file locking in `builtin_write_atomic` in `src/builtins_io.rs` as an optional stronger-exclusion hint alongside the existing temp+rename strategy (`src/builtins_io.rs`)
- [ ] **`str::ceil_char_boundary` / `floor_char_boundary`** (stable 1.91): replace manual `.char_indices()` + `.next_back()` UTF-8 boundary calculations in `src/lexer.rs` and `src/parser.rs` (`src/lexer.rs`, `src/parser.rs`)
- [ ] **`Path::file_prefix()`** (stable 1.91): replace manual `file_stem()` + extension-stripping in `find_libdir_path()` in `src/main.rs` (`src/main.rs`)
- [ ] **`HashMap::extract_if`** (stable 1.88): replace `retain`+`remove` two-pass patterns where they appear in dict evaluation and builtin helpers (`src/eval_dict.rs`, `src/builtins_dict.rs`)
- [ ] **`Peekable::next_if_map()`** (stable 1.94): replace peek-then-advance patterns in `src/lexer.rs` and `src/parser.rs` (`src/lexer.rs`, `src/parser.rs`)
- [ ] **`cfg_select!` macro** (stable 1.95): simplify the `#[cfg(target_arch)]` chain in `setup_seccomp()` in `src/main.rs` (`src/main.rs`)
- [ ] **`OsStr::display()`** (stable 1.87): replace `.to_string_lossy()` calls on path components in error messages in `src/main.rs` (`src/main.rs`)
- [ ] **`LazyLock` for stdlib env cache**: when implementing `typecheck-import-env`, prefer `LazyLock<Rc<TypeEnv>>` over `OnceLock` for the prelude env cache in `src/imports.rs` if the closure form is cleaner; note that `LazyLock::get()` / `force_mut()` are stable at 1.94 (`src/imports.rs`)

## Type Checking Infrastructure

### `typecheck-import-env`

Seed the type checker with a fully-resolved import environment before running inference. Currently `typecheck_file()` starts from `TypeEnv::with_builtins()` only — it has no knowledge of the prelude or user `$include` files. This causes ~250 corpus tests to carry stale `=== warn: undefined variable` annotations for prelude functions, and means type errors in included files are invisible until eval time.

**Design: shared `src/imports.rs` module — no LSP-specific plumbing**

All import resolution logic lives in a new `src/imports.rs`. The LSP replaces its bespoke `PreludeIndex`/`with_prelude_types` machinery with calls into this shared module. The only legitimately LSP-specific code that remains is `IncludeGraph` invalidation on file-change events.

**Prelude environment:**

- [ ] Add `src/imports.rs` with `pub fn build_prelude_env() -> Rc<TypeEnv>`: parse the embedded `include_str!("../stdlib/prelude.llt")` source, run `expand_macros` + `desugar_file`, then `typecheck_file_with_types_and_env` seeded with `TypeEnv::with_builtins()`; walk the resulting `TypeMap` to extract top-level binding names and their inferred types; extend `TypeEnv::with_builtins()` with those bindings; cache the result in a `std::sync::OnceLock<Rc<TypeEnv>>` so it is built once per process (`src/imports.rs`)
- [ ] `build_prelude_env()` must tolerate prelude-internal type warnings without failing — it returns the best available env even if some prelude functions have unresolved type vars; log nothing to stderr (callers don't want noise) (`src/imports.rs`)

**Include path collection (moved from LSP):**

- [ ] `pub fn collect_include_paths(file: &File) -> Vec<(Span, String)>`: walk AST for `Call { func: VarRef("include"), args: [Str(path)] }` — extracts all statically-known include paths with their spans; dynamic includes (computed paths) are silently skipped (`src/imports.rs`)

**Include resolution:**

- [ ] `pub fn resolve_includes(paths: &[(Span, String)], base_dir: &Path, base_env: Rc<TypeEnv>, visited: &mut HashSet<PathBuf>) -> Rc<TypeEnv>`: for each path, resolve relative to `base_dir`; skip if already in `visited` (cycle detection); read file via `std::fs::read_to_string` (plain OS read, no eval involved); parse + `expand_macros` + `desugar_file`; call `typecheck_file_with_types_and_env` with the current accumulated env; extract top-level bindings from the resulting `TypeMap`; extend env; insert path into `visited`; recurse for the included file's own includes; depth cap 16 (`src/imports.rs`)
- [ ] `resolve_includes` returns `base_env` unchanged on any IO or parse failure (best-effort; type errors in included files surface at eval time as before) (`src/imports.rs`)

**Unified entry point:**

- [ ] `pub fn build_type_env(file: &File, base_dir: Option<&Path>) -> Rc<TypeEnv>`: compose the above — start from `build_prelude_env()`, then if `base_dir` is `Some`, call `collect_include_paths(file)` + `resolve_includes`; return the fully-seeded env. When `base_dir` is `None` (source-only callers like `typecheck_source`), only the prelude is seeded — include resolution requires a filesystem path (`src/imports.rs`)

**Wire into the eval/typecheck pipeline:**

- [ ] Update `typecheck_source(input)` in `src/lib.rs` to call `imports::build_type_env(&file.node, None)` and pass the result to `typecheck_file_with_types_and_env` instead of starting from bare `TypeEnv::with_builtins()` (`src/lib.rs`)
- [ ] Update `typecheck_file()` in `src/typecheck.rs` to call `imports::build_prelude_env()` as its base env instead of `TypeEnv::with_builtins()` — this ensures all callers of the lower-level function also get the prelude (`src/typecheck.rs`)
- [ ] Update `tinct eval <file>` path in `src/main.rs`: derive `base_dir` from the file path and pass it when building the type env, enabling include resolution for file-based eval; `--stdin` mode passes `None` (`src/main.rs`)
- [ ] Update `typecheck_file_with_types()` (the LSP's current zero-arg entry) to use `build_prelude_env()` as its base — this is a one-line change since it delegates to `typecheck_file_with_types_and_env` (`src/typecheck.rs`)

**LSP migration — remove divergent plumbing:**

- [ ] Delete `build_prelude_index()` from `src/lsp/document.rs`; delete `PreludeIndex` struct, `PreludeIndexInner`, `PreludeIndex::empty()`, `PreludeIndex::name_to_key_span()`, `PreludeIndex::type_map()`, `find_stdlib_prelude_path()` (`src/lsp/document.rs`)
- [ ] Delete `TypeEnv::with_prelude_types()` from `src/types.rs` — no longer needed once `build_type_env` is the seeding mechanism (`src/types.rs`)
- [ ] Update `DocumentState::new`: remove `prelude_index: &PreludeIndex` parameter; instead call `imports::build_type_env(&file.node, Some(base_dir))` to get the seeded env; pass to `typecheck_file_with_types_and_env` (`src/lsp/document.rs`)
- [ ] Remove `prelude_index` field from `DocumentStore`; remove `build_prelude_index()` call from `DocumentStore::new()`; update all `DocumentState::new()` call sites to drop the `prelude_index` argument (`src/lsp/document.rs`)
- [ ] Replace LSP-local `collect_include_paths` usage with `imports::collect_include_paths` — the `IncludeGraph` invalidation logic in `index_document_includes` and `reindex_document` remains LSP-specific (file watching is legitimately LSP-only) but the path-extraction step is shared (`src/lsp/document.rs`)
- [ ] Update LSP `index_document_file` (the include graph crawler): it already does read+parse+`DocumentState::new` recursively; this is now redundant with `resolve_includes` for type-env purposes, but the LSP still needs it for the `IncludeGraph` (hover, go-to-definition over includes). Keep the graph crawler, but have it call `imports::collect_include_paths` instead of its own extraction logic (`src/lsp/document.rs`)

**Corpus cleanup:**

- [ ] Script: `grep -rl 'undefined variable:' tests/corpus/ | xargs grep -l '=== warn'` — collect the ~250 affected files; for each, if the warned name is a prelude function (not a pattern-match variable), remove the `=== warn` section entirely; verify with `cargo test` that the prelude-function warnings are gone (`tests/corpus/`)
- [ ] Keep `=== warn` sections for pattern-match bound variables (`h`, `v`, `a`, `b`, `n`, etc.) — those are a separate type checker scoping bug, not fixed by this sprint

**Tests:**

- [ ] Unit tests in `src/imports.rs`: `build_prelude_env()` returns an env where `map`, `filter`, `and`, `or`, `flatten`, `zip` are all resolvable; `collect_include_paths` finds `[call $include "foo.llt"]` and returns `"foo.llt"`; `resolve_includes` with a missing file returns the base env unchanged (`src/imports.rs`)
- [ ] Integration test in `src/lib.rs`: `typecheck_source("[call $map [fn [x] x] [1 2 3]]")` returns `Ok(())` (no undefined-variable warning for `map`) (`src/lib.rs`)
- [ ] Corpus suite passes with zero `undefined variable` warnings for prelude functions after the cleanup pass (`tests/corpus/`)

## Phase D: Advanced Typing

### `type-classes-full`

See doc/06-type-inference.md §Type Classes, doc/07-type-extensions.md. **Depends on:** `type-classes-constrained` (B4), `param-type-aliases` (B3), let-generalization complete. **Note:** multi-parameter type classes and functional dependencies are explicitly out of scope for this sprint.

**Parsing and AST:**
- [ ] Verify `[class [ClassName params] superclasses... methods...]` parser against spec syntax; add `class` and `instance` to keyword denylist if not already present (`src/lexer.rs`, `src/parser.rs`)
- [ ] Verify `[instance [ClassName Type] methods...]` parser; method entries may be signature-only or signature+body (default implementations) (`src/parser.rs`)
- [ ] Formatter: round-trip `Expr::ClassDecl` and `Expr::InstanceDecl` without losing method bodies (`src/formatter.rs`)

**Kind system:**
- [ ] `Kind::Var(u32)` variant for kind variables; `KindState` analogous to `InferState` for kind unification (`src/types.rs`)
- [ ] `unify_kind(k1: &Kind, k2: &Kind, state: &mut KindState) -> Result<(), KindError>` — Robinson unification on `Kind` terms (`src/types.rs`)
- [ ] Kind inference for class type parameters from method signatures: infer kind of `f` in `Mappable f` from how `f` is used in `f a` in method types (`src/typecheck.rs`)
- [ ] Kind checking at instance declaration: instance type's kind must match the class parameter's inferred kind; `[instance [Mappable Int] ...]` is a kind error (Int has kind `*`, Mappable expects `* → *`) (`src/typecheck.rs`)
- [ ] Kind defaulting: unresolved kind variables default to `Kind::Type` after class declaration is processed (Jones 1993, §4) (`src/typecheck.rs`)

**Class/instance registration:**
- [ ] `ClassEnv` population from `Expr::ClassDecl`: register class with methods (signature + optional default body) and superclasses; compute superclass transitive closure at registration time (`src/typecheck.rs`)
- [ ] `InstanceEnv` population from `Expr::InstanceDecl`: replace string-key lookup with unification-based instance resolution — attempt `unify(instance_head_type, target_type)` to select matching instance (Hall et al. 1996, §3.2) (`src/typecheck.rs`, `src/types.rs`)
- [ ] Instance coherence: reject overlapping instances for the same class+type pair globally — `InstanceEnv::insert` must be global, not dict-scoped (`src/typecheck.rs`)
- [ ] Scoping: class declarations are dict-scoped (visible in the dict and children); instance declarations are globally registered in `InstanceEnv` (coherence requires global uniqueness) (`src/typecheck.rs`)

**Dictionary construction and passing:**
- [ ] Dictionary value construction: `Value::Dict` with method name as key, eagerly materialized at instance registration time; superclass dictionary embedded as a sub-dict under the superclass name (`src/eval.rs`)
- [ ] Superclass dictionary embedding: `Comparable` dict contains `Equatable` sub-dict under key `"equatable"`; `entailment(context, target)` extracts sub-dict when only a superclass dict is available (`src/eval.rs`)
- [ ] Dictionary threading in evaluator: constrained function calls receive implicit dictionary argument; `eval` for call nodes looks up the appropriate dict from `InstanceEnv` and prepends it to args (`src/eval.rs`)
- [ ] Ensure dictionary values are materialized (not thunked) when passed to constrained functions — dicts must not be re-forced on every method call (`src/eval.rs`)
- [ ] Default method implementations: at instance construction time, methods absent from the instance declaration are filled in from `ClassDecl.default_methods` before building the dict (`src/eval.rs`)

**Type inference integration:**
- [ ] Constraint entailment: `entails(context: &[Constraint], target: &Constraint) -> bool` using superclass transitive closure — `Comparable a` entails `Equatable a` if `Equatable` is a superclass of `Comparable` (`src/typecheck.rs`)
- [ ] Constraint simplification during generalization: remove redundant constraints (if `Comparable a` is present, remove `Equatable a`) (`src/typecheck.rs`)
- [ ] Instance resolution during constraint solving: when a type variable is unified with a concrete type, resolve pending class constraints against `InstanceEnv`; error if no matching instance (`src/typecheck.rs`)
- [ ] Integration with B4 constrained type variables: B4's hardcoded instance sets (`Equatable`, `Numeric`, etc.) become backed by actual `ClassEnv`/`InstanceEnv` entries registered at startup (`src/typecheck.rs`, `src/builtins.rs`)

**Testing (25+ tests):**
- [ ] Tests: class declaration parsing/round-trip; instance declaration parsing; dictionary construction and method dispatch; superclass hierarchy and entailment; kind checking at instance sites; constraint propagation through let-generalization; missing instance error; kind mismatch error; overlapping instance error; integration with B4 constrained vars; higher-kinded `Mappable Seq` instance; default method implementations (`tests/corpus/eval/type_system/`)

**Spec:**
- [ ] Write `doc/06-type-inference.md` §Type Classes with formal rules: constraint generation, entailment checking, dictionary elaboration, instance resolution, superclass extraction (`doc/06-type-inference.md`)

---

## Standard Library

### `stdlib-modernize`

Modernize all 14 stdlib `.llt` files to leverage the typing cluster facilities implemented 2026-05-07. Three orthogonal improvements applied uniformly: (1) **encapsulation** — internal helpers scoped to a private first-document, public API in the final document; (2) **pattern matching** — replace `type-of` string comparisons and `try`-result `[first [keys r]]` checks with `[match ...]`; (3) **full type annotations** — every public function carries `fn@ReturnType` and every parameter carries `@Type`. **Applies to:** `prelude.llt`, `numeric.llt`, `formatter/compact.llt`, `formatter/pretty.llt`, `out/json.llt`, `out/json-pretty.llt`, `out/yaml.llt`, `out/csv.llt`, `out/toml.llt`, `out/env.llt`, `out/raw.llt`, `in/json.llt`, `io.llt`, `net.llt`.

**Architecture: public/private encapsulation pattern**

`eval_document` evaluates each expression in a document sequentially: intermediate dicts are materialized and their entries become bindings in a child environment for the next expression; only the **last expression's value is returned**. This gives true encapsulation with no special syntax — internal helpers are simply a separate earlier dict in the same document:

```tinct
# Internal helpers — in scope for the public dict, but NOT returned
[
    make-entry: [fn@Dict [k v] [$k: v]]
    any?-impl:  [fn@Bool [pred@Fn xs@Dict ks i@Int len@Int] ...]
    # ... all -impl / -step / -check helpers
]
# Public API — last expression, so this is the only value returned/exported
[
    any?: [fn@Bool [pred@Fn xs@Dict]
        [builtin-if [seq? xs]
            [error "any?: expected Dict, got Seq"]
            [any?-impl pred xs [keys xs] 0 [length xs]]]]
    # ... all public functions; reference helpers by plain name
]
```

Helpers are reachable by plain name inside the public dict (via the child env chain). They do not appear in the returned dict and are not exported. No `%.fn` or `_impl.fn` prefixes needed.

**Files with no internal helpers** (`numeric.llt` and most `out/` files) remain as a single dict — no split needed.

**Pattern: `try` result dispatch**

Three sites in `prelude.llt` use `[builtin-eq [first [keys result]] "ok"]` to inspect `try` outcomes. Replace all with structural pattern matching:

```tinct
# Before
try-or-impl: [fn [try-result@Dict default]
    [builtin-if [builtin-eq [first [keys try-result]] "ok"]
        try-result.ok
        default]]

# After
try-or-impl: [fn [try-result@Dict default]
    [match try-result
        [ok: v]  v
        [err: _] default]]
```

Apply to: `has?-impl`, `try-or-impl`, `find-deep-try-check`.

**Pattern: `type-of` string comparison → predicate or `[match]`**

Replace `[builtin-eq [type-of x] "Dict"]` with `[dict? x]` where only a boolean is needed, or with `[match x Dict ... _ ...]` where the dispatch selects between typed branches:

```tinct
# Before (walk)
walk: [fn [f@Fn xs]
    [builtin-if [builtin-eq [type-of xs] "Dict"]
        [f [walk-dict f xs]]
        [f xs]]]

# After
walk: [fn [f@Fn xs]
    [match xs
        Dict [f [%.walk-dict f xs]]
        _    [f xs]]]
```

Apply to: `walk`, `flatten-step`, `deep-merge-step`, `find-deep-check`.

**Pattern: string-literal dispatch in formatters**

The formatter files use `[cond [[= [get "type" node] "literal"] ...] ...]` chains. Replace with `[match [get "type" node] "literal" ... "var" ... _ [error ...]]`:

```tinct
# Before (compact.llt format-node)
format-node: [fn [node]
  [cond [
    [[= [get "type" node] "literal"] [format-literal node]]
    [[= [get "type" node] "var"]     [get "name" node]]
    ...
  ]]]

# After
format-node: [fn [node]
  [match [get "type" node]
    "literal"        [format-literal node]
    "var"            [get "name" node]
    ...
    _                [error [str "unknown node type: " [get "type" node]]]]]
```

Apply to: `format-node` and `format-literal` in `compact.llt` and analogous dispatches in `pretty.llt`.

**Tasks — `prelude.llt`:**

- [ ] Public/private split: move all `-impl`, `-step`, `-check` helpers (≈30 functions) into a first dict in the same document; move all public functions into a second (final) dict; helpers are reachable by plain name from the public dict and are not exported (`stdlib/prelude.llt`)
- [ ] `try` result pattern matching: rewrite `has?-impl`, `try-or-impl`, `find-deep-try-check` using `[match result [ok: v] ... [err: _] ...]` (`stdlib/prelude.llt`)
- [ ] `type-of` → predicate/match: rewrite `walk`, `flatten-step`, `deep-merge-step`, `find-deep-check` to use `[dict? x]` / `[match x Dict ...]` instead of `[builtin-eq [type-of x] "Dict"]` (`stdlib/prelude.llt`)
- [ ] Union type annotations for dual-dispatch parameters: add `@[Dict Seq]` to `sorted`, `sorted-by`, `zip`, `contains?`, `flat-map`, `partition`, `group-by`, `fold`, `map` (wrapper), `reduce` (wrapper) (`stdlib/prelude.llt`)
- [ ] Complete annotation pass: add missing `fn@ReturnType` and `param@Type` annotations to `find-deep` family (return type missing), `walk` (return type missing), `get-in`/`get-in-or` (return `@Any`), `zip`/`zip-seq-impl` (unannotated), `cond`/`when`/`unless` (return `@Any`) (`stdlib/prelude.llt`)
- [ ] `sign` → match: replace nested `builtin-if` chain in `sign` with a `[match ...]` expression using a literal 0 arm and guard arms for positive/negative (`stdlib/prelude.llt`)
- [ ] `doc:` annotations: add `doc: "..."` to the return-type annotation of every exported function in the second (public) dict, e.g. `fn@[type: Bool  doc: "Returns true if pred holds for any element"]` (`stdlib/prelude.llt`)

**Tasks — `numeric.llt`:**

- [ ] Add return type annotation to `to-bytes`: `fn@Str`; verify type alias entries (`UInt8`, `UInt16`, etc.) carry correct `Int` range constraints (`stdlib/numeric.llt`)

**Tasks — `formatter/compact.llt`:**

- [ ] Public/private split: move `join-strings-impl`, `map-list-impl`, `make-entry` into a first dict; public formatting functions in the final dict reference them by plain name (`stdlib/formatter/compact.llt`)
- [ ] Replace `format-node` `cond` dispatch on `node.type` string with `[match [get "type" node] "literal" ... _ [error ...]]` (`stdlib/formatter/compact.llt`)
- [ ] Replace `format-literal` `cond` dispatch on `node.kind` string with `[match [get "kind" node] "int" ... _ [error ...]]` (`stdlib/formatter/compact.llt`)
- [ ] Complete annotation pass: add `fn@Str` return types on all formatting functions; add param annotations for `node@Dict`, `entry@Dict`, `na@Dict` etc. (`stdlib/formatter/compact.llt`)

**Tasks — `formatter/pretty.llt`:**

- [ ] Read file; apply same three improvements: public/private split, string-dispatch → `[match]`, complete annotation pass (`stdlib/formatter/pretty.llt`)

**Tasks — `out/` formatters (7 files: `json`, `json-pretty`, `yaml`, `csv`, `toml`, `env`, `raw`):**

- [ ] For each file: (a) identify internal helpers; apply public/private `---` split if any exist; (b) replace any `type-of`/cond-string dispatch with `[match]`; (c) add `fn@Str` return types to all output-generating functions and `@Type` to all params (`stdlib/out/`)

**Tasks — `in/json.llt`, `io.llt`, `net.llt`:**

- [ ] For each file: public/private split, pattern match modernization, complete annotation pass (`stdlib/in/json.llt`, `stdlib/io.llt`, `stdlib/net.llt`)

**Tests and spec:**

- [ ] Run full corpus test suite after each file refactor; zero regressions required (`tests/corpus/`)
- [ ] Add one corpus test per pattern-matched `try` result site verifying the new dispatch path: `[ok: v]` arm and `[err: e]` arm both exercised (`tests/corpus/eval/stdlib/`)
- [ ] Update `doc/11-stdlib.md` type signature table to reflect new union-type annotations (`@[Dict Seq]` on dual-dispatch functions) and any newly-annotated functions (`doc/11-stdlib.md`)

## Testing & Quality

### `corpus-cleanup`: Corpus Test Cleanup

Audit findings from 2026-05-07. One category of test failures (macro `=== warn` stale expectations) and two categories of dead annotation (valid/ warn sections, errors/ missing `=== error`).

**Failures (test_eval_corpus FAILED — 7 tests):**

The macro expansion pass now runs before typechecking (`typecheck_source` calls `expand::expand_macros` first). DefMacro nodes are fully expanded before the typechecker sees them, so the "defmacro should be removed by expansion pass before typechecking" warnings and the "undefined variable: <macro-name>" warnings are no longer produced. The `=== warn` sections in these 7 files are stale expectations from before the pipeline fix.

Fix: remove the `=== warn` sections from all 7 files (typecheck now cleanly handles expanded macro code).

- [x] Fix test: remove stale `=== warn` section from `tests/corpus/eval/macros/defmacro_simple.llt-eval`
- [x] Fix test: remove stale `=== warn` section from `tests/corpus/eval/macros/defmacro_unless.llt-eval`
- [x] Fix test: remove stale `=== warn` section from `tests/corpus/eval/macros/hygiene_no_capture.llt-eval`
- [x] Fix test: remove stale `=== warn` section from `tests/corpus/eval/macros/macro_integration_full.llt-eval`
- [x] Fix test: remove stale `=== warn` section from `tests/corpus/eval/macros/macro_with_underscore.llt-eval`
- [x] Fix test: remove stale `=== warn` section from `tests/corpus/eval/macros/nested_expansion.llt-eval`
- [x] Fix test: remove stale `=== warn` section from `tests/corpus/eval/macros/scope_isolation.llt-eval`

**Unenforced `=== warn` annotations in `tests/corpus/valid/` (29 files):**

`test_valid_corpus` only runs `parse()` / `parse_expression()` — it never calls `typecheck_source`. The `=== warn` sections in valid/ files are currently ignored. Fixed by `corpus-section-consistency` below, which extends the valid/ runner to enforce them.

**`=== error` section — clarification:**

The `strict-mode` sprint wired up `=== error` in `test_eval_corpus()`. The section is now live. `tests/corpus/eval/errors/` files historically use `=== out` for error substrings (established convention predating labeled sections); migration to `=== error` is tracked in `corpus-section-consistency` below.

- [x] Document `=== error` live behavior in `doc/12-tooling.md §Corpus Test Format`; clarify coexistence with `eval/errors/` convention of using `=== out` (`doc/12-tooling.md`)

**Deferred cleanup from strict-mode sprint:**

- [x] Audit unit test helpers in `src/lib.rs` and `src/typecheck.rs` that call `eval` or `typecheck` directly on hand-constructed ASTs; add `expand_macros` calls where needed to preserve the pipeline invariant (`src/lib.rs`, `src/typecheck.rs`)
- [ ] Refactor `run_fmt` to parse once and pass the AST to both the formatter and the type checker when `--strict` is active (`src/main.rs`)

### `corpus-section-consistency`

Enforce one semantic per labeled section and unify all corpus runners behind a single shared enforcement engine. The canonical contract after this sprint:

- `=== out` — eval succeeds; output matches substring. Never used for error messages.
- `=== error` — eval fails; message matches substring. Must be non-empty. Every `=== error` file must also produce a runtime error message containing `[EXXX]`.
- `=== warn` — type warnings match substring. Orthogonal to `out`/`error`. Absent section asserts zero warnings. Enforced uniformly by the shared runner regardless of pipeline.

The `valid/` and `eval/` directory distinction is load-bearing: `valid/` = parse-only tests (some files reference undefined vars that parse legally but would fail eval); `eval/` = full evaluation tests. The runner is unified; the pipeline passed to it differs.

**Shared runner infrastructure (`tests/test_helpers.rs`):**

- [ ] Extract `split_test_file` into `tests/test_helpers.rs`; reconcile `String`-vs-`&str` return type (favor owned `String`); re-export for use by `corpus_tests.rs` and `lsp_corpus_tests.rs` (`tests/test_helpers.rs`)
- [ ] Add `run_corpus_dir(dir: &Path, pipeline: impl Fn(&TestFile) -> CorpusOutcome) -> Vec<Failure>` to `tests/test_helpers.rs`; `CorpusOutcome { output: Option<String>, warnings: Vec<String>, error: Option<String> }`; the function handles: find files, split, mutual-exclusivity guard (`=== out` + `=== error` rejected), call pipeline, compare each channel against expectations, collect failures (`tests/test_helpers.rs`)
- [ ] Add runner guard inside `run_corpus_dir`: `=== error` section must be non-empty; `=== warn` section must be non-empty; blank labeled sections are a test-file authoring error (`tests/test_helpers.rs`)
- [ ] Add `[EXXX]` runtime check inside `run_corpus_dir`: when `CorpusOutcome.error` is `Some`, assert it contains an `[EXXX]` prefix — preserves the check currently in `test_eval_error_corpus:721` and applies it universally (`tests/test_helpers.rs`)

**Rewrite `test_eval_corpus` using shared runner:**

- [ ] Replace the inline enforcement logic in `test_eval_corpus` with a call to `run_corpus_dir`; the eval pipeline closure calls `eval_source_with_config` and `typecheck_source`, returning `CorpusOutcome { output, warnings, error }` (`tests/corpus_tests.rs`)
- [ ] Remove the `errors_dir` exclusion filter — `eval/errors/` runs through the unified runner once its files use `=== error` (`tests/corpus_tests.rs`)
- [ ] Delete `test_eval_error_corpus` and `test_eval_error_corpus_has_error_codes` — fully superseded (`tests/corpus_tests.rs`)

**Rewrite `test_valid_corpus` using shared runner:**

- [ ] Replace `test_valid_corpus` with a call to `run_corpus_dir` using a parse-only pipeline: `parse()` + `typecheck_source()`, returning `CorpusOutcome { output: None, warnings, error: None }`; parse failure maps to a `Failure` directly; `=== out` and `=== error` sections in `valid/` files are rejected as authoring errors (`tests/corpus_tests.rs`)
- [ ] This gives `valid/` files full `=== warn` enforcement for free — the shared runner applies the same warn-channel logic regardless of pipeline

**`=== error` migration — `eval/errors/` (113 files):**

- [ ] Rename `=== out` → `=== error` in all 113 files under `tests/corpus/eval/errors/` (scripted: `sed -i 's/^=== out$/=== error/' tests/corpus/eval/errors/**/*.llt-eval`); verify with `cargo test` (`tests/corpus/eval/errors/`)

**LSP runner:**

- [ ] Update `lsp_corpus_tests.rs` to use `split_test_file` from `tests/test_helpers.rs` — the LSP runner has its own enforcement logic and keeps it, but shares the file parsing step (`tests/lsp_corpus_tests.rs`)

