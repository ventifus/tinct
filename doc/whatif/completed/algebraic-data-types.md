# What If: Algebraic Data Types for tinct

**State:** Accepted — 2026-05-05

What would it take to add algebraic data types to tinct's structural, dict-first language?

## Current State

tinct has no formal facility for declaring sum types. The `try` builtin already
produces a structural ADT in practice — it returns either `[ok: value]` or
`[err: message]` — but there is no type to declare, no way to constrain a parameter
to "one of these shapes," and no exhaustiveness checking at the match site.

```tinct
# try already produces an ADT, but the type is Any
res: [try risky]

# Discrimination is ad-hoc today
[if [has? res "ok"]
    res.ok
    [error res.err]]
```

Declarations like `Result`, `Event`, `Color` cannot be expressed. There is no static
guarantee that a value is one of a given set of variants, and pattern matching
(see `doc/whatif/pattern-matching.md`) cannot enforce exhaustiveness without knowing
the variant set.

### What's Missing

1. **No way to declare a sum type.** `type Result a = [ok: a] | [err: Str]` is
   inexpressible; the type of `try` is `Any`.
2. **No static discrimination.** The type checker cannot narrow a value of "one of
   these shapes" — all branching on dict shape is untyped.
3. **No exhaustiveness checking.** A `[match]` on a union-typed scrutinee cannot
   warn on missing variants (see `doc/whatif/pattern-matching.md` §Phase 5).
4. **No named constructors.** `Ok` and `Err` are not first-class values; variant
   construction is by dict literal.
5. **No recursive ADTs.** `type Tree a = Leaf | [node: a  left: [Tree a]  right: [Tree a]]`
   requires parametric type aliases and recursive type unfolding, neither of which
   exists today.

## Why Algebraic Data Types Matter for tinct

**Formalise an existing pattern.** `try`'s `[ok: v]` / `[err: msg]` convention is
already an ADT in practice. Every tinct program that uses `try` is already working
with structural sum types — they just have no static type. Declaring `Result` makes
this intention visible to the type checker, the LSP, and readers.

**User-defined sum types.** Config data is full of discriminated shapes: event types,
status codes, parse results, API responses, protocol messages. Today all of these
collapse to `Any`. Named union declarations give them precise types.

**Pattern matching with exhaustiveness.** `[match]` on a declared union type can
statically verify that all variants are handled — the same guarantee Haskell, Elm,
and Rust offer for pattern matching on sum types (see `doc/whatif/pattern-matching.md`
§Phase 5 and §Exhaustiveness Checking).

**Self-hosting more stdlib.** Functions like `try`, `to-int`, and potential
future `decode` operations return sum-typed results. Precise return types make
these functions composable with the type checker rather than opaque.

**Foundation for the type system's next level.** Named union types are the prerequisite
for occurrence typing (Tobin-Hochstadt & Felleisen 2010), which enables the type
checker to narrow a value's type inside each `[match]` arm without explicit casts.

## Design

### Structural Tagged Records

**Core thesis:** ADTs in tinct are unions of closed record types, discriminated by
key set. This follows directly from Principle 1 (Dicts Are Fundamental,
`doc/03-data-model.md`): a sum type is not a new kind of value — it is a
type-system name for a set of dict shapes. No new `Value` variant is needed. The
`try` convention (`[ok: v]` and `[err: msg]`) is already this pattern.

The discrimination model parallels TypeScript's discriminated unions and OCaml's
polymorphic variants (Garrigue 1998): two variants are distinguishable when their
key sets differ. A closed `[ok: a]` type (no `...` rest) contains exactly one
field; a value that has both `ok` and `err` keys does not satisfy either variant.

### Syntax: Multi-Entry `[type ...]`

Union type aliases use the existing `[type ...]` form extended to accept multiple
positional entries — consistent with the annotation rule that positional entries
are union members. No new keyword, no new special form.

```tinct
# Payload variants — positional record type expressions
Result: [type [ok: a] [err: Str]]
Shape:  [type [circle: [radius: Number]] [rect: [w: Number  h: Number]]]

# Tag-only variants — quoted string literal types
Status: [type "ok" "err" "pending"]
Color:  [type "red" "green" "blue"]

# Mixed — payload and tag variants
Event: [type
    [click: [x: Int  y: Int]]
    [key:   [code: Str]]
    "resize"]

# Single-entry alias (current behavior, unchanged)
Name: [type Str]
```

`[type ...]` with multiple positional entries expands to `Type::Union(vec![T1, T2, ...])`.
Single-entry `[type T]` is unchanged — it remains a simple alias. String literals in
type position produce `Type::StringLiteral(s)` — a small extension to
`resolve_type_expr` (one new match arm for `Expr::Str`).

