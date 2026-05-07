# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Phase A: Foundations

### `pattern-matching-basic`

**Depends on:** Type predicates (DONE)
**Spec chapters:** `doc/02-syntax.md` (§Match Expression — syntax, arm forms, pin operator), `doc/08-evaluation.md` (§Pattern Matching — arm testing, scrutinee materialization)

1. `match` added to keyword denylist
2. `Expr::Match`, `MatchArm`, `Pattern`, `LiteralPattern` AST types
3. Parser: `[match scrutinee arm1 arm2 ...]` parsing
4. Pattern parsing mode (bare names as bindings, capitalized words as
   type tags)
5. Evaluator: materialize scrutinee, try arms top-to-bottom
6. Type checker: typed as `Any` initially
7. Formatter: round-trip `Expr::Match`
8. Tests: 10+ corpus tests (type patterns, literal patterns, wildcard,
   variable binding, nested match, no-match error)

### `pattern-matching-destructure`

**Depends on:** `pattern-matching-basic` (A2), `let-binding` (A1) for multi-expression arm bodies
**Spec chapters:** `doc/02-syntax.md` (§Structural Patterns — dict/seq/nested patterns, path-key desugar), `doc/08-evaluation.md` (§Structural Pattern Matching — lazy dict forcing, recursive binding)

1. `Pattern::Dict` with field patterns and `rest` flag
2. `Pattern::Seq` with head/tail patterns
3. Nested pattern support (patterns inside patterns)
4. Evaluator: recursive pattern matching with environment extension
5. Lazy dict matching (only matched keys forced)
6. Seq matching (force head, bind tail thunk)
7. Tests: 10+ corpus tests (dict destructure, nested destructure,
   seq head/tail, open vs closed dict matching, lazy field access,
   `try` result handling)

---

## Phase B: Type System Primitives

### `union-types`

**Depends on:** None (can be done in parallel with Phase A)
**Spec chapters:** `doc/05-type-annotations.md` (§Union Types — `@[T1 T2]` positional syntax, desugar rule, `type: [T1 T2]` resolution), `doc/06-type-inference.md` (§Union Subtyping — `[UNION-INJ-L]`, `[UNION-INJ-R]`, `[UNION-ELIM]` rules), `doc/17-references.md` (Pierce 2002 Ch.15 already present)

1. `Type::Union(Vec<Type>)` variant
2. `normalize_union()` — sort, dedup, flatten nested unions
3. `is_subtype` gains `[UNION-INJ-L]`, `[UNION-INJ-R]`,
   `[UNION-ELIM]`
4. `apply_substitution` handles `Union`
5. `occurs_in` handles `Union`
6. `collect_type_vars` handles `Union`
7. Annotation resolver: collect positional entries from
   `Annotation::PropertyDict` into `type:` value as list; resolve
   `type: [T1 T2]` as `Union(normalize(T1), normalize(T2))` in
   `resolve_annotation` (`src/typecheck.rs`) — no parser change needed
8. Tests: 10+ (union creation, subtyping injection/elimination,
   union in function signatures, union in TypeAssert, union display,
   duplicate elimination, nested union flattening, positional desugar
   `x@[Int Null]`)

### `gradual-typing-split`

**Depends on:** `union-types` (B1) — ships after B1 so union subtyping immediately uses the proper lattice
**Spec chapters:** `doc/06-type-inference.md` (§Gradual Typing — `Unknown` vs `Top`, consistency relation), `doc/07-type-extensions.md` (§Gradual Typing extension roadmap), `doc/17-references.md` (Siek & Taha 2006, Garcia et al. 2016)

1. Document: catalog all `Type::Any` uses with reclassification
2. `Type::Unknown` replaces `Type::Any` (gradual)
3. `Type::Top` added (true supertype)
4. `is_consistent()` function (~30 lines)
5. `is_subtype`: remove `[S-ANY-TOP]` and `[S-ANY-BOT]`; add
   `tau <: Top`
6. Audit and update every `match` on `Type::Any` in `types.rs`
7. Audit and update every `Any` default in `typecheck.rs`
8. Update `unify()` to use consistency for `Unknown`
9. Update doc/06-type-inference.md
10. Tests: full corpus regression + 8 targeted tests (Unknown
    consistency, Top subtyping, non-transitivity of consistency,
    TypeAssert with Top)

### `param-type-aliases`

**Depends on:** None
**Spec chapters:** `doc/05-type-annotations.md` (§Parameterized Type Aliases — `[type [a] body]` syntax, instantiation semantics)

