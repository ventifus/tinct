# What If: Parameterized Type Aliases for tinct

What would it take to add parameterized (generic) type aliases to tinct's
type system?

## Current State

tinct has simple type aliases via `[type ...]`:

```lisp
[
  Person: [type [name: String  age: Number]]
  Mapper: [type [Fn@b [a]]]
  Predicate: [type [Fn@Bool [a]]]
]
```

These are textual expansions. `Mapper` expands to `Function { params: [TypeVar("a")], ret: TypeVar("b") }`. The free type variables `a` and `b` connect by name — when `Mapper` is used in an annotation, unification binds these variables against the context.

**Implementation:** `register_type_aliases()` in `typecheck.rs:112-135`
resolves the inner type expression and stores the result in
`TypeEnv::type_aliases` as a plain `Type`. `get_type_alias()` looks up
through the parent chain. No parameter tracking, no instantiation.

### What Works

Free-variable-based aliases handle many cases:

```lisp
[
  Mapper: [type [Fn@b [a]]]
  map: [fn@[b] [f@Mapper  xs@[a]] ...]
]
```

Here `a` and `b` in `Mapper` unify with `a` and `b` in the function
signature because they share names. This is simple and works.

### What Doesn't Work

**Name collision.** If two aliases use the same variable name for
different purposes, they silently unify:

```lisp
[
  Pair: [type [first: a  second: a]]    # both fields same type
  Box:  [type [value: a  label: String]]

  # PROBLEM: a in Pair and a in Box are the SAME variable
  wrap: [fn [p@Pair  b@Box] ...]
  # Unification binds a to one type — Pair.first, Pair.second,
  # AND Box.value all forced to the same type
]
```

With parameterized aliases, each use would instantiate fresh variables:

```lisp
[
  Pair: [type [a] [first: a  second: a]]
  Box:  [type [a] [value: a  label: String]]

  # Each alias instantiates its own fresh variables
  wrap: [fn [p@[Pair Int]  b@[Box String]] ...]
  # Pair Int  → [first: Int  second: Int]
  # Box String → [value: String  label: String]
]
```

**Multi-instance ambiguity.** Using the same alias twice with different
intended type arguments:

```lisp
[
  Mapper: [type [Fn@b [a]]]

  # PROBLEM: two Mappers that should have different types
  compose: [fn [f@Mapper  g@Mapper] ...]
  # Both share the same a and b — f and g forced to identical type
]
```

With parameters:
```lisp
[
  Mapper: [type [a b] [Fn@b [a]]]
  compose: [fn [f@[Mapper b c]  g@[Mapper a b]] [Mapper a c]]
]
```

**No arity checking.** Current aliases don't declare how many type
variables they expect. `Pair` might use `a` and `b`, but nothing prevents
referencing it with the wrong number of variables. Parameterized aliases
would enforce arity.

## What Parameterized Type Aliases Would Provide

1. **Fresh instantiation** — each use of `[Pair Int]` gets fresh internal
   variables, preventing cross-alias collision
2. **Arity enforcement** — `[Pair Int]` with one argument when `Pair`
   expects one is checked; `[Pair Int String]` would be an error
3. **Partial application** — `[Mapper Int]` could produce a
   one-parameter alias `[Fn@b [Int]]` (if supported)
4. **Documentation** — parameters explicitly declare what's generic,
   making aliases self-documenting
5. **Foundation for type constructors** — parameterized aliases are a
   prerequisite for higher-kinded types and type classes (`Functor f`
   requires `f` to be a type constructor)

## Design

Hybrid approach: keep current behavior for non-parameterized aliases,
add explicit parameter lists for aliases that need fresh instantiation.
This is fully backward compatible — all existing `[type ...]` aliases
continue to work identically.

### Syntax

```lisp
# No parameters — current behavior (shared variables)
Mapper: [type [Fn@b [a]]]

# Explicit parameters — fresh instantiation per use
Pair: [type [a] [first: a  second: a]]

# Application in annotations
pair-of-ints: [fn@[Pair Int] [] [first: 1  second: 2]]
compose: [fn [f@[Mapper b c]  g@[Mapper a b]] [Mapper a c]]
```