Used as a value in dict entries, exactly like `[fn ...]`:

```tinct
[
  Result: [type [ok: a] [err: Str]]

  parse: [fn@Result [input@String]
    [try [parse-int input]]]

  handle: [fn [res@Result]
    [match res
      [ok:  v]:    v
      [err: msg]:  [error msg]]]
]
```

### Tag-Only Variants

Tag-only variants are quoted string literal types. The runtime value is simply
the string `"ok"`. No new value representation — `StringLiteral(s)` already
exists in the type checker. Construction: `status: "ok"`. Pattern matching uses
existing literal patterns.

```tinct
Status: [type "ok" "err" "pending"]

# Construction
current-status: "ok"

# Pattern matching on tag variants
[match current-status
    "ok":      [handle-ok]
    "err":     [handle-err]
    "pending": [handle-pending]
    ...:         [error "unknown status"]]
```

This follows tinct's dicts-are-fundamental principle — a tag-only enum is a
union of string literal types. No new syntax or runtime representation is required.
Quoted strings are used (not bare words) because bare words in type position are
type variable names or type references, not string literals.

### Closed Variants in Declarations, Open in Patterns

Variants in `[type T1 T2 ...]` declarations are **closed record types by default**. A
declaration `Result: [type [ok: a] [err: Str]]` means:

- The `ok` variant has *exactly* the field `ok` — no additional fields.
- The `err` variant has *exactly* the field `err` — no additional fields.

This closure is what enables exhaustiveness checking: the type checker knows the
complete set of shapes a `Result` value can take.

Pattern matching **remains open by default** (`[ok: v]` matches any dict with an
`ok` key, whether or not it has extra fields). This is consistent with the
pattern-matching design (see `doc/whatif/pattern-matching.md` §Open vs Closed Dict
Matching) and with row polymorphism's open-record default. The closed constraint
lives at the *declaration* site, not the *consumption* site. A value entering a
union-typed context must satisfy the closed shape; inside a `[match]` arm, it is
already known to match, so the pattern can be open.

Open variants are supported explicitly with `...`:

```tinct
# Open payload variant — accepts extra fields beyond the declared ones
FlexResult: [type [ok: a ...]  [err: Str ...]]
```

### Construction

Variant construction is by dict literal — no constructor function is needed:

```tinct
# Constructing an Ok variant
success: [ok: 42]   # type: [ok: IntLiteral(42)] — satisfies Result Int

# TypeAssert enforces variant membership
safe-result: [@Result [ok: 42]]
```

Because ADTs are structural, any dict with the right key set satisfies the variant
type. `[@Result expr]` (TypeAssert, see `doc/07-type-extensions.md` §TypeAssert
Runtime Validation) enforces variant membership at a boundary: it checks that `expr`
is a closed dict matching exactly one of the declared variants.

Named constructor functions can be defined by the user when desired:

```tinct
[
  Result: [type [ok: a] [err: Str]]
  Ok:     [fn [v] [ok: v]]
  Err:    [fn [msg] [err: msg]]
]
```

### Type-Level Representation

`[type [ok: a] [err: Str]]` compiles to `Type::Union(vec![Record(...), Record(...)])`.
This requires `Type::Union(Vec<Type>)` from `doc/whatif/union-types.md` Phase 2 as
a prerequisite. The union type is stored as a **`TypeScheme`** — not a bare `Type` —
so that the type variable `a` is properly generalized per call site. At usage sites,
`res@Result` instantiates the alias: `a` becomes a fresh type variable, yielding
`Type::Union(vec![Record({ok: TypeVar("_t0")}), Record({err: Str})])`. Storing a bare
`Type::Union` with a free `a` would cause two call sites to share the same type
variable and accidentally unify against each other — scheme wrapping is required.

Variants in a union are checked via `is_subtype`: `is_subtype(Record({ok: Int, tail: Empty}), Union(...))` succeeds if the record is a subtype of any variant
(`[UNION-INJ-L]`, `[UNION-INJ-R]` from `doc/whatif/union-types.md` §Subtyping Rules).

### Interaction with Pattern Matching

`[match]` on a union-typed scrutinee narrows the type in each arm — occurrence
typing (Tobin-Hochstadt & Felleisen 2010). When matching:

```tinct
[match res
    [ok:  v]:    ...   # res narrowed to [ok: a], v has type a
    [err: msg]:  ...]  # res narrowed to [err: Str], msg has type Str
```