1. `TypeAlias { params: Vec<String>, body: Type }` in type checker
2. Parser: detect `[type [lowercase-words] body]` as parameterized
3. Type checker: resolve `[AliasName Arg1 ...]` with arity check
4. Substitution: build `{param -> arg}` and apply to body
5. Error: arity mismatch error with expected/actual counts
6. Tests: 6+ (parameterized alias, instantiation, arity error,
   row variable in alias body, backward compat for zero-param)

### `type-classes-constrained`

**Depends on:** `gradual-typing-split` (B2), let-generalization complete
**Spec chapters:** `doc/06-type-inference.md` (§Constrained Type Variables — `Equatable a =>`, fixed instance sets), `doc/07-type-extensions.md` (§Dual-Dispatch Builtins — update with constrained signatures), `doc/17-references.md` (Wadler & Blott 1989, Jones 1995)

1. `Constraint` type: `Class(String, String)` pairs
2. `TypeScheme.constraints: Vec<Constraint>` field
3. Fixed instance sets: `Equatable`, `Comparable`, `Numeric`,
   `Showable`, `Mappable`, `Foldable`, `Filterable`, `Appendable`
4. Constraint generation during inference for overloaded builtins
5. Constraint checking during instantiation
6. Builtin signatures updated with constrained type schemes
7. Display: `Equatable a => Fn(a, a -> Bool)` format
8. Error messages: "type X does not satisfy constraint Equatable"
9. doc/06-type-inference.md: constrained type variables section
10. Tests: 10+ (constraint generation, satisfaction, violation errors,
    overloaded builtins, let-generalization with constraints)

### `narrowing-basic`

**Depends on:** Scheduled after `pattern-matching-destructure` (A3). Result type precision improves when `union-types` (B1) lands.
**Spec chapters:** `doc/06-type-inference.md` (§Path-Sensitive Narrowing — `if` as type-level special form, `narrow()` function, environment forking), `doc/17-references.md` (Tobin-Hochstadt & Felleisen 2010, Dunfield & Pfenning 2004)

1. `if` special form: detect `if` calls in `infer_expr` and dispatch
   to dedicated `infer_if(cond, then_expr, else_expr, env, state)`
   instead of generic `check_call` (`src/typecheck.rs`)
2. `Narrowing` enum: `EqLiteral { var, ty }`, `TypeOf { var, ty }`,
   `HasKey { var, key }` (`src/typecheck.rs`)
3. `extract_narrowings(cond: &Expr) -> Vec<Narrowing>`: pattern match
   on condition AST shape; recognize `[= x lit]`, `[= [type-of x] "Int"]`,
   `[has? x "key"]` (`src/typecheck.rs`)
4. Environment forking: clone `env` into `env_true`, apply narrowings;
   `env_false` = clone `env` unmodified (`src/typecheck.rs`)
5. Branch type join: infer `then_expr` in `env_true`, `else_expr` in
   `env_false`; result type is LUB (without B1) or `Union(t1, t2)`
   (with B1) (`src/typecheck.rs`)
6. Conjunction support: `[and cond1 cond2]` applies both narrowings
   to `env_true` (`src/typecheck.rs`)
7. Update type map with narrowed types for LSP hover precision
   (`src/typecheck.rs`)
8. Tests: 10+ (equality narrowing, type-of guard, has? key narrowing,
   conjunction, both operand orderings for `=`, no false-branch
   narrowing, nested if chains, narrowing not leaking across branches,
   type map has narrowed type for LSP hover)

### `narrowing-predicates`

**Depends on:** `narrowing-basic` (B5a). Type predicates (DONE).
**Spec chapters:** `doc/06-type-inference.md` (§Type Predicate Narrowing — `int?` as direct narrowing trigger)

1. Extend `extract_narrowings` to recognize `[int? x]`, `[str? x]`,
   `[dict? x]`, `[bool? x]`, `[float? x]`, `[fn? x]`, `[null? x]`,
   `[seq? x]` as direct narrowing triggers — map each predicate to its
   corresponding `Type` (`src/typecheck.rs`)
2. Predicate-to-type mapping: `int?` -> `Type::Int`, `str?` -> `Type::Str`,
   `dict?` -> `Type::Record(Row::Open)`, `seq?` -> `Type::Seq(Any)`,
   `fn?` -> `Type::Any`, `null?` -> `Type::Record(Row::Empty)`,
   `float?` -> `Type::Float`, `bool?` -> `Type::Bool` (`src/typecheck.rs`)
3. `num?` narrowing: `num?` -> `Type::Number` (supertype of Int | Float)
   (`src/typecheck.rs`)
4. `cond` narrowing: extend narrowing to `cond` — each condition-body
   pair narrows independently (optional — can defer to `if`-only)
   (`src/typecheck.rs`)
5. Tests: 8+ (each predicate narrows correctly, num? supertype narrowing,
   predicate inside `and`, predicate with variable binding, match
   desugared to if/int?/str? chain has correct arm body types, LSP
   hover shows narrowed type in match arm)

