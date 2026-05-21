# Parameterized Type Aliases

## Overview

Parameterized type aliases extend tinct's `[type ...]` form with explicit parameter lists, enabling fresh instantiation per use site and arity-checked type constructors.

Non-parameterized aliases continue to work exactly as before. Parameterized aliases get fresh instantiation at each use site, preventing cross-alias variable collision and enabling reusable generic record types.

The feature provides:

1. **Fresh instantiation** — each use of `[Pair Int]` gets fresh internal variables, preventing cross-alias collision
2. **Arity enforcement** — `[Pair Int]` with one argument when `Pair` expects one is checked; `[Pair Int String]` is an error
3. **Partial application** — `[Mapper Int]` produces a one-parameter alias `[Fn@b [Int]]` (co-scheduled with type classes)
4. **Documentation** — parameters explicitly declare what's generic, making aliases self-documenting
5. **Foundation for type constructors** — parameterized aliases are a prerequisite for higher-kinded types and type classes (`Functor f` requires `f` to be a type constructor)

> **⚠ Not implemented:** Partial application is rejected — the type checker enforces exact arity and errors on mismatch.

## Supersession Notes

- **`TypeScheme.row_vars`**: The `row_vars` field was removed when BAS replaced Rémy-style row polymorphism. All records are closed under BAS. See [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09).
- **Partial application**: The feature doc describes partial application as a design option. The implemented behavior enforces exact arity — partial application is rejected with an error.

## Design

Hybrid approach: current behavior for non-parameterized aliases, explicit parameter lists for aliases that need fresh instantiation. This is fully backward compatible — all existing `[type ...]` aliases continue to work identically.

### Syntax

```tinct
# No parameters — current behavior (shared variables)
Mapper: [type [Fn@b [a]]]

# Explicit parameters — fresh instantiation per use
Pair: [type [a] [first: a  second: a]]

# Application in annotations
pair-of-ints: [fn@[Pair Int] [] [first: 1  second: 2]]
compose: [fn [f@[Mapper b c]  g@[Mapper a b]] [Mapper a c]]
```

### The Problem Solved

**Name collision.** Without parameters, if two aliases use the same variable name for different purposes, they silently unify:

```tinct
[
  Pair: [type [first: a  second: a]]    # both fields same type
  Box:  [type [value: a  label: String]]

  # PROBLEM: a in Pair and a in Box are the SAME variable
  wrap: [fn [p@Pair  b@Box] ...]
  # Unification binds a to one type — Pair.first, Pair.second,
  # AND Box.value all forced to the same type
]
```

With parameterized aliases, each use instantiates fresh variables:

```tinct
[
  Pair: [type [a] [first: a  second: a]]
  Box:  [type [a] [value: a  label: String]]

  # Each alias instantiates its own fresh variables
  wrap: [fn [p@[Pair Int]  b@[Box String]] ...]
  # Pair Int  → [first: Int  second: Int]
  # Box String → [value: String  label: String]
]
```

**Multi-instance ambiguity.** Using the same alias twice with different intended type arguments, without parameters, forces both to the same type. With parameters:

```tinct
[
  Mapper: [type [a b] [Fn@b [a]]]
  compose: [fn [f@[Mapper b c]  g@[Mapper a b]] [Mapper a c]]
]
```

**No arity checking.** Parameterized aliases enforce arity — the correct number of arguments is checked at the alias application site.

### Semantics

Parameterized type aliases are **syntactic abbreviations**, not polymorphic type schemes. The distinction:

- **Type alias:** `Pair: [type [a] [first: a  second: a]]` — textual expansion with substitution, no quantification
- **Let-generalization:** `id: [fn [x] x]` gets type `forall a. a -> a` — the type scheme quantifies `a` and each use instantiates fresh variables via the inference algorithm

When `[Pair Int]` appears in a type annotation position, the type checker:

1. Looks up `Pair` in the alias environment
2. Checks arity: `Pair` declares 1 parameter, 1 argument provided
3. Builds substitution `{a |-> Int}`
4. Applies substitution to the body: `[first: Int  second: Int]`
5. Returns the resulting type for unification with the context

This is the same model as Haskell's `type` synonyms (as opposed to `newtype` or `data` declarations). Aliases are always fully expanded before unification — no alias names appear in inferred types.