Non-parameterized aliases behave exactly as today. Parameterized
aliases get fresh instantiation at each use site.

### Semantics

Parameterized type aliases are **syntactic abbreviations**, not
polymorphic type schemes. The distinction:

- **Type alias:** `Pair: [type [a] [first: a  second: a]]` — textual
  expansion with substitution, no quantification
- **Let-generalization:** `id: [fn [x] x]` gets type `forall a. a -> a`
  — the type scheme quantifies `a` and each use instantiates fresh
  variables via the inference algorithm

When `[Pair Int]` appears in a type annotation position, the type
checker:
1. Looks up `Pair` in the alias environment
2. Checks arity: `Pair` declares 1 parameter, 1 argument provided
3. Builds substitution `{a |-> Int}`
4. Applies substitution to the body: `[first: Int  second: Int]`
5. Returns the resulting type for unification with the context

This is the same model as Haskell's `type` synonyms (as opposed to
`newtype` or `data` declarations). Aliases are always fully expanded
before unification — no alias names appear in inferred types.

### Interaction with Row Polymorphism

Parameterized aliases can produce record types with row variables:

```lisp
Extensible: [type [a] [name: String  ..a]]
```

When `[Extensible r]` is used, the substitution `{a |-> r}` splices
`r` into the row variable position. This is sound because row variable
substitution is already implemented in `types.rs` — the alias expansion
happens before unification, so the row variable enters the type system
through the same path as a hand-written record type with a row variable.

The key constraint: alias parameters that appear in row variable
position must be substituted with row-kinded types. If `[Extensible Int]`
is written, the resulting `[name: String  ..Int]` is ill-formed. The
type checker should detect this during unification (Int cannot unify
with a row variable), producing an error at the alias application site
rather than deep inside the unifier.

### Interaction with Type Inference

Alias expansion happens during type annotation resolution, before
inference begins. This means:

1. **No impact on principal types.** Alias expansion is a preprocessing
   step — it does not change the inference algorithm or its guarantees.
2. **No impact on unification.** The unifier never sees alias names,
   only their expanded forms.
3. **No impact on let-generalization.** Aliases are expanded in
   annotation positions; the inference algorithm's generalization
   step operates on the expanded types.

The one subtlety: when a parameterized alias is used without full
application (e.g., `Pair` appears bare in an annotation without
arguments), it should be treated as the current non-parameterized
behavior (free variables connect by name). This preserves backward
compatibility for aliases that are used both with and without
parameters during a migration period.

## What Would Change

### AST (`parser.rs`)

**Current:** `Expr::TypeAlias(Box<Spanned<Expr>>)` — the inner
expression is the body. No parameter tracking.

**Proposed:** `Expr::TypeAlias` gains an optional parameter list:

```rust
/// Type alias: [type body] or [type [params] body]
TypeAlias {
    params: Vec<String>,         // empty for non-parameterized
    body: Box<Spanned<Expr>>,
}
```

**Impact:** Minor — structural change to one AST variant.

### Parser (`parser.rs`)

**Current:** `[type body]` parses the inner expression as the alias body.

**Proposed:** The parser must detect whether the first `[]` inside
`[type ...]` is a parameter list or the body:

- If the first `[]` contains only lowercase bare words (no `:`, no `@`,
  no uppercase words, no literals): it's a parameter list, and the next
  expression is the body.
- Otherwise: it's the body (zero parameters).

This is unambiguous because type bodies always contain either uppercase
type names, `:` for record fields, or `@` for function types.

**Impact:** Minor — localized change to `type` form parsing.

### Type Checker (`typecheck.rs`)

**Current:** `register_type_aliases()` resolves the inner type expression
and stores the result in `TypeEnv::type_aliases` as a plain `Type`.
`get_type_alias()` looks up through the parent chain. No parameter
tracking, no instantiation.

**Proposed:**
1. **Registration:** `register_type_aliases()` stores
   `TypeAlias { params, body }` instead of plain `Type`.
2. **Resolution:** When resolving a type expression that references a
   parameterized alias (e.g., `[Pair Int]`), build a substitution from
   parameter names to provided type arguments, then apply to the body.