The type checker: (1) synthesises the scrutinee's type (`Result a`), (2) expands
the union alias, (3) for each arm, checks that the arm's pattern is a subtype of
some variant, (4) narrows the arm environment to use the variant's field types for
bound variables. Exhaustiveness (Phase 3 of adoption) checks that the arm set
covers all variants.

The consumption mechanism — dict destructuring in `[match]` — is the primary
motivation for the `doc/whatif/pattern-matching.md` Phase 3 (structural patterns).
ADT discrimination without destructuring is expressible in Phase 2 (type + literal
patterns) but requires an extra access step for payloads:

```tinct
# Phase 2 — type patterns work but must access payload separately
[match res
    Dict:  [if [has? res "ok"] res.ok [error res.err]]
    Str:   [error res]]

# Phase 3 — dict destructuring is the right form
[match res
    [ok: v]:    v
    [err: msg]: [error msg]]
```

### Interaction with Row Polymorphism

Rémy (1989) originally covered both records *and* variants in the same row
framework — they are dual constructs using the same row machinery. In tinct's
current design, all ADT variants are record rows (`Type::Record(Row)`), and
the union wraps them in `Type::Union`. This is sound and does not require a
dedicated variant row type.

If a future phase adds `Type::Variant(Row)` as a distinct row-kinded type (the
Gaster & Jones (1996) / Blume (2006) approach), the `[type ...]` declaration form
and all usage sites remain unchanged — the internal representation changes but the
surface syntax does not. Approach A does not close off this future.

### Interaction with Algebraic Subtyping

Under Simple-sub (Parreaux 2020, see `doc/whatif/algebraic-subtypes.md`), structural
union types become *inferred*, not just annotated. With algebraic subtyping, `[if
cond [ok: v] [err: msg]]` would automatically produce type `[ok: T] | [err: Str]`
without a `Result` declaration. The `[type ...]` declaration then becomes a
*name* for a set of shapes the type system already understands — an alias,
not a foundation. This means Phase 3 of `doc/whatif/algebraic-subtypes.md` makes
`[type ...]` declarations more ergonomic, not less relevant.

### The Structural Typing Trade-Off

Structural ADTs mean `[ok: 42]` is simultaneously a plain dict and an `Ok Int`
value. Any dict with the right key set satisfies the variant type — duck typing
over data shape (parallel to TypeScript's discriminated unions and OCaml's
polymorphic variants). This brings two properties:

**Gained:** External data (JSON, config files) automatically satisfies variant
types when it has the right shape. `from-json` output that happens to have an
`ok` key is immediately a valid `Ok T` without conversion. Interop is free.

**Foregone:** Opaque constructors with enforced invariants. There is no way to
declare that `Ok` must always contain a non-null value — any dict with key `ok`
satisfies it. TypeAssert provides *runtime* enforcement at boundaries; static
enforcement requires nominal types (a different design with much higher adoption
cost, not appropriate for a configuration language).

## What Would Change

### Type Checker (`src/typecheck.rs`) — `[type ...]` Extension

**Current:** `[type TypeExpr]` accepts exactly one positional type expression.
String literals (`Expr::Str`) in type-expression position are not handled.

**Proposed:**

1. Extend `resolve_type_dict` (or the `[type ...]` handler in `infer_dict`): when
   the `[type ...]` body contains multiple positional entries, resolve each as a
   type expression and wrap in `Type::Union(vec![...])`.
2. Add `Expr::Str(s) => Ok(Type::StringLiteral(s.clone()))` to `resolve_type_expr`
   so string literals work as type expressions in type position.

**Impact:** Minor — two small additions to the type checker. No parser changes.
No new keywords. No new AST variants. Backward compatible: single-entry
`[type T]` is unchanged.

### AST (`src/ast.rs`)

**Current:** `Expr` has no union type variant. Type expressions are parsed as
general expressions and interpreted by the type checker.

**Proposed:** No new `Expr` variant is strictly required if type expressions are
parsed as general `Expr` nodes (current approach). The type checker recognises
multi-entry `[type ...]` as a type-expression context and converts positional
entries to `Type::Union(vec![...])`. Alternatively, a dedicated
`TypeExpr::Union(Vec<TypeExpr>)` could be added if type expressions are separated
from value expressions.

**Impact:** Minor to Moderate depending on whether type expressions gain a
dedicated AST. The simpler path (no new AST variant) reuses existing infrastructure
at the cost of some clarity.

### Type Representation (`src/types.rs`)

**Current:** No `Type::Union` variant. The escape hatch is `Type::Any`.

