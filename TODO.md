# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Phase D: Advanced Typing

### `type-classes-full`

See doc/06-type-inference.md §Type Classes, doc/07-type-extensions.md. **Depends on:** `type-classes-constrained` (B4), `param-type-aliases` (B3).

- [ ] `class` and `instance` keywords added to denylist; `Expr::ClassDecl`, `Expr::InstanceDecl` AST variants; parser: `[class [Name a] method: Type]`, `[instance [Name Int] method: impl]` (`src/lexer.rs`, `src/parser.rs`, `src/ast.rs`)
- [ ] Class environment: `ClassEnv` map from class name → `ClassDecl { params, methods, superclasses }`; registered during expansion/eval-time, queried during type-check (`src/typecheck.rs`)
- [ ] Instance environment: `InstanceEnv` map from `(ClassName, Type)` → `InstanceDecl { methods }`; checked for uniqueness, no overlapping instances (`src/typecheck.rs`)
- [ ] Superclass hierarchy: `ClassEnv` stores `superclasses: Vec<String>`; constraint entailment (`Equatable a ⊢ Showable a` if Showable is a superclass of Equatable) (`src/typecheck.rs`)
- [ ] Dictionary construction: for each instance, build `Value::Dict` of method implementations at instance registration time; bind to environment (`src/eval.rs`)
- [ ] Dictionary threading: overloaded function calls look up the dictionary for the concrete type and pass it as an implicit first argument at eval time (`src/eval.rs`)
- [ ] Kind system extension: `Kind::Type` and `Kind::Arrow(Box<Kind>, Box<Kind>)` to support `Functor f` (higher-kinded); type variables carry a `kind` field (`src/types.rs`)
- [ ] Higher-kinded type variable inference: resolve `Functor f` where `f` must have kind `Type → Type`; unification checks kind compatibility (`src/typecheck.rs`)
- [ ] Error messages: "no instance of `Equatable` for type `Function`"; "ambiguous type variable `a` in class constraint" (`src/error.rs`)
- [ ] Update overloaded builtin signatures to use class constraints from B4 plus new D1 instances (`src/builtins.rs`)
- [ ] Tests: `[class [Equatable a] ...]` declaration; `[instance [Equatable Int] ...]`; dictionary lookup; superclass entailment; kind error on wrong arity; no-instance error (`tests/corpus/eval/type_system/`)

### `algebraic-subtyping`

See doc/06-type-inference.md §Algebraic Subtyping, doc/whatif/completed/union-types.md §Full Algebraic Subtyping. **Depends on:** `union-types` (B1), `gradual-typing-split` (B2).

**3a — Constraint infrastructure:**

- [ ] `TypeVarBounds { lower: Vec<Type>, upper: Vec<Type> }` struct; `InferState.bounds: HashMap<u32, TypeVarBounds>` alongside existing `Substitution` (`src/types.rs`)
- [ ] `constrain(t1: &Type, t2: &Type, state: &mut InferState, span: Span)` — polarity-aware structural decomposition: contravariant params, covariant returns and record fields (`src/types.rs`)
- [ ] Constraint provenance: `ConstraintSource { span, reason: String }` threaded through `constrain()` for error messages (`src/typecheck.rs`)
- [ ] Bound satisfiability check: `join(lower) <: meet(upper)` at each constraint site; error on conflict with provenance chain (`src/types.rs`)

**3b — Migrate call sites (replace `unify` with `constrain`):**

- [ ] Replace literal-to-base promotion `unify` with `constrain`: `IntLiteral(42) <: Int`, `StringLiteral("x") <: Str` (`src/typecheck.rs`)
- [ ] Replace function application parameter and return type `unify` with `constrain` pairs (`src/typecheck.rs`)
- [ ] Replace record field checking `unify` with `constrain` for shared fields; row variable binding becomes lower-bound constraint (`src/typecheck.rs`)
- [ ] Replace let-generalization `unify` fallback with bound-carrying scheme: generalized variables carry `lower`/`upper` from `InferState.bounds` into `TypeScheme` (`src/typecheck.rs`)
- [ ] Remove `[U-SUBSUME]` ground-type compatibility check — subtyping now built into `constrain`; remove `check_subsumption` call sites (`src/typecheck.rs`)

**3c — Inferred unions/intersections from bound compaction:**

- [ ] Inferred `Type::Union` when a type variable has multiple lower bounds: `compact_lower(bounds) -> Type` via `normalize_union` (`src/types.rs`)
- [ ] Inferred `Type::Intersection` when a type variable has multiple upper bounds: `compact_upper(bounds) -> Type` via `normalize_intersection` (`src/types.rs`)
- [ ] `normalize_intersection(Vec<Type>) -> Type`: sort, deduplicate, flatten nested intersections; `Top` identity, `Never` absorbing (`src/types.rs`)
- [ ] Tests: inferred union from `[if cond 1 "x"]`; inferred intersection from multi-bound variable; bound conflict error with provenance chain; round-trip principal type property (`tests/corpus/eval/type_system/`)

