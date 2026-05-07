# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## CLI and Test Infrastructure

### `strict-mode`

`--strict` CLI flag, labeled corpus sections (`=== out` / `=== warn` / `=== error`), and LSP stdlib validation. See doc/12-tooling.md §Strict Mode.

**Corpus format extension — labeled sections:**

- [ ] Replace `split_test_file` with a new parser for labeled section delimiters: `=== out`, `=== warn`, `=== error`; bare `===` is now a parse error — the runner panics with "bare `===` is no longer valid; use `=== out`, `=== warn`, or `=== error`"; return `TestExpectations { out: Option<String>, warn: Option<String>, error: Option<String> }` (`tests/corpus_tests.rs`)
- [ ] Migrate all existing corpus test files: rewrite every bare `===` to `=== out`; script this with `sed -E 's/^===$/=== out/'` across `tests/corpus/`; verify no bare `===` remains (`tests/corpus/`)
- [ ] Update corpus test runner to collect three outputs per test run: (1) eval output via `eval_source`, (2) type warnings via `typecheck_source`, (3) parse/eval error messages; compare each against its labeled section — absent section means assert empty (`tests/corpus_tests.rs`)
- [ ] Semantics: a test with no `=== warn` section asserts zero type warnings; a test with `=== warn` asserts the warnings match exactly — no external flag needed to enforce warning-cleanliness (`tests/corpus_tests.rs`)
- [ ] Seed one corpus test per distinct warning category using `=== warn`: `type_mismatch.llt-eval`, `unresolved_type_var.llt-eval`, `record_field_missing.llt-eval`, `function_arity.llt-eval` — each has `=== out` (empty or value) and `=== warn` (expected message) (`tests/corpus/typecheck/warnings/`)
- [ ] Audit existing corpus files after migration: run the full suite; any file that currently produces type warnings will now fail on its empty `=== warn` expectation; fix each warning or add an explicit `=== warn` section (`tests/corpus/`)

**CLI `--strict` flag (for end-user CI use, independent of corpus format):**

- [ ] `--strict` flag on `tinct eval`: type errors become fatal — collect all `TypeError` from `typecheck_file`, print to stderr with the existing error format, exit code 1; without `--strict`, current advisory behavior unchanged (`src/main.rs`)
- [ ] `EvalConfig.strict: bool` threaded through the eval pipeline so `eval_source` and `eval_file` callers can also opt in (`src/lib.rs`, `src/eval.rs`)
- [ ] `tinct fmt --strict`: exits 1 if the file has type errors; useful for CI pre-commit hooks (`src/main.rs`)

**LSP validation — reusing the existing corpus:**

- [ ] LSP corpus runner: spawn `tinct lsp`, send `initialize` + `textDocument/didOpen` with the source extracted from each `.llt-eval` file (content before the first `=== ` section), collect `publishDiagnostics`; map `DiagnosticSeverity::WARNING` → compare against `=== warn` section, `DiagnosticSeverity::ERROR` → compare against `=== error` section; a file with no `=== warn` / `=== error` sections must produce zero diagnostics (`tests/lsp_tests.rs` — new file, reads same `tests/corpus/` files as the eval runner)
- [ ] LSP stdlib validation: for each `.llt` file under `stdlib/`, open via the LSP runner; stdlib files have no `=== warn` or `=== error` sections so the assertion is zero diagnostics of any severity — ensures stdlib is both warning-free and error-free in the LSP view (`tests/lsp_tests.rs`)
- [ ] The `tests/lsp_corpus/` directory and its `.expected.json` format (described in `README.md`) are superseded by the labeled-section approach; update `README.md` to describe the new design; the directory is kept for any future LSP-specific tests (hover, completion, definition) that have no eval-corpus equivalent (`tests/lsp_corpus/README.md`)
- [ ] Document `--strict` and the `=== out` / `=== warn` / `=== error` corpus format in doc/12-tooling.md §Strict Mode and §Corpus Test Format (`doc/12-tooling.md`)

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

