# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

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

### `structural-contracts-validate`

See doc/11a-builtins.md §validate, doc/whatif/structural-contracts.md Phase 2. **Depends on:** `structural-contracts-input` (SC Ph1).

- [ ] `validate` builtin: `Dict → Any → Any` (schema first, data second — `[validate nginx-schema %]`); walks schema and data in parallel, collects ALL violations (not fail-fast), returns data unchanged on success (`src/builtins.rs`)
- [ ] Schema key dispatch: `type`, `min`, `max`, `min-length`, `max-length`, `pattern`, `required`, `default`, `items`, `fields`, `enum` — each key applies the corresponding constraint (`src/builtins.rs`)
- [ ] `ErrorKind::SchemaViolation { violations: Vec<(String, String)> }` in `src/error.rs` with error code E090; `pub fn schema_violation(...)` constructor; add E090 to `run_explain` in `src/main.rs` (`src/error.rs`, `src/main.rs`)
- [ ] Violation accumulation: `violations: Vec<[field: Str message: Str]>` where `field` is a dot-path string (note: ambiguous for keys containing `.`; document this limitation); error thrown via `ErrorKind::SchemaViolation` (`src/builtins.rs`, `src/error.rs`)
- [ ] Ensure `validate` materializes `Overlay` values before structural traversal (`$merge` result is lazy overlay); add test with `[validate schema [merge a b]]` input (`src/builtins.rs`)
- [ ] Tests: range validation; pattern matching; required/optional fields; nested schema; `items:` sequence element validation; all violations reported (not first only); Overlay input; pass-through on success (`tests/corpus/eval/stdlib/`, `tests/corpus/eval/errors/`)

### `structural-contracts-describe`

See doc/16-architecture.md §CLI, doc/whatif/structural-contracts.md Phase 3. **Depends on:** `structural-contracts-validate` (SC Ph2).

- [ ] `tinct describe file.llt` CLI subcommand: parse the file, extract `%@Type` annotation; detect schema dicts via heuristic (a dict is a schema dict if any of its values is a dict with at least one recognized schema key: `type`, `min`, `max`, `min-length`, `max-length`, `pattern`, `required`, `items`, `fields`, `enum`); document the heuristic (`src/main.rs`)
- [ ] Human-readable output: one line per field, merging type constraints from `%@Type` with constraint values from the schema dict (`src/main.rs`)
- [ ] JSON output mode: `tinct describe --json file.llt` emits machine-readable contract as a tinct dict serialized to JSON (`src/main.rs`)
- [ ] Tests: `tinct describe fmt/nginx.llt` produces expected output; `--json` mode round-trips; file with no `%@Type` reports "no input contract"; schema dict detection heuristic (`tests/cli_tests.rs`)

### `structural-contracts-blame`

See doc/10-errors.md §Pipeline Blame. **Depends on:** `structural-contracts-describe` (SC Ph3), `blame-tracking` (D4).

- [ ] Pipeline stage tagging: add `blame_map: RefCell<HashMap<ThunkId, String>>` to `EvalContext`; at each `---` boundary, record the producing stage's file path/index keyed on the `%` thunk's ID (avoids `Value::Tagged` variant which would require updating all exhaustive `Value` matches) (`src/eval.rs`, `src/lib.rs`)
- [ ] Contract violation enrichment: when `validate` or `%@Type` check fails, include stage label in error: "Produced by: data.llt, line 3" (`src/error.rs`)
- [ ] Positive/negative party identification per Findler & Felleisen (2002): producing stage is positive party (blamed for wrong output shape), consuming stage is negative party (blamed for wrong contract) (`src/error.rs`)
- [ ] Hints in error messages: suggest `[@Int %.port]` cast or "fix the producing stage" based on mismatch direction (`src/error.rs`)
- [ ] Tests: blame attribution for type violations at `---` boundary; schema constraint violations; multi-stage pipeline chain; hints accurate to the mismatch (`tests/corpus/eval/errors/`)

### `numeric-range`

See doc/05-type-annotations.md §Range Annotations, doc/whatif/numeric-types.md Phase 1. **Depends on:** None (stdlib-only).

- [ ] `between: [fn [lo hi] [fn [v] [and [>= v lo] [<= v hi]]]]` predicate factory in stdlib (`stdlib/prelude.llt`)
- [ ] Helper predicates: `non-negative: [fn [v] [>= v 0]]`, `positive: [fn [v] [> v 0]]` (`stdlib/prelude.llt`)
- [ ] Named width type aliases: `UInt8: [type Int@[is: [between 0 255]]]`, `Int8: [type Int@[is: [between -128 127]]]`, `UInt16`, `Int16`, `UInt32`, `Int32` (`stdlib/numeric.llt` — new file)
- [ ] Verify TypeAssert runtime calls `is:` predicates for range validation — audit existing TypeAssert `default:` handling to ensure `is:` predicate path is exercised (`src/builtins.rs`)
- [ ] Tests: range constraint validation; out-of-range value errors; arithmetic on range-annotated values passes without propagating constraint; `UInt8` alias used in annotation (`tests/corpus/eval/stdlib/`)

