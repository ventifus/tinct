# What If: Explicit Constraint Annotations for tinct

**State:** Accepted — 2026-05-11

What would it take to let users write typeclass constraints directly in function
annotations, and to unify `fn@[...]` into a proper metadata dict?

## Current State

Tinct's type system infers typeclass constraints from the function body. When
you write `[fn [a b] [= a b]]`, the type checker sees the `=` builtin (typed
`Equatable a => a → a → Bool`) and infers `Equatable a` on the result scheme.
LSP hover shows `Equatable a => Fn@Boolean [a a]`. The constraint is correct — but
it cannot be written in source.

The annotation `fn@Type` names the return type, and lowercase TypeVar names
(`fn@a`, `xs@a`) thread through `ann_mapping` so that `a` in the return
annotation and `a` in a param annotation refer to the same inferred variable.
What's missing is any way to attach a constraint to that TypeVar.

The `fn@[...]` dict form exists syntactically but is currently mis-implemented:
the dict is treated as a record type annotation rather than a metadata bag.
There are zero uses of this form in the codebase — the implementation was never
completed.

```tinct
# Today: constraint is inferred from body, invisible at the call site
min: [fn@Unknown [xs] ...]

# TypeVar works for the return type, but constraint has no syntax
min: [fn@a [xs@[Seq a]] ...]   # a is Comparable — but nothing says so
```

```tinct
# LSP hover shows: Comparable a => Fn@a [[Seq a]]
# User cannot copy that display back into source annotations
```

### What's Missing

1. No syntax for declaring that a TypeVar must satisfy a constraint — users
   cannot write what the type checker infers and displays.
2. `fn@[...]` dict form is a dead end: it produces a record type instead of a
   function type, making it useless for any structured annotation.
3. No mechanism for `doc:` strings on function types (the existing `@[doc: "..."]`
   syntax attaches to dict keys, not to function types).
4. Inferred constraint display in LSP hover cannot round-trip to source — the
   hover output and the annotation language are disconnected.

## Why Explicit Constraint Annotations Matter for tinct

**Accurate stdlib types.** `min`, `max`, `sorted`, and any function that
compares or hashes its arguments currently can only be annotated `@Unknown`.
With constraint annotations they become `fn@a [xs@[Seq a]]` with `Comparable a`
— a precise type the checker enforces.

**LSP hover round-trip.** The type checker already computes `Comparable a =>
Fn@a [[Seq a]]`. With constraint annotations, users can paste that back into
source. Hover and annotation are isomorphic.

**Library interface documentation.** Stdlib authors writing polymorphic
functions can declare their requirements explicitly, making function interfaces
self-documenting without relying on callers to read the source body.

**Typecheck-enforced contracts.** A function annotated with `constraint: [a:
Comparable]` is checked against its body — if the body never uses `a` in a
Comparable context, the annotation is still honoured as an explicit lower bound
on the constraint set.

## Design

### `fn@[...]` as a function metadata dict

The `fn@[...]` annotation form is restructured as a named-key metadata dict.
Three keys are defined:

| Key | Value | Semantics |
|-----|-------|-----------|
| `return:` | any type annotation | The function's return type |
| `constraint:` | `[typevar: ClassName ...]` | TypeVar constraints |
| `doc:` | string literal | Documentation for LSP hover |

All three keys are optional. `fn@[...]` with no `return:` key infers the return
type from the body, identical to a bare `fn`.

The existing `fn@Type` shorthand is permanent and unchanged — it is equivalent
to `fn@[return: Type]` and covers the common case with no extra syntax.

```tinct
# Shorthand — unchanged, always valid
min: [fn@a [xs@[Seq a]] ...]

# Full form — explicit return type, constraint, and doc
min: [fn@[return: a  constraint: [a: Comparable]  doc: "Return smallest element"] [xs@[Seq a]] ...]

# Doc-only — return type inferred from body
greet: [fn@[doc: "Format a greeting string"] [name@String] [str "Hello " name]]

# Constraint on a TypeVar not used as the return type
check-all: [fn@[return: Bool  constraint: [a: Equatable]] [xs@[Seq a]  target@a] ...]
```

