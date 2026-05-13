# Algebraic Data Types

## Overview

Algebraic data types in tinct are structural tagged records — unions of closed
record types discriminated by key set. The `[type ...]` form extended to multiple
positional entries declares named sum types. No new `Value` variant is needed:
the `try` convention (`[ok: v]` and `[err: msg]`) is already this pattern at
runtime; ADTs give it a static type, a name, and exhaustiveness checking in
`[match]`. External JSON data automatically satisfies variant types when it has
the right shape — interop is free.

> **⚠ Superseded by [error-patterns.md](error-patterns.md) (2026-05-09):** `try` now returns nominal `Value::Variant { tag: "Ok"/"Err" }`, not structural `{ok: v}`/`{err: msg}` dicts. The `try` convention described throughout this doc as structural is stale for the specific `try` builtin; use `[Ok value]`/`[Err msg]` nominal constructors or `[match [try ...] ...]` instead.

## Supersession Notes

Parts of this feature were modified by later features:

- **§Design (key-set discrimination)**: Under BAS, S-RcdTop (`src/types.rs:882`) collapses disjoint single-field record unions — e.g., `{ok: T} | {err: S}` — to `Type::Top`. The core premise of structural key-set discrimination does not hold for single-field variants. For discriminated unions, use [nominal variants](nominal-variants.md) instead. Multi-field structural records (e.g., `{ok: Bool, value: T} | {err: Bool, msg: S}`) are not affected by S-RcdTop but require the `@[[all ...]]` intersection annotation form. See [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09).
- **`try` result type**: `try` returns `Value::Variant { tag: "Ok"/"Err" }` (nominal), not structural `{ok: v}/{err: msg}`. See [error-patterns.md](error-patterns.md) (2026-05-09).
- **`@Record` / `@Dict` semantics**: `@Dict` resolves as a closed empty record; `@Record` no longer implies an open record with a row-variable tail (RowVar was removed under BAS). See [parameterized-dict.md](parameterized-dict.md) (2026-05-09).

## Design

### Structural Tagged Records

> **⚠ Superseded (single-field variants) by [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09):** Under BAS, S-RcdTop collapses unions of disjoint single-field records (e.g., `{ok: T} | {err: S}`) to `Type::Top`. The key-set discrimination described here works for **multi-field** variants (e.g., `{ok: Bool, value: T} | {err: Bool, msg: S}`) but not for single-field variants. For single-field discriminated unions, use [nominal variants](nominal-variants.md).

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