### `recursive-adts`

See doc/05-type-annotations.md §Recursive Type Aliases, doc/07-type-extensions.md. **Depends on:** `param-type-aliases` (B3), `adts` (C1).

- [ ] `RecursiveTypeGuard` in `InferState`: tracks aliases currently being expanded (cycle detection); `HashSet<String>` with `MAX_APPLY_DEPTH` counter (`src/typecheck.rs`)
- [ ] Alias expansion in `is_subtype`: when comparing a named alias to another type, unfold one layer and recurse; guard prevents infinite unfolding (`src/types.rs`)
- [ ] Alias expansion in `unify`/`constrain`: same unfolding strategy for recursive alias unification (`src/types.rs`)
- [ ] Error: "recursive type `Tree` exceeds maximum unfolding depth" with the alias chain path (`src/error.rs`)
- [ ] Spec: document equi-recursive semantics in `doc/05-type-annotations.md` §Recursive Type Aliases — aliases are transparent (equi-recursive), not opaque (iso-recursive) (`doc/05-type-annotations.md`)
- [ ] Tests: `Tree: [type Leaf [node: a left: [Tree a] right: [Tree a]]]`; structural subtyping between recursive types; depth limit detection; mutual recursion `A = [B] B = [A]` (`tests/corpus/eval/type_system/`)

### `blame-tracking`

See doc/10-errors.md §Blame, doc/08-evaluation.md §Blame Labels. **Depends on:** `gradual-typing-split` (B2).

- [ ] `BlameLabel { origin_span: Span, boundary_span: Span, polarity: BlameParity }` struct; `BlameParity::Positive | Negative` (`src/error.rs`)
- [ ] Extend `ThunkState::Guarded` with `blame_label: Option<BlameLabel>` — co-natural strategy: O(1) space per thunk, discard outer label when chaining (`src/value.rs`)
- [ ] TypeAssert guard construction: populate `BlameLabel` from the `[@Type expr]` annotation site; `Positive` polarity (value must conform to type) (`src/typecheck.rs`)
- [ ] Blame propagation across function calls: annotated parameter acts as a contract boundary; `Negative` polarity for expected type at call site (`src/eval.rs`)
- [ ] `---` pipeline boundary blame: each document's output `%` carries `BlameLabel` pointing to the producing stage's final expression (`src/eval.rs`)
- [ ] Error message enrichment: "type assertion failed at line 5; value originated from unannotated expression at line 3" with positive party and negative party named (`src/error.rs`)
- [ ] Automatic guard insertion elaboration: at `Unknown → Concrete` boundaries (function calls where arg type is `Unknown`, field access on `Unknown`), insert `ThunkState::Guarded` with blame label (`src/typecheck.rs`)
- [ ] Tests: blame attribution on TypeAssert failure; pipeline boundary blame pointing to producing stage; co-natural strategy O(1) heap check; `Unknown` boundary blame (`tests/corpus/eval/errors/`, `tests/corpus/eval/type_system/`)

### `structural-contracts-input`

See doc/05-type-annotations.md §Pipeline Input Types, doc/whatif/structural-contracts.md Phase 1. **Depends on:** None.

- [ ] Parser: `%@Type` as document-level annotation — first expression in a document that is a `VarRef("%")` with `@` annotation is treated as input type binding, not a value expression (`src/parser.rs`)
- [ ] Type checker: resolve `%@Type` annotation and bind `%` to the declared type within the document; cross-document checking: doc N's inferred output type must unify with doc N+1's `%@Type` (`src/typecheck.rs`)
- [ ] Multi-file pipeline type checking: `tinct eval data.llt fmt.llt` propagates output type of `data.llt` as input constraint for `fmt.llt`'s `%@Type` (`src/main.rs`, `src/typecheck.rs`)
- [ ] LSP auto-complete: when cursor is inside a document with `%@Type`, offer completions for `%` field access based on declared type (`src/lsp/analysis.rs`)
- [ ] Tests: single-document `%@[port: Int hostname: Str]` binding, cross-document unification, mismatch error, open record annotation accepts extra fields (`tests/corpus/eval/pipeline/`)

### `structural-contracts-validate`

See doc/11a-builtins.md §validate, doc/whatif/structural-contracts.md Phase 2. **Depends on:** `structural-contracts-input` (SC Ph1).

- [ ] `validate` builtin: `Dict → Any → Any` — walks schema dict and data value in parallel, collects ALL violations (not fail-fast), returns data unchanged on success (`src/builtins.rs`)
- [ ] Schema key dispatch: `type`, `min`, `max`, `min-length`, `max-length`, `pattern`, `required`, `default`, `items`, `fields`, `enum` — each key applies the corresponding constraint (`src/builtins.rs`)
- [ ] Violation accumulation: `violations: Vec<[field: Str message: Str]>` collected throughout the walk; error thrown as `[violations: [...]]` structured value (`src/builtins.rs`, `src/error.rs`)
- [ ] Field path tracking: `field` in each violation is the dot-path to the failing field (e.g. `"config.port"`) (`src/builtins.rs`)
- [ ] Tests: range validation, pattern matching, required/optional fields, nested schema, all violations reported (not just first), pass-through on success (`tests/corpus/eval/stdlib/`, `tests/corpus/eval/errors/`)