### Constraint value syntax

The `constraint:` value is a dict using the natural binding form `[typevar:
ClassName]`. Lowercase keys are TypeVar names; uppercase values are class names.
This is unambiguous: tinct record field annotations always have lowercase keys
(`@[name: String]`), so an uppercase value identifies a class, not a type.

```tinct
# Single constraint
constraint: [a: Comparable]

# Multiple TypeVars
constraint: [a: Comparable  b: Showable]

# Multiple constraints on one TypeVar (list value)
constraint: [a: [Comparable Showable]]
```

### Disambiguation: metadata dict vs. union return type

`resolve_fn_type()` inspects the `PropertyDict` entries: if ANY entry has a
named key matching `return:`, `constraint:`, or `doc:`, the dict is routed to
`resolve_fn_metadata()` (metadata handler). If ALL entries are positional (no
named keys), the dict is routed to the existing union return type handler
(`fn@[Int Null]` → function returning `Int | Null`). Mixed named + positional
entries are rejected: "fn annotation must use either named keys (return:,
constraint:, doc:) or positional entries (union return type), not both."

### Processing order

`resolve_fn_metadata()` processes keys in a fixed order to ensure TypeVars
exist before they are referenced:

1. **`constraint:`** — creates fresh TypeVars in `ann_mapping`, sets levels in
   `state.levels`, registers `Constraint` structs in `state.constraints`. For
   list values (`[a: [Comparable Showable]]`), expand to one `Constraint` per
   class. Validate class names against known classes (hardcoded set pre-HKT,
   `ClassEnv` post-HKT) — emit "unknown class 'Foo'" on mismatch.
2. **`return:`** — resolved as a type expression via `resolve_type_expr`. May
   reference TypeVars created by `constraint:` via `ann_mapping`.
3. **`doc:`** — stored as a string in `TypeScheme.doc`. Not part of the type.

### TypeVar scoping

Constraint declaration and TypeVar naming use the same `ann_mapping` mechanism
as today. Processing `constraint: [a: Comparable]` creates a fresh TypeVar
`_t0`, registers `a → _t0` in `ann_mapping` (and `state.levels[_t0] =
state.level`), and adds `Constraint { class: "Comparable", var: "_t0" }` to
`state.constraints`. When `return: a` or `xs@[Seq a]` is resolved subsequently,
`ann_mapping` looks up `a` and returns the same constrained `_t0`.

### Interaction with inference

Explicit constraint annotations and inferred constraints compose. If `constraint:
[a: Comparable]` is declared and the body also uses `a` in an `Equatable`
context, both constraints are registered. Constraint simplification
(`simplify_constraints`) removes `Equatable a` because `Comparable` entails
`Equatable` via the superclass relation — the generalised scheme carries only
`Comparable a`.

If the declared constraint is stronger than what the body requires (the body
never uses `a` comparably), the constraint is still enforced: the TypeVar is
constrained, and any call site passing an `a` that doesn't satisfy `Comparable`
gets a type error. This allows library authors to declare the intended interface
even when the body doesn't exercise the full constraint.

### `doc:` integration

The `doc:` string is extracted from the annotation dict and stored alongside the
function's `TypeScheme` in the type environment. It is not part of the type and
does not affect type checking. The LSP hover handler reads it and displays it
above the inferred type. This replaces the current workaround of putting
`@[doc: "..."]` on the dict key (`f@[doc: "..."]: [fn@T ...]`) for function
definitions.

```tinct
between: [fn@[return: Fn  doc: "Return a predicate that tests whether a value lies in [lo, hi)"] [lo hi] ...]
```

LSP hover for `between`:

```text
between: Fn@Fn [lo hi]
Return a predicate that tests whether a value lies in [lo, hi)
```