# Single-entry alias (unchanged)
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

  parse: [fn@Result [input@Str]
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
    _:         [error "unknown status"]]
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
pattern-matching design (see `doc/feature/pattern-matching.md` §Open vs Closed Dict
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
This requires `Type::Union(Vec<Type>)` from `doc/feature/union-types.md` as a
prerequisite. The union type is stored as a **`TypeScheme`** — not a bare `Type` —
so that the type variable `a` is properly generalized per call site. At usage sites,
`res@Result` instantiates the alias: `a` becomes a fresh type variable, yielding
`Type::Union(vec![Record({ok: TypeVar("_t0")}), Record({err: Str})])`. Storing a bare
`Type::Union` with a free `a` causes two call sites to share the same type
variable and accidentally unify against each other — scheme wrapping is required.

Variants in a union are checked via `is_subtype`: `is_subtype(Record({ok: Int, tail: Empty}), Union(...))` succeeds if the record is a subtype of any variant
(`[UNION-INJ-L]`, `[UNION-INJ-R]` from `doc/feature/union-types.md` §Subtyping Rules).

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
bound variables. Exhaustiveness checks that the arm set covers all variants.

### Interaction with Row Polymorphism

Rémy (1989) originally covered both records *and* variants in the same row
framework — they are dual constructs using the same row machinery. In tinct's
current design, all ADT variants are record rows (`Type::Record(Row)`), and
the union wraps them in `Type::Union`. This is sound and does not require a
dedicated variant row type.

If a future phase adds `Type::Variant(Row)` as a distinct row-kinded type (the
Gaster & Jones (1996) / Blume (2006) approach), the `[type ...]` declaration form
and all usage sites remain unchanged — the internal representation changes but the
surface syntax does not.

### Interaction with Algebraic Subtyping

Under Simple-sub (Parreaux 2020, see `doc/feature/union-types.md`), structural
union types become *inferred*, not just annotated. With algebraic subtyping, `[if
cond [ok: v] [err: msg]]` automatically produces type `[ok: T] | [err: Str]`
without a `Result` declaration. The `[type ...]` declaration then becomes a
*name* for a set of shapes the type system already understands — an alias,
not a foundation.

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
enforcement requires nominal types (see `doc/feature/nominal-variants.md`).

## Implementation

### Type Checker (`src/typecheck.rs`) — `[type ...]` Extension

1. Extend `resolve_type_dict` (or the `[type ...]` handler in `infer_dict`): when
   the `[type ...]` body contains multiple positional entries, resolve each as a
   type expression and wrap in `Type::Union(vec![...])`.
2. Add `Expr::Str(s) => Ok(Type::StringLiteral(s.clone()))` to `resolve_type_expr`
   so string literals work as type expressions in type position.

Impact: Minor — two small additions to the type checker. No parser changes.
No new keywords. No new AST variants. Backward compatible: single-entry
`[type T]` is unchanged.

### AST (`src/ast.rs`)

No new `Expr` variant is required. The type checker recognises multi-entry
`[type ...]` as a type-expression context and converts positional entries to
`Type::Union(vec![...])`. Type expressions are parsed as general `Expr` nodes
(current approach).

### Type Representation (`src/types.rs`)

`Type::Union(Vec<Type>)` from `doc/feature/union-types.md` is the hard
prerequisite — multi-entry `[type ...]` declarations cannot be represented
without it. Canonical form: sorted, deduplicated, flattened (no nested unions).

### Type Checker (`src/typecheck.rs`) — Three Extensions

1. **Declaration parsing.** When resolving multi-entry `[type ...]` in type expression
   position, convert each variant to a `Type` and wrap in `Type::Union(vec![...])`.
   Register as a type alias.

2. **Union alias instantiation.** When `res@Result` appears in a function parameter,
   instantiate the `Result` alias with fresh type variables via the existing
   `instantiate()` mechanism.

3. **TypeAssert for union membership.** `[@Result expr]` checks `is_subtype(actual,
   Union(...))` using the subtype rules from `doc/feature/union-types.md` §Subtyping
   Rules (`[UNION-INJ-L]`, `[UNION-INJ-R]`, `[UNION-ELIM]`).

### Evaluator (`src/eval.rs`)

No changes. Union types are erased at runtime — values are dicts or
strings, never `Union` instances. Pattern matching in `[match]` operates on
concrete dict values regardless of their static union type.

### Builtins (`src/builtins.rs`)

`try` is typed as `(→ a) → Union([ok: a], [err: Str])` in the builtin type
environment. The most visible immediate benefit: `try` results are statically typed.

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
  for arm-local type narrowing.
- Amadio, R.M. & Cardelli, L. (1993). "Subtyping recursive types." *ACM TOPLAS*,
  15(4), 575–631. — Decidability of type equality for equi-recursive types. Proves
  that type equality checking terminates with a depth guard. Foundation for
  equi-recursive unfolding in future recursive ADT support.
- Parreaux, L. (2020). "The simple essence of algebraic subtyping." In *ICFP '20*,
  Article 124. ACM. — Simple-sub inference: structural union types become *inferred*
  rather than annotated. Under algebraic subtyping, `[type ...]` declarations become
  named aliases for shapes the type system already understands.
- Gaster, B.R. & Jones, M.P. (1996). "A polymorphic type system for extensible
  records and variants." TR NOTTCS-TR-96-3, University of Nottingham. — Variant
  rows dual to record rows; the Remy-Gaster-Jones duality motivates keeping
  `[type ...]`'s surface syntax stable across a future `Type::Variant(Row)` migration.
- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press. Chapter 11
  (variants as sum types), Chapter 15 (subtyping for records and variants). —
  Standard subtyping rules `[UNION-INJ-L]`, `[UNION-INJ-R]`, `[UNION-ELIM]` adopted
  from this treatment (see `doc/feature/union-types.md` §Subtyping Rules).
