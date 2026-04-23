# What If: Union Types for tinct

What would it take to add union types to tinct's type system?

## Current State

tinct cannot express "a value that is either type A or type B." The main
consequence: dual-dispatch builtins (`$map`, `$filter`, `$take`, `$drop`,
`$reduce`, `$join`) accept both Dict and Seq but are typed as `Any` because
`Dict | Seq` is not expressible.

```
# Current: imprecise
$map : Any → Any → Any

# Desired: precise
$map : (a → b) → (Dict a | Seq a) → (Dict b | Seq b)
```

Other uses for union types:
- `$if` return type: `$if $cond $then-branch $else-branch` should have type
  `type($then) | type($else)`, not `Any`
- Nullable values: `Int | Null` instead of `Any`
- Error returns: `Result a = [ok: a] | [err: String]`

### What's Missing

1. **No `Type::Union` variant.** The type representation has no way to
   express "A or B" — the only escape hatch is `Any`.
2. **No union subtyping rules.** `is_subtype` has no injection or
   elimination rules for unions.
3. **No syntax for union annotations.** The grammar cannot parse `Int | Str`
   in type position.
4. **`$if` returns `Any`.** Branch result types are joined via `unify`, which
   either finds a common type or falls back to `Any`.
5. **Dual-dispatch builtins are untyped.** Builtins that accept both Dict
   and Seq are typed as `Any → Any` because the argument type cannot be
   expressed precisely.

## What Union Types Would Provide

1. **Precise typing for dual-dispatch builtins.** `$map`, `$filter`, etc.
   could be typed as `(a → b) → (Dict a | Seq a) → (Dict b | Seq b)`
   instead of `Any → Any → Any`.
2. **Nullable value types.** `Int | Null` instead of collapsing to `Any`,
   enabling the type checker to enforce null handling.
3. **Tagged union / result types.** `[ok: a] | [err: String]` for error
   handling patterns, with static checking that both cases are handled.
4. **More precise `$if` return types.** `$if $cond 42 "hello"` could have
   type `Int | Str` instead of `Any`.
5. **Foundation for sum types.** Union types are a prerequisite for
   algebraic data types and discriminated union patterns.

## Design

### Annotation-Only Unions (Core Design)

The core design adds unions as an annotation-level construct: unions appear
in explicit type annotations and builtin signatures, but `unify` never
produces them. This is the conservative approach — it avoids the complexity
of algebraic subtyping while covering the primary use cases.

#### Syntax

Union types use `|` in type annotation position:

```lisp
# In type annotations
[fn [x : Int | Str] ...]

# In builtin signatures (internal)
$map : (a → b) → (Dict a | Seq a) → (Dict b | Seq b)

# Named type aliases
Result: [type [ok: a] | [err: String]]
```

#### Internal Representation

```rust
enum Type {
    // ... existing variants ...
    Union(Vec<Type>),  // invariant: sorted, deduplicated, flattened (no nested unions)
}
```

Unions are maintained in a canonical form: flattened (no `Union` inside
`Union`), sorted by a stable type ordering, and deduplicated. This ensures
`Int | Str` and `Str | Int` are the same `Type` value.

#### Subtyping Rules

Three new rules extend `is_subtype`:

```
[UNION-INJ-L]  A <: A | B
[UNION-INJ-R]  B <: A | B
[UNION-ELIM]   If A <: C and B <: C, then A | B <: C
```

These are the standard covariant union rules from Pierce (2002), Chapter 15.
They are decidable and preserve transitivity.

#### Unification Behavior

`unify(Int, Str)` still fails. Unions only appear in annotations, not from
inference. When checking `expr : Int | Str`, the type checker uses
`is_subtype(inferred, Int | Str)` in checking mode — unification is not
involved.

This means `$if true 1 "hello"` still infers `Any` (no union from
inference). Inferred unions require algebraic subtyping (Phase 3).

#### Interaction with Row Polymorphism

Union types interact with records: `[x: Int] | [y: Str]` is a union of two
record types, not a single record. Width subtyping and row variable
unification remain unchanged — unions are orthogonal to row polymorphism
at the annotation-only level.

The deeper interaction (row polymorphism with algebraic subtyping) is
addressed by Lorenz et al. (2024) and deferred to Phase 3.

#### Interaction with `Any`

`Any` is currently used where union types would be more precise. With
annotation-only unions, `Any` remains the inferred join for incompatible
types. Splitting `Any` into `Unknown` (gradual) + `Top` (subtyping ceiling)
is a prerequisite for full union semantics — see `doc/whatif/gradual-typing.md`.

## What Would Change

### Type Representation (`src/types.rs`)

**Current:** `Type` enum has no union variant. Incompatible types cause
unification failure or collapse to `Any`.
**Proposed:** Add `Union(Vec<Type>)` variant with canonical form invariant.
Add `normalize_union()` for flattening, sorting, deduplication.
**Impact:** Moderate — new variant propagates through `is_subtype`,
`apply_substitution`, `collect_type_vars`, `display`. Does not affect
`unify`.

### Subtyping (`src/types.rs` — `is_subtype`)

**Current:** No union rules. Subtyping covers literals, base types, records,
functions, sequences.
**Proposed:** Add `[UNION-INJ-L]`, `[UNION-INJ-R]`, `[UNION-ELIM]` rules.
`is_subtype(T, Union(ts))` succeeds if `T <: t_i` for some `t_i`.
`is_subtype(Union(ts), T)` succeeds if `t_i <: T` for all `t_i`.
**Impact:** Minor — three new match arms, no changes to existing rules.