### `numeric-decimal`

See doc/03-data-model.md §Decimal Type, doc/whatif/numeric-types.md Phase 2. **Depends on:** Independent.

- [ ] Resolve `rust_decimal::Decimal` vs `d128` (IEEE 754 decimal128) before implementation: `rust_decimal` is 96-bit software decimal (common in financial Rust), `d128` is true IEEE 754 — pick one, document precision/serialization implications (`Cargo.toml`)
- [ ] `Value::Decimal(chosen_type)` variant; adding this triggers compile errors at ALL exhaustive `Value` match sites — audit with `cargo check` before PR merges (`src/value.rs`, `Cargo.toml`)
- [ ] `Type::Decimal` variant; subtype of `Number`; added to `is_subtype` and `unify`/`constrain` (`src/types.rs`)
- [ ] `decimal: Str → Decimal` builtin — parses exact base-10 string; error on invalid format (`src/builtins.rs`)
- [ ] Arithmetic: `Int + Decimal → Decimal`, `Decimal + Decimal → Decimal`, `Float + Decimal → error` (no lossy cross-type); update all arithmetic builtins (`src/builtins.rs`)
- [ ] Add `Value::Decimal` arms to `value_to_json` and `value_to_display_string` in `src/lib.rs`; `Decimal` → JSON number (exact string representation); `Decimal` → display as `Decimal(9.99)` (`src/lib.rs`)
- [ ] Tests: `9.99 + 1.00 = 10.99` exact; `0.1 + 0.2 ≠ 0.30000000000000004` (no IEEE 754 error); cross-type error; JSON round-trip; `value_to_display_string` correctness (`tests/corpus/eval/builtins/`)

### `numeric-bigint`

See doc/03-data-model.md §BigInt, doc/whatif/numeric-types.md Phase 3. **Depends on:** `numeric-decimal` (Ph2).

- [ ] `Value::BigInt(num_bigint::BigInt)` variant; add `num-bigint` crate to `Cargo.toml` (`src/value.rs`, `Cargo.toml`)
- [ ] `Type::BigInt` variant; subtype of `Number`; added to `is_subtype` and `unify` (`src/types.rs`)
- [ ] `big-int: Int → BigInt` builtin; overflow detection in `$+`, `$*`, `$-` on `Int`: promote to `BigInt` on overflow rather than wrapping (`src/builtins.rs`)
- [ ] Promotion rules: `Int + BigInt → BigInt`, `BigInt + BigInt → BigInt`, `BigInt + Decimal → error`, `BigInt + Float → Float` (lossy, explicit) (`src/builtins.rs`)
- [ ] Add `Value::BigInt` arms to `value_to_json` and `value_to_display_string` in `src/lib.rs`; `BigInt` → JSON number string (document interop risk: may exceed JSON receiver's i64 range); `BigInt` → display as `BigInt(n)` (`src/lib.rs`)
- [ ] JSON serialization: `BigInt` serializes as JSON number string (no literal suffix syntax — BigInt is created via `[big-int n]` call or arithmetic overflow, not by parse-time suffix) (`src/builtins.rs`)
- [ ] Tests: factorial computation; integer overflow promotion; `BigInt + Float` rejected; JSON round-trip for large integers (`tests/corpus/eval/builtins/`)

### `numeric-repr`

See doc/05-type-annotations.md §Storage Hints, doc/whatif/numeric-types.md Phase 4. **Depends on:** `numeric-range` (Ph1) — `repr:` consistency is validated against `is:` range constraints; no BigInt dependency (all valid repr values u8–i64 fit within `Value::Int(i64)`).

- [ ] `repr:` annotation key parsed in property dict annotations alongside `type:`, `is:`, `default:` (`src/parser.rs`, `src/typecheck.rs`)
- [ ] Valid `repr:` values: `"u8"`, `"i8"`, `"u16"`, `"i16"`, `"u32"`, `"i32"`, `"u64"`, `"i64"` — type checker validates consistency with declared type and `is:` range constraint (`src/typecheck.rs`)
- [ ] `repr:` propagated to binary serialization dispatch: `to-bytes: [fn [v@[repr: "u8"]] ...]` in stdlib (`stdlib/numeric.llt`)
- [ ] Error: `repr: "u8"` with `is: [between -1 255]` rejected — range exceeds repr capacity (`src/typecheck.rs`)
- [ ] Tests: `@[type: Int  is: [between 0 255]  repr: "u8"]` accepted; `repr` inconsistent with range rejected; binary encoding dispatch (`tests/corpus/eval/type_system/`)