## What Would Change

### `src/typecheck_annot.rs` — annotation resolution

**Current:** `resolve_fn_type()` handles the `Simple(name)` annotation form
only. If a `PropertyDict` annotation appears on `fn`, it is delegated to
`resolve_type_dict()` and incorrectly produces a record type.

**Proposed:** `resolve_fn_type()` detects `PropertyDict` annotations and
dispatches to a new `resolve_fn_metadata()` helper that extracts `return:`,
`constraint:`, and `doc:` keys. `return:` is resolved as a type expression via
`resolve_type_expr`. `constraint:` entries are resolved as `(typevar_name,
class_name)` pairs and registered in `ann_mapping` and `state.constraints`.
`doc:` is stored as a string. The `Simple(name)` path is unchanged.

**Impact:** Moderate. Localised to `resolve_fn_type` and surrounding annotation
infrastructure. The `fn@Type` shorthand path is untouched.

### `src/types.rs` — TypeScheme

**Current:** `TypeScheme` holds `type_vars`, `row_vars`, `constraints`, and
`body`. No doc string field.

**Proposed:** Add `doc: Option<String>` to `TypeScheme`. The field is `None` for
all inferred schemes; set only when a `fn@[doc: "..."]` annotation is present.
The `doc` field is populated exclusively during function annotation resolution
(`resolve_fn_metadata`), not during general annotation resolution — non-function
bindings always have `doc: None`.

**Impact:** Minor. One new optional field; all construction sites default to
`None`.

### `src/lsp/analysis.rs` — hover display

**Current:** Hover shows the inferred type scheme. No doc string integration for
function types.

**Proposed:** When a function binding's `TypeScheme` has `doc: Some(text)`,
append the text to the hover output below the type signature.

**Impact:** Minor. Additive change to hover rendering.

### `doc/04-functions.md` and `doc/05-type-annotations.md`

**Current:** Documents `fn@Type` as the only annotation form. No coverage of the
dict form.

**Proposed:** Add documentation for the full `fn@[return: ... constraint: ...
doc: ...]` form. Update the annotation reference in `doc/05-type-annotations.md` with
the constraint syntax and its interaction with TypeVar scoping.

**Impact:** Minor. Documentation only.

### `stdlib/prelude.llt` — annotation migration (optional)

**Current:** 295 occurrences of `fn@Type` shorthand. All remain valid.

**Proposed:** Functions that benefit from constraint or doc annotations migrate
to the full form. Specifically: `min`, `max`, `sorted`, `sort-by` gain
`constraint: [a: Comparable]`; `fold`, `reduce` gain doc strings. Migration is
voluntary — the shorthand is permanent.

**Impact:** Minor. Source-only changes to annotation strings; no semantic change
for migrated functions (constraints were already inferred from the body).

## Prerequisites

The basic `fn@[return: ... constraint: ... doc: ...]` form works with the
existing hardcoded constraint infrastructure (`satisfies_constraint` in
`src/type_unify.rs`). No HKT dependency for the core implementation.

The `constraint:` key becomes fully expressive — accepting user-defined class
names in addition to built-in classes — once `hkt-mappable-appendable` is
complete, which migrates all built-in classes to proper `[class ...]`
declarations and makes `ClassEnv` the authoritative source for constraint
validation.

## References

- Cardelli, L. & Wegner, P. (1985). "On Understanding Types, Data Abstraction,
  and Polymorphism." *ACM Computing Surveys 17(4).* — [type annotation as
  documentation and enforcement]
- Wadler, P. & Blott, S. (1989). "How to make ad-hoc polymorphism less
  ad hoc." *POPL 1989.* — [type class constraints as the model for constraint
  annotations]
- Elm documentation. "Constrained Type Variables." — [named-constraint TypeVar
  precedent; tinct rejects the magic-name approach in favour of explicit
  `constraint:` syntax]