### `structural-contracts-describe`

See doc/16-architecture.md §CLI, doc/whatif/structural-contracts.md Phase 3. **Depends on:** `structural-contracts-validate` (SC Ph2).

- [ ] `tinct describe file.llt` CLI subcommand: parse the file, extract `%@Type` annotation and any schema dicts (variables whose values match schema shape), emit human-readable description (`src/main.rs`)
- [ ] Human-readable output: one line per field, including type constraints and schema constraints merged (`src/main.rs`)
- [ ] JSON output mode: `tinct describe --json file.llt` emits machine-readable contract as a tinct dict serialized to JSON (`src/main.rs`)
- [ ] Tests: `tinct describe fmt/nginx.llt` produces expected output; `--json` mode round-trips; file with no `%@Type` reports "no input contract" (`tests/cli_tests.rs`)

### `structural-contracts-blame`

See doc/10-errors.md §Pipeline Blame. **Depends on:** `structural-contracts-describe` (SC Ph3), `blame-tracking` (D4).

- [ ] Pipeline stage tagging: each `%` value carries a `stage_label: Option<String>` (file path or `---` index) attached at each `---` boundary (`src/eval.rs`)
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

- [ ] `Value::Decimal(rust_decimal::Decimal)` variant; add `rust_decimal` crate to `Cargo.toml` (`src/value.rs`, `Cargo.toml`)
- [ ] `Type::Decimal` variant; subtype of `Number`; added to `is_subtype` and `unify` (`src/types.rs`)
- [ ] `decimal: Str → Decimal` builtin — parses exact base-10 string; error on invalid format (`src/builtins.rs`)
- [ ] Arithmetic: `Int + Decimal → Decimal`, `Decimal + Decimal → Decimal`, `Float + Decimal → error` (no lossy cross-type); update all arithmetic builtins (`src/builtins.rs`)
- [ ] JSON serialization: `Decimal` serializes as a JSON number (exact representation); deserializes from JSON number string with explicit `decimal` call (`src/builtins.rs`)
- [ ] Tests: `9.99 + 1.00 = 10.99` exact; `0.1 + 0.2 ≠ 0.30000000000000004` (no IEEE 754 error); cross-type error; JSON round-trip (`tests/corpus/eval/builtins/`)

### `numeric-bigint`

See doc/03-data-model.md §BigInt, doc/whatif/numeric-types.md Phase 3. **Depends on:** `numeric-decimal` (Ph2).

- [ ] `Value::BigInt(num_bigint::BigInt)` variant; add `num-bigint` crate to `Cargo.toml` (`src/value.rs`, `Cargo.toml`)
- [ ] `Type::BigInt` variant; subtype of `Number`; added to `is_subtype` and `unify` (`src/types.rs`)
- [ ] `big-int: Int → BigInt` builtin; overflow detection in `$+`, `$*`, `$-` on `Int`: promote to `BigInt` on overflow rather than wrapping (`src/builtins.rs`)
- [ ] Promotion rules: `Int + BigInt → BigInt`, `BigInt + BigInt → BigInt`, `BigInt + Decimal → error`, `BigInt + Float → Float` (lossy, explicit) (`src/builtins.rs`)
- [ ] JSON serialization: `BigInt` serializes as a JSON number or string (configurable); literals parsed as `Int` unless suffix or overflow (`src/builtins.rs`)
- [ ] Tests: factorial computation; integer overflow promotion; `BigInt + Float` rejected; JSON round-trip for large integers (`tests/corpus/eval/builtins/`)

### `numeric-repr`

See doc/05-type-annotations.md §Storage Hints, doc/whatif/numeric-types.md Phase 4. **Depends on:** `numeric-bigint` (Ph3).

- [ ] `repr:` annotation key parsed in property dict annotations alongside `type:`, `is:`, `default:` (`src/parser.rs`, `src/typecheck.rs`)
- [ ] Valid `repr:` values: `"u8"`, `"i8"`, `"u16"`, `"i16"`, `"u32"`, `"i32"`, `"u64"`, `"i64"` — type checker validates consistency with declared type and `is:` range constraint (`src/typecheck.rs`)
- [ ] `repr:` propagated to binary serialization dispatch: `to-bytes: [fn [v@[repr: "u8"]] ...]` in stdlib (`stdlib/numeric.llt`)
- [ ] Error: `repr: "u8"` with `is: [between -1 255]` rejected — range exceeds repr capacity (`src/typecheck.rs`)
- [ ] Tests: `@[type: Int  is: [between 0 255]  repr: "u8"]` accepted; `repr` inconsistent with range rejected; binary encoding dispatch (`tests/corpus/eval/type_system/`)