### Interaction with Row Polymorphism

> **⚠ Superseded by [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09):** `row_vars` was removed from `TypeScheme`. All records are closed under BAS; there are no row-variable tails.

Parameterized aliases produce record types with row variables:

```tinct
Extensible: [type [a] [name: String  ..a]]
```

When `[Extensible r]` is used, the substitution `{a |-> r}` splices `r` into the row variable position. This is sound because row variable substitution is already implemented in `types.rs` — the alias expansion happens before unification, so the row variable enters the type system through the same path as a hand-written record type with a row variable.

The key constraint: alias parameters that appear in row variable position must be substituted with row-kinded types. If `[Extensible Int]` is written, the resulting `[name: String  ..Int]` is ill-formed. The type checker detects this during unification (Int cannot unify with a row variable), producing an error at the alias application site rather than deep inside the unifier.

### Interaction with Type Inference

Alias expansion happens during type annotation resolution, before inference begins. This means:

1. **No impact on principal types.** Alias expansion is a preprocessing step — it does not change the inference algorithm or its guarantees.
2. **No impact on unification.** The unifier never sees alias names, only their expanded forms.
3. **No impact on let-generalization.** Aliases are expanded in annotation positions; the inference algorithm's generalization step operates on the expanded types.

When a parameterized alias is used without full application (e.g., `Pair` appears bare in an annotation without arguments), it is treated as the current non-parameterized behavior (free variables connect by name). This preserves backward compatibility for aliases used both with and without parameters.

## Implementation

### AST (`parser.rs`)

`Expr::TypeAlias` carries an optional parameter list:

```rust
/// Type alias: [type body] or [type [params] body]
TypeAlias {
    params: Vec<String>,         // empty for non-parameterized
    body: Box<Spanned<Expr>>,
}
```

### Parser (`parser.rs`)

The parser detects whether the first `[]` inside `[type ...]` is a parameter list or the body:

- If the first `[]` contains only lowercase bare words (no `:`, no `@`, no uppercase words, no literals): it's a parameter list, and the next expression is the body.
- Otherwise: it's the body (zero parameters).

This is unambiguous because type bodies always contain either uppercase type names, `:` for record fields, or `@` for function types.

### Type Checker (`typecheck.rs`)

`register_type_aliases()` stores `TypeAlias { params, body }` instead of plain `Type`. When resolving a type expression that references a parameterized alias (e.g., `[Pair Int]`):

1. Build a substitution from parameter names to provided type arguments
2. Check arity: if an alias has `n` parameters and is applied to `m` arguments where `m != n`, report a type error
3. Apply the substitution to the body and return the resulting type

Zero-parameter aliases are unchanged from current behavior — stored with empty `params`, free variables connect by name.

### Unification (`types.rs`)

No change. Parameterized alias expansion still happens before unification. The unifier continues to operate on expanded types.

## References

- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press. Chapter 11 (simple extensions, type abbreviations). — Formalizes type abbreviations as syntactic sugar expanded before type checking. Establishes that alias expansion preserves principal types when aliases are non-recursive.
- Damas, L. & Milner, R. (1982). "Principal type-schemes for functional programs." In *POPL '82*, pp. 207--212. ACM. — Principal type guarantee that parameterized alias expansion must preserve. Since aliases expand before inference, the guarantee holds trivially.
- Remy, D. (1994). "Type inference for records in a natural extension of ML." In *Theoretical Aspects of Object-Oriented Programming*, pp. 67--95. MIT Press. — Row polymorphism that parameterized aliases must interact with correctly when alias bodies contain row variables.
- Haskell Report §4.2.2. Type synonym declarations. — `type Pair a b = (a, b)` — textual expansion with explicit parameters. Not recursive. Fully applied at every use site. Direct precedent for tinct's design.
- TypeScript Handbook. Generic type aliases. — `type Pair<A, B> = { first: A; second: B }` — explicit type parameters, instantiated at use. Shows the pattern in a structural type system.
- OCaml Manual §1.8. Type definitions. — `type ('a, 'b) pair = { first: 'a; second: 'b }` — explicit parameters, always fully applied. OCaml enforces full application of type synonyms, which is the same constraint tinct enforces.