---

## Phase C: Algebraic Types

### `adts`

**Depends on:** `union-types` (B1)
**Spec chapters:** `doc/05-type-annotations.md` (§Union Declarations — multi-entry `[type ...]` syntax, string literal type variants), `doc/03-data-model.md` (§Algebraic Data Types — structural discrimination, runtime representation)

1. Type checker: multi-entry `[type ...]` body -> `Type::Union(vec![...])`
2. Type checker: `Expr::Str` in type-expression position -> `Type::StringLiteral(s)`
3. Type alias registration for named union types, stored as `TypeScheme`
   (not bare `Type`) so type variables are properly generalized per call
   site — prevents cross-site variable sharing
4. `try` return type updated to `Union([ok: a], [err: Str])`
5. Type alias instantiation: `res@Result` instantiates the `TypeScheme`
   with fresh type variables via existing `instantiate()` mechanism
6. Tests: 8+ (union declaration, tag-only variants, mixed variants,
   TypeAssert enforcement, `try` result type, type alias usage,
   two call sites don't share type variables)

### `nominal-variants-unit`

**Depends on:** ADTs Phase 1 (convention — effectively none)
**Spec chapters:** `doc/03-data-model.md` (§Nominal Variants — `Value::Variant`, `tag-of`, serialization as `{"Tag": null}`), `doc/05-type-annotations.md` (§Nominal Constructors — uppercase bare words in union declarations)

1. `Value::Variant { tag: String, payload: Option<Rc<Thunk>> }`
2. `type-of` returns `"Variant"` for nominal values
3. `tag-of` builtin: `Variant -> Str`
4. Parser: uppercase bare words in `[type ...]` multi-entry position as nominal
   constructors
5. Environment registration: unit constructor names bound to
   `Value::Variant { tag, payload: None }` (no `Expr::Constructor` —
   constructors are regular `Expr::VarRef` lookups)
6. Serialization: `Value::Variant` to JSON as `{"Tag": null}`
7. Tests: 6+ (unit constructor creation, tag-of, serialization,
   type-of, equality, constructor as VarRef)

### `nominal-variants-full`

**Depends on:** `nominal-variants-unit` (C2), `pattern-matching-basic` (A2)
**Spec chapters:** `doc/03-data-model.md` (§Nominal Variant Payloads — constructor application, lazy payload, serialization), `doc/08-evaluation.md` (§Constructor Evaluation), `doc/05-type-annotations.md` (§Constructor Types — `Some : a -> Option a`)

1. Payload constructor registration: bind name to closure
   `fn(x) -> Variant { tag, payload: Some(x) }` in environment +
   type signature in type environment
2. Constructor call is regular function application (no special eval
   path — `[Some 42]` is `Expr::Call(Expr::VarRef("Some"), [42])`)
3. `Pattern::Constructor { tag, binding }` for match
4. Type: `Type::NominalVariant { tag, payload }`
5. Subtype rules: NominalVariant vs Union, never vs Record
6. Constructor type signatures (`Some : a -> Option a`)
7. Lazy payload semantics (payload as thunk, not forced)
8. Tests: 10+ (payload construction, pattern matching, constructor
   as value for `map`, lazy payload, mixed nominal/structural union,
   serialization)

### `pattern-matching-guards`

**Depends on:** `pattern-matching-destructure` (A3)
**Spec chapters:** `doc/02-syntax.md` (§Pattern Guards — `when` syntax, or-patterns), `doc/08-evaluation.md` (§Guard Evaluation)

1. `MatchArm.guard: Option<Box<Spanned<Expr>>>` field
2. `Pattern::Or(Vec<Spanned<Pattern>>)` variant
3. Or-pattern variable binding consistency check
4. Tests: 6+ (guards, or-patterns, mixed guard+or, variable
   binding errors)

### `exhaustiveness`

**Depends on:** `union-types` (B1), `adts` (C1), `pattern-matching-destructure` (A3). Nominal exhaustiveness additionally depends on `nominal-variants-full` (C3).
**Spec chapters:** `doc/06-type-inference.md` (§Exhaustiveness Checking — Maranget usefulness algorithm, lazy bottom extension, coverage witnesses), `doc/07-type-extensions.md` (§Pattern Matrix — Maranget 2007/2008 reference, Karachalias et al. 2015 three-way partition)

1. `Pattern` enum in Rust: `Constructor { tag, sub_patterns }`, `Wildcard`, `Or(Box<Pattern>, Box<Pattern>)` (`src/coverage.rs`)
2. Extract patterns from `Expr::Match` arm patterns -> `Pattern` representation (`src/coverage.rs`)
3. Core usefulness algorithm: `specialize(c, matrix)`, `default_matrix(matrix)`, `useful(matrix, pattern_vector, sig)` — full Maranget recursive descent over nested patterns (`src/coverage.rs`)
4. Lazy extension: add bottom to constructor signature; wildcards match bottom, constructors don't; `divergent_useful()` for inaccessible RHS detection (`src/coverage.rs`)
5. `infer_match()` integration: when scrutinee type is `Type::Union`, extract variant set and call coverage algorithm; emit type error for uncovered variants (`src/typecheck.rs`)
6. Non-exhaustive error: type error listing uncovered pattern witnesses
7. Redundancy + inaccessible RHS warnings: flag unreachable arms and divergent-but-inaccessible arms
8. Nominal variant coverage: constructor set from `Type::Union` containing `Type::NominalVariant` entries (depends on C3)
9. Tests: 15+ — Rust unit tests for coverage algorithm (complete coverage, missing variant, wildcard, or-pattern, nested pattern exhaustiveness, nested pattern redundancy, inaccessible RHS with lazy bottom, guard opacity); corpus tests for type checker integration (union scrutinee triggers check, non-union skips, TypeAssert works, error messages show uncovered witnesses)

---

## Phase D: Advanced Typing

### `type-classes-full`

**Depends on:** `type-classes-constrained` (B4), `param-type-aliases` (B3)
**Spec chapters:** `doc/06-type-inference.md` (§Type Classes), `doc/07-type-extensions.md`, `doc/17-references.md`
**Estimated tasks:** 15+

Class declarations, instance declarations, superclass hierarchy,
dictionary passing at runtime.

### `algebraic-subtyping`

**Depends on:** `union-types` (B1), `gradual-typing-split` (B2)
**Spec chapters:** `doc/06-type-inference.md` (§Algebraic Subtyping — Simple-sub, constraint solving, `Any` split integration), `doc/17-references.md`
**Estimated tasks:** 20+

Replace Robinson unification + `[U-SUBSUME]` with Simple-sub (Parreaux
2020) constraint solving. Inferred union and intersection types.

### `recursive-adts`

**Depends on:** `param-type-aliases` (B3), `adts` (C1)
**Spec chapters:** `doc/05-type-annotations.md` (§Recursive Type Aliases), `doc/07-type-extensions.md`
**Estimated tasks:** 6

Equi-recursive type unfolding with depth guard.

### `blame-tracking`

**Depends on:** `gradual-typing-split` (B2)
**Spec chapters:** `doc/10-errors.md` (§Blame — provenance, typed/untyped boundary), `doc/08-evaluation.md` (§Blame Labels)
**Estimated tasks:** 12+

Full blame provenance for typed/untyped boundaries.

### `structural-contracts-input`

**Depends on:** None
**Spec chapters:** `doc/05-type-annotations.md` (§Pipeline Input Types)
**Estimated tasks:** 4

`%@Type` pipeline boundary annotation.

### `structural-contracts-validate`

**Depends on:** `structural-contracts-input` (SC Ph1)
**Spec chapters:** `doc/11a-builtins.md` (§validate), `doc/05-type-annotations.md` (§Schema Validation)
**Estimated tasks:** 6

`validate` builtin for schema-as-dict runtime constraints.

### `structural-contracts-describe`

**Depends on:** `structural-contracts-validate` (SC Ph2)
**Spec chapters:** `doc/16-architecture.md` (§CLI — tinct describe)
**Estimated tasks:** 4

`tinct describe` CLI subcommand for schema introspection.

### `structural-contracts-blame`

**Depends on:** `structural-contracts-describe` (SC Ph3), `blame-tracking` (D4)
**Spec chapters:** `doc/10-errors.md` (§Pipeline Blame)
**Estimated tasks:** 6

Pipeline blame integration with structural contracts.

### `numeric-range`

**Depends on:** None (stdlib-only)
**Spec chapters:** `doc/05-type-annotations.md` (§Range Annotations)
**Estimated tasks:** 6

Range annotations via `is:` predicate — pure stdlib.

### `numeric-decimal`

**Depends on:** Independent
**Spec chapters:** `doc/03-data-model.md` (§Decimal Type)
**Estimated tasks:** 6

Exact base-10 decimal arithmetic.

### `numeric-bigint`

**Depends on:** `numeric-decimal` (Ph2)
**Spec chapters:** `doc/03-data-model.md` (§BigInt)
**Estimated tasks:** 6

Arbitrary-precision integer type.

### `numeric-repr`

**Depends on:** `numeric-bigint` (Ph3)
**Spec chapters:** `doc/05-type-annotations.md` (§Storage Hints)
**Estimated tasks:** 6

`repr:` storage hint annotations for numeric types.
