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

## Approaches

### Approach C: Hybrid — Implicit Default, Explicit Opt-In

Default to current behavior (shared free variables). Add optional
explicit parameters for aliases that need fresh instantiation:

```lisp
# No parameters — current behavior (shared variables)
Mapper: [type [Fn@b [a]]]

# Explicit parameters — fresh instantiation per use
Pair: [type [a] [first: a  second: a]]
```

Non-parameterized aliases behave exactly as today. Parameterized aliases
get fresh instantiation.

**Pros:**
- Fully backward compatible
- Users opt into parameterization only when needed
- Covers both use cases (shared vars and fresh instantiation)

**Cons:**
- Two mental models for type aliases
- Users must know when to use parameters vs. not

## What Would Change

### AST

`Expr::TypeAlias` gains an optional parameter list:

```rust
/// Type alias: [type body] or [type [params] body]
TypeAlias {
    params: Vec<String>,         // empty for non-parameterized
    body: Box<Spanned<Expr>>,
}
```

Currently `TypeAlias(Box<Spanned<Expr>>)` — the inner expression is the
body. With parameters, the parser must distinguish `[type [a b] [Fn@b [a]]]`
(parameterized) from `[type [Fn@b [a]]]` (non-parameterized).

### Parser

The parser must detect whether the first `[]` inside `[type ...]` is a
parameter list or the body:

- If the first `[]` contains only lowercase bare words (no `:`, no `@`,
  no uppercase words, no literals): it's a parameter list, and the next
  expression is the body.
- Otherwise: it's the body (zero parameters).

This is unambiguous because type bodies always contain either uppercase
type names, `:` for record fields, or `@` for function types.

### Type Checker

1. **Registration:** `register_type_aliases()` stores `TypeAlias { params, body }`
   instead of plain `Type`.

2. **Resolution:** When resolving a type expression that references a
   parameterized alias (e.g., `[Pair Int]`), build a substitution from
   parameter names to provided type arguments, then apply to the body.

3. **Arity check:** If an alias has `n` parameters and is applied to `m`
   arguments where `m ≠ n`, report a type error.

4. **Zero-parameter aliases:** Unchanged from current behavior — stored
   with empty `params`, expanded by name, free variables connect by name.

### Interaction with Let-Generalization

Parameterized type aliases are **not** polymorphic let-bindings. They are
syntactic abbreviations expanded before inference. The distinction:

- **Type alias:** `Pair: [type [a] [first: a second: a]]` — textual
  expansion, no quantification, no instantiation machinery
- **Let-generalization:** `id: [fn [x] $x]` gets type `∀a. a → a` —
  the type scheme quantifies `a` and each use instantiates fresh variables

Parameterized aliases use simple substitution at the alias usage site.
This is the same model as Haskell's `type` aliases (as opposed to
`newtype` or `data` declarations).

## Recommendation

**Approach C: Hybrid — keep current behavior for non-parameterized
aliases, add explicit parameter lists for aliases that need them.**

### Rationale

1. **Backward compatible.** All existing `[type ...]` aliases continue
   to work identically. No migration needed.

2. **Approach B breaks existing patterns.** Implicit freshening would
   disconnect type variables that currently unify by name across function
   signatures. The `apply-mapper` example above shows this is a real
   problem, not a theoretical one.

3. **Explicit parameters are standard.** Haskell, TypeScript, OCaml, F#,
   Scala all use explicit parameter lists for parameterized type aliases.
   The syntax `[type [a b] body]` is natural in tinct's bracket syntax.

4. **Arity checking catches real errors.** Without parameters, there's
   no way to detect that `Pair` was used with the wrong number of type
   arguments. With explicit parameters, `[Pair Int String Bool]` is an
   immediate type error.

5. **Foundation for type classes.** If type classes are adopted (see
   `doc/whatif/typeclasses.md`), type constructors like `Functor f`
   require `f` to be a parameterized type. Explicit parameters establish
   that `Pair` takes one argument, `Mapper` takes two, etc.

### Phased Adoption

#### Phase 1: Parameterized Alias Registration

Add the `params` field to `TypeAlias`. Parser recognizes
`[type [lowercase-words] body]` as parameterized. Type checker stores
the parameter names. Zero-parameter aliases unchanged.

**Trigger:** The first time a user encounters the name collision problem
(two aliases sharing a variable name that shouldn't unify), or when the
type checker is mature enough that alias arity checking has value.

#### Phase 2: Alias Application in Annotations

Support `[AliasName Arg1 Arg2]` in type annotation positions. The type
checker resolves the alias, checks arity, builds the substitution, and
returns the instantiated type.

```lisp
[
  Pair: [type [a] [first: a  second: a]]
  pair-of-ints: [fn@[Pair Int] [] [first: 1  second: 2]]
]
```

#### Phase 3: Partial Application (Deferred)

Allow applying fewer arguments than parameters:
`[Mapper Int]` with `Mapper: [type [a b] [Fn@b [a]]]` produces a
one-parameter alias equivalent to `[type [b] [Fn@b [Int]]]`.

This is a convenience, not a necessity. Defer until partial application
patterns emerge in user code.

### Prerequisites

- Type alias shadowing policy implemented (already decided — allow lexical
  shadowing, `TODO.md` type-extensions sprint)
- Let-generalization complete (parameter instantiation reuses substitution
  machinery)

### Trigger

Adopt when:
- Variable name collision causes a real type error that confuses a user
- The type system needs arity-checked type constructors (prerequisite for
  type classes)
- Users request generic record type definitions for reusable config schemas

The TODO.md item correctly notes: "Deferred until variable name collision
becomes a real problem. Textual expansion is sufficient for now." This
remains the right call — parameterized aliases add value only when the
collision problem actually manifests.

## References

- Haskell Report §4.2.2: Type synonym declarations.
  `type Pair a b = (a, b)` — textual expansion with explicit parameters.
  Not recursive. Fully applied at every use site.
- TypeScript Handbook: Generic type aliases.
  `type Pair<A, B> = { first: A; second: B }` — explicit type parameters,
  instantiated at use.
- OCaml Manual §1.8: Type definitions.
  `type ('a, 'b) pair = { first: 'a; second: 'b }` — explicit parameters,
  always fully applied.
- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press.
  Chapter 11 (simple extensions, type abbreviations).