3. **Arity check:** If an alias has `n` parameters and is applied to `m`
   arguments where `m != n`, report a type error.
4. **Zero-parameter aliases:** Unchanged from current behavior — stored
   with empty `params`, expanded by name, free variables connect by name.

**Impact:** Moderate — new resolution logic and arity checking in
`register_type_aliases()` and type annotation resolution.

### Unification (`types.rs`)

**Current:** Unification never sees alias names — aliases are expanded
to their body types before unification.

**Proposed:** No change. Parameterized alias expansion still happens
before unification. The unifier continues to operate on expanded types.

**Impact:** None.

## Phased Adoption

### Phase 1: Parameterized Alias Registration

Add the `params` field to `TypeAlias`. Parser recognizes
`[type [lowercase-words] body]` as parameterized. Type checker stores
the parameter names. Zero-parameter aliases unchanged. This phase is
independently useful for documentation — explicit parameters declare
what's generic, even before application syntax is supported.

### Phase 2: Alias Application in Annotations

Support `[AliasName Arg1 Arg2]` in type annotation positions. The type
checker resolves the alias, checks arity, builds the substitution, and
returns the instantiated type.

```lisp
[
  Pair: [type [a] [first: a  second: a]]
  pair-of-ints: [fn@[Pair Int] [] [first: 1  second: 2]]
]
```

### Phase 3: Partial Application (Deferred)

Allow applying fewer arguments than parameters:
`[Mapper Int]` with `Mapper: [type [a b] [Fn@b [a]]]` produces a
one-parameter alias equivalent to `[type [b] [Fn@b [Int]]]`.

This is a convenience, not a necessity. Defer until partial application
patterns emerge in user code. Partial application of type aliases
corresponds to type-level currying and is a prerequisite for
higher-kinded type variables (`Functor f` requires `f` to accept one
type argument).

### Prerequisites

- Type alias shadowing policy implemented (already decided — allow
  lexical shadowing, `TODO.md` type-extensions sprint).
- Let-generalization complete (parameter instantiation reuses the
  substitution machinery from `types.rs`).
- Phase 2 requires Phase 1.
- Phase 3 requires Phase 2 and is only motivated by type class adoption
  (`doc/whatif/typeclasses.md`).

### Trigger

- When variable name collision causes a real type error that confuses
  a user
- When the type system needs arity-checked type constructors
  (prerequisite for type classes)
- When users request generic record type definitions for reusable
  config schemas

The TODO.md item correctly notes: "Deferred until variable name
collision becomes a real problem. Textual expansion is sufficient for
now." This remains the right call — parameterized aliases add value
only when the collision problem actually manifests.

## References

- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press.
  Chapter 11 (simple extensions, type abbreviations). — Formalizes
  type abbreviations as syntactic sugar expanded before type checking.
  Establishes that alias expansion preserves principal types when
  aliases are non-recursive.
- Damas, L. & Milner, R. (1982). "Principal type-schemes for functional
  programs." In *POPL '82*, pp. 207--212. ACM. — Principal type
  guarantee that parameterized alias expansion must preserve. Since
  aliases expand before inference, the guarantee holds trivially.
- Remy, D. (1994). "Type inference for records in a natural extension
  of ML." In *Theoretical Aspects of Object-Oriented Programming*,
  pp. 67--95. MIT Press. — Row polymorphism that parameterized aliases
  must interact with correctly when alias bodies contain row variables.
- Haskell Report §4.2.2. Type synonym declarations. —
  `type Pair a b = (a, b)` — textual expansion with explicit parameters.
  Not recursive. Fully applied at every use site. Direct precedent for
  tinct's design.
- TypeScript Handbook. Generic type aliases. —
  `type Pair<A, B> = { first: A; second: B }` — explicit type
  parameters, instantiated at use. Shows the pattern in a structural
  type system.
- OCaml Manual §1.8. Type definitions. —
  `type ('a, 'b) pair = { first: 'a; second: 'b }` — explicit
  parameters, always fully applied. OCaml enforces full application
  of type synonyms, which is the same constraint tinct would enforce
  in Phases 1--2.