**Proposed:** `Type::Union(Vec<Type>)` from `doc/whatif/union-types.md` Phase 2.
This is the hard prerequisite — multi-entry `[type ...]` declarations cannot be
represented without it. Canonical form: sorted, deduplicated, flattened (no nested
unions).

**Impact:** Fundamental prerequisite; must arrive with `doc/whatif/union-types.md`
Phase 2.

### Type Checker (`src/typecheck.rs`)

**Current:** No handling for union type declarations or union-typed values.

**Proposed:** Three extensions:

1. **Declaration parsing.** When resolving multi-entry `[type ...]` in type expression
   position, convert each variant to a `Type` and wrap in `Type::Union(vec![...])`.
   Register as a type alias.

2. **Union alias instantiation.** When `res@Result` appears in a function parameter,
   instantiate the `Result` alias with fresh type variables. This uses the existing
   `instantiate()` mechanism.

3. **TypeAssert for union membership.** `[@Result expr]` checks `is_subtype(actual,
   Union(...))` using the subtype rules from `doc/whatif/union-types.md` §Subtyping
   Rules (`[UNION-INJ-L]`, `[UNION-INJ-R]`, `[UNION-ELIM]`).

**Impact:** Moderate. Three new behaviours in the type checker, all building on
existing infrastructure. Exhaustiveness checking (Phase 3) is a separate, larger
addition requiring `[match]` to track variant coverage.

### Evaluator (`src/eval.rs`)

**Current:** No awareness of sum types.

**Proposed:** No changes. Union types are erased at runtime — values are dicts or
strings, never `Union` instances. Pattern matching in `[match]` operates on
concrete dict values regardless of their static union type.

**Impact:** None.

### Builtins (`src/builtins.rs`)

**Current:** `try` return type is `Any`.

**Proposed:** Once `Type::Union` exists, `try` can be typed as
`(→ a) → Union([ok: a], [err: Str])` in the builtin type environment. This is the
most visible immediate benefit of Phase 2: `try` results become statically typed.

**Impact:** Minor — a signature update to one builtin.

### Parser (`src/grammar.pest`)

See Grammar above.

## Phased Adoption

### Phase 1: Convention Documentation (No Code Changes)

Document the structural ADT pattern in `doc/03-data-model.md` and `doc/11-stdlib.md`.
Establish naming conventions:

- `try` result shape is `[ok: v]` / `[err: msg]` — the canonical ADT.
- Payload variants use a single descriptive key: `[circle: ...]`, `[click: ...]`.
- Tag-only variants are lowercase bare words: `ok`, `err`, `pending`.

No implementation work. This phase formalises existing practice, so existing code
is already compliant.

### Phase 2: Multi-Entry `[type ...]` Declarations and Named Types

Extend `[type ...]` to accept multiple positional entries, expanding to
`Type::Union`. No new keyword — reuses the existing `[type ...]` form.

- `Result: [type [ok: a] [err: Str]]` becomes a registered type alias.
- `res@Result` instantiates the alias at usage sites.
- `[@Result expr]` enforces union membership via TypeAssert.
- `try` receives its precise return type: `Union([ok: a], [err: Str])`.
- Type errors on incorrect variant shape: passing `[ok: 42  extra: true]` where a
  closed `[ok: Int]` variant is expected raises a type error.

**Prerequisite:** `doc/whatif/union-types.md` Phase 2 (`Type::Union`, `[UNION-INJ-*]`,
`[UNION-ELIM]` subtyping rules, `normalize_union()`).

### Phase 3: Exhaustiveness Checking in `[match]`

When the scrutinee of `[match]` has a declared union type, check that the arm set
covers all variants:

```tinct
res: [@Result [try risky]]

[match res
    [ok: v]  v]
# Error: non-exhaustive match on Result — missing variant [err: Str]
```

Coverage is computed by comparing the arm patterns against the union's variant
list. A wildcard `_` or variable binding `x` covers all remaining variants. An
`or`-pattern (see `doc/whatif/pattern-matching.md` §Phase 4) can cover multiple
variants in one arm.

**Prerequisite:** `doc/whatif/pattern-matching.md` Phase 3 (dict destructuring) and
Phase 5 (exhaustiveness infrastructure).

### Phase 4: Recursive Variants

Recursive ADTs require two things that do not exist today:

1. **Parameterised type aliases** — `Tree a` as a type alias taking a type parameter.
   See `doc/whatif/parameterized-type-aliases.md`.