### Type Checker (`src/typecheck.rs`)

**Current:** No special handling for union annotations. Checking mode uses
`is_subtype` for concrete expected types.
**Proposed:** Extend checking mode to handle `Union` expected types (already
covered by new `is_subtype` rules). Parse union type annotations into
`Type::Union`. No changes to inference mode — unions are not inferred.
**Impact:** Minor — annotation parsing and subtype checking, no new
inference logic.

### Parser (`src/parser.rs`, `src/grammar.pest`)

**Current:** Type annotation grammar does not include `|`.
**Proposed:** Add `|` as a type-level operator in annotation position.
Precedence: `|` binds looser than `→` (function arrow).
**Impact:** Minor — grammar extension in annotation position only.

### Evaluator (`src/eval.rs`)

**Current:** No runtime representation of unions.
**Proposed:** No changes. Unions are erased at runtime — they exist only
in the type system. Values are `Int` or `Str`, never `Int | Str`.
**Impact:** None.

### Builtins (`src/builtins.rs`)

**Current:** Dual-dispatch builtins typed as `Any → Any`.
**Proposed:** Update builtin type signatures to use `Union` types.
`$map : (a → b) → Union(Dict(a), Seq(a)) → Union(Dict(b), Seq(b))`.
**Impact:** Minor — signature changes only, no runtime behavior changes.

## Phased Adoption

### Phase 1: Type Classes (no union types)

Solve the dual-dispatch typing problem without union types by adding
constrained polymorphism (see `doc/whatif/typeclasses.md`):

```
$map : Functor f => (a → b) → f a → f b
```

where `Functor` has instances for Dict and Seq. This covers the primary
motivation (precise typing for `$map`, `$filter`, etc.) without any union
type machinery.

### Phase 2: Annotation-Only Unions

Add `Type::Union(Vec<Type>)` with subtyping rules `[UNION-INJ-L]`,
`[UNION-INJ-R]`, `[UNION-ELIM]`. Unions appear only in explicit type
annotations and builtin signatures — `unify` never produces them.

This enables:
- `Int | Null` for nullable values
- `[ok: a] | [err: String]` for result types
- Explicit union annotations on user-defined functions

Implementation: add `Union` variant to `Type`. Extend `is_subtype` with
three new rules. Extend `is_consistent` (from gradual typing, see
`doc/whatif/gradual-typing.md`) for union consistency. No changes to
`unify` — unions are not inferred, only checked.

### Phase 3: Inferred Unions (Simple-sub)

If annotation-only unions prove insufficient — specifically, if `$if`
return types and other inferred positions need unions — adopt Simple-sub
(Parreaux 2020) for full algebraic subtyping with inferred union and
intersection types. See `doc/whatif/algebraic-subtypes.md` for the
complete analysis.

This is the heaviest phase: `unify` becomes `constrain`, type variables
carry bounds instead of bindings, and `Any` must split into
Top/Bottom/Gradual. Only adopt if Phases 1–2 are insufficient.

### Prerequisites

- Phase 1 requires `let-generalization` and `builtin-type-signatures`
- Phase 2 requires Phase 1 (type classes provide the Functor abstraction
  that makes unions less urgent) and `gradual-typing` Phase 2 (splitting
  `Any` into `Unknown` + `Top`)
- Phase 3 requires `doc/whatif/algebraic-subtypes.md` adoption

### Trigger

Phase 1 (type classes): see `doc/whatif/typeclasses.md` §Trigger.

Phase 2 (annotation unions): begin when:
- Nullable types are needed (`Int | Null` instead of `Any`)
- Tagged union / sum type patterns become common in user code
- `$try` result types need `[ok: a] | [err: String]` precision

Phase 3 (inferred unions): begin when:
- `$if` return types need inferred unions (not just annotated)
- Annotation-only unions create too much annotation burden
- Row polymorphism needs width subtyping built into the lattice

## References

- Dolan, S. & Mycroft, A. (2017). "Polymorphism, subtyping, and type
  inference in MLsub." In *POPL '17*, pp. 228–242. ACM.
  — Algebraic subtyping: union and intersection types with decidable
  inference. Foundation for Phase 3 inferred unions.
- Parreaux, L. (2020). "The simple essence of algebraic subtyping." In
  *ICFP '20*, Article 124. ACM.
  — Simplified presentation of MLsub. Direct implementation reference for
  Phase 3's `constrain`-based inference.
- Lorenz, J. et al. (2024). "Towards algebraic subtyping for extensible
  records." arXiv:2407.06747.
  — Addresses the interaction between algebraic subtyping and row
  polymorphism, directly relevant to union types over record types.
- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press.
  Chapter 15 (subtyping), Chapter 24 (existential types).
  — Standard subtyping rules for union types: [UNION-INJ-L], [UNION-INJ-R],
  [UNION-ELIM].
- Dunfield, J. & Pfenning, F. (2004). "Tridirectional typechecking."
  In *POPL '04*, pp. 281–292. ACM.
  — Datasort refinements and union/intersection types in a bidirectional
  checking framework. Relevant to checking mode for union annotations.
- Tobin-Hochstadt, S. & Felleisen, M. (2010). "Logical types for untyped
  languages." In *ICFP '10*, pp. 117–128. ACM.
  — Occurrence typing in Typed Racket: union types with path-sensitive
  narrowing. Practical reference for unions + narrowing interaction.
