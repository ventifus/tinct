# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Phase C: Algebraic Types

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