2. **Equi-recursive type unfolding** — when unifying `Tree a` with its expansion,
   the type checker must unfold the recursive reference to a fixed depth rather than
   looping. Amadio & Cardelli (1993) prove decidability of type equality for
   equi-recursive types with a depth guard. The depth guard is already present in
   tinct's substitution application (`MAX_APPLY_DEPTH`); equi-recursive unfolding
   extends this to type alias expansion. Iso-recursive types (explicit fold/unfold)
   are not appropriate for a configuration language.

```tinct
# Phase 4 syntax — requires parameterized-type-aliases
Tree: [type Leaf [node: a  left: [Tree a]  right: [Tree a]]]

# Usage
leaf: Leaf
tree: [node: 1  left: Leaf  right: [node: 2  left: Leaf  right: Leaf]]
```

Phase 4 gates on parameterized type aliases. While uncommon in configuration,
recursive types are essential for self-hosting stdlib functions (tree
traversals, nested parse results). See `doc/whatif/parameterized-type-aliases.md`.

**Prerequisites:** `doc/whatif/parameterized-type-aliases.md` complete; equi-recursive
unfolding research (a future `doc/whatif/` item).

### Prerequisites

| Phase | Prerequisites |
|-------|--------------|
| Phase 1 | None |
| Phase 2 | `union-types.md` Phase 2 (`Type::Union`, subtyping rules) |
| Phase 3 | Phase 2 complete; `pattern-matching.md` Phase 3 and Phase 5 |
| Phase 4 | Phase 2 complete; `parameterized-type-aliases.md`; equi-recursive unfolding |

### Trigger

**Phase 2** (named union types): adopt immediately after `union-types.md`
Phase 2 lands. `try` result types already lack precision and
`[ok: T] / [err: Str]` patterns are already recurring.

**Phase 3** (exhaustiveness): adopt together with `pattern-matching.md`
Phase 5 — exhaustiveness checking is the primary motivation for both,
and the two are co-dependent.

**Phase 4** (recursive variants): adopt after parameterised type aliases
land. Stdlib functions written in tinct (tree traversals, nested decode)
are the primary use case.

## References

- Rémy, D. (1989). "Typechecking records and variants in a natural extension of
  ML." In *POPL '89*, pp. 77–88. ACM. — Row polymorphism covers records *and*
  variants from the start; both use the same presence/absence flag machinery in the
  full system. Structural ADT discrimination follows directly from Rémy's framework.
- Garrigue, J. (1998). "Programming with polymorphic variants." In *ML Workshop
  '98*. — OCaml's structural variant types (`` `Foo value ``): a 25-year production
  validation of structural discrimination. Principal types proven for polymorphic
  variants with row polymorphism.
- Blume, M., Acar, U.A. & Chae, W. (2006). "Extensible programming with first-class
  cases." In *ICFP '06*, pp. 239–250. ACM. — Extends row polymorphism to variant
  types, enabling functions polymorphic over open variant sets. Provides a template
  for `Type::Variant(Row)` if tinct eventually adds dedicated variant rows.
- Tobin-Hochstadt, S. & Felleisen, M. (2010). "Logical types for untyped languages."
  In *ICFP '10*, pp. 117–128. ACM. — Occurrence typing: narrows the type of a
  variable inside each `[match]` arm based on the pattern that matched. Foundation
  for Phase 2's arm-local type narrowing.
- Amadio, R.M. & Cardelli, L. (1993). "Subtyping recursive types." *ACM TOPLAS*,
  15(4), 575–631. — Decidability of type equality for equi-recursive types. Proves
  that type equality checking terminates with a depth guard. Foundation for Phase 4
  equi-recursive unfolding.
- Parreaux, L. (2020). "The simple essence of algebraic subtyping." In *ICFP '20*,
  Article 124. ACM. — Simple-sub inference: structural union types become *inferred*
  rather than annotated. Under algebraic subtyping (see `doc/whatif/algebraic-subtypes.md`),
  `[type ...]` declarations become named aliases for shapes the type system already
  understands. See `doc/whatif/union-types.md` Phase 3.
- Gaster, B.R. & Jones, M.P. (1996). "A polymorphic type system for extensible
  records and variants." TR NOTTCS-TR-96-3, University of Nottingham. — Variant
  rows dual to record rows; the Remy-Gaster-Jones duality motivates keeping
  `[type ...]`'s surface syntax stable across a future `Type::Variant(Row)` migration.
- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press. Chapter 11
  (variants as sum types), Chapter 15 (subtyping for records and variants). —
  Standard subtyping rules `[UNION-INJ-L]`, `[UNION-INJ-R]`, `[UNION-ELIM]` adopted
  from this treatment (see `doc/whatif/union-types.md` §Subtyping Rules).
