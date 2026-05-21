# Union Types and Algebraic Subtyping

## Overview

Union types exist at two levels in tinct. Annotation-only unions
(`x@[Int Null]`) express "A or B" in explicit type annotations and builtin
signatures without altering inference. Full algebraic subtyping (Simple-sub)
replaces Robinson unification with polarity-aware constraint solving, enabling
inferred union and intersection types throughout the type system.

## Supersession Notes

Parts of this feature were modified by later features:

- **§Full Algebraic Subtyping (Simple-sub)**: The Simple-sub (Parreaux 2020) description was superseded by Boolean-Algebraic Subtyping. The codebase implements BAS (Chau & Parreaux, POPL 2026) — a hybrid system retaining HM unification alongside BAS subtyping via `constrain()`. See [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09).
- **§Interaction with `Any`**: `Type::Any` was split into `Type::Unknown` (gradual typing opt-out) and `Type::Top` (true supertype). Any section referring to `Type::Any` uses stale terminology. See [gradual-typing.md](gradual-typing.md) (2026-05-07) and [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09).
- **`try` result type**: `try` returns `Value::Variant { tag: "Ok"/"Err" }` (nominal), not structural `{ok: v}/{err: msg}`. See [error-patterns.md](error-patterns.md) (2026-05-09).
- **S-RcdTop**: Under BAS, disjoint single-field record unions like `{ok: T} | {err: S}` collapse to `Top` via S-RcdTop (`src/types.rs:882`). Structural discriminated unions of this form are not valid ADTs. Use [nominal variants](nominal-variants.md) instead.

## Design

### Annotation-Only Unions

The core design adds unions as an annotation-level construct: unions appear in
explicit type annotations and builtin signatures, but `unify` never produces them.

#### Syntax

Union types use **positional entries in `@[…]` annotations**. No infix operator.
This avoids collision with the `|` pipe operator (access-pipeline) and extends
the existing annotation model naturally: positional entries are type-union members,
named entries are metadata.

```tinct
# Positional entries in @[...] are collected and unioned
x@[Int Str]                          # type: Int | Str
x@[String Null]                      # type: String | Null
x@[String Null default: ""]         # type: String | Null, with default metadata

# In function parameters
[fn [x@[Int Str]] ...]               # x accepts Int or Str
[fn@[String Null] [name@String] ...] # returns String or Null

# In type aliases
Result: [type [ok: a] [err: String]] # union of two record types

# Shorthand still works for single types — unchanged
x@String                             # equivalent to x@[type: String]
```

**Desugar rule.** Positional entries in an annotation dict are moved to the
`type:` key as a list, preserving the existing annotation resolution path:

```text
x@[T1 T2 ...named...]  →  x@[type: [T1 T2]  ...named...]
x@[T]                  →  x@[type: T]         (single positional unwraps)
x@T                    →  x@[type: T]         (existing shorthand, unchanged)
```

The type resolver then handles `type: [T1 T2]` as `Union(T1, T2)`. This is
backward-compatible: `x@[type: Number  default: 30]` has no positional entries
and is unchanged.

**Param lists are unaffected.** `[Fn@Number [String Bool]]` means a two-param
function (String, Bool → Number) — the inner `[String Bool]` is in param-list
context, not annotation context, so it is not treated as a union. To express
a one-param function taking a union type: `[Fn@Number [@[String Bool]]]`.

#### Internal Representation

```rust
enum Type {
    // ... existing variants ...
    Union(Vec<Type>),  // invariant: sorted, deduplicated, flattened (no nested unions)
}
```

Unions are maintained in canonical form: flattened, sorted by stable type ordering,
and deduplicated. `x@[Int Str]` and `x@[Str Int]` resolve to the same `Type` value.

#### Subtyping Rules

Three new rules extend `is_subtype`:

```text
[UNION-INJ-L]  A <: A | B
[UNION-INJ-R]  B <: A | B
[UNION-ELIM]   If A <: C and B <: C, then A | B <: C
```

Standard covariant union rules from Pierce (2002, Chapter 15, §15.7).
Decidable, preserve transitivity and reflexivity. The join operation
(`A | B`) is the least upper bound when these rules are the only source
of unions. `unify(Int, Str)` still fails — unions only appear in
annotations, not from inference. Because `unify` never produces unions,
annotation-only unions do not interfere with Robinson's (1965) most general
unifier guarantee: the substitution produced by `unify` remains a valid
MGU for the non-union fragment, and union types appear only in checking
positions where `is_subtype` mediates.

#### Interaction with `Any`

> **⚠ Superseded:** `Type::Any` was split into `Type::Unknown` (gradual opt-out) and `Type::Top` (true supertype) as part of [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09). The split described here as "coordinated with gradual-typing Phase 2" is complete.

`Unknown` is the inferred join for incompatible types in gradual positions. `Top` is the subtyping ceiling.

### Full Algebraic Subtyping: Simple-sub

> **⚠ Superseded by [Boolean-Algebraic Subtyping](boolean-algebraic-subtyping.md) (2026-05-09):** The Simple-sub (Parreaux 2020) design described in this section was superseded before the unification constraints shipped publicly. The codebase implements BAS (Chau & Parreaux, POPL 2026) instead — a hybrid retaining HM `unify` alongside BAS `constrain()`. The description below is preserved for historical context.

Simple-sub (Parreaux 2020) replaces [U-SUBSUME] + Robinson unification as
the constraint-solving algorithm.

**Why Simple-sub over Dolan's biunification:** Dolan (2017) uses automata-theoretic
type simplification — theoretically elegant but complex and producing hard-to-read
types without simplification. Simple-sub achieves the same principal type guarantees
with a simpler algorithm (~500 lines for the core), bounds as concrete types rather
than automata states, and better error messages.

#### Core Mechanism

1. **`unify(t1, t2)` becomes `constrain(t1 <: t2)`** with polarity-aware structural decomposition:
   - `constrain(Fn(A → B) <: Fn(C → D))` → `constrain(C <: A)` + `constrain(B <: D)`
   - `constrain(Record(R1) <: Record(R2))` → constrain shared fields covariantly
2. **Type variables carry bounds instead of equality bindings:**

   ```rust
   struct TypeVarBounds {
       lower: Vec<Type>,  // types that are subtypes of this var (positive positions)
       upper: Vec<Type>,  // types that are supertypes of this var (negative positions)
   }
   ```

   Satisfiable iff `join(lower) <: meet(upper)`.
3. **Union and intersection types emerge from bound compaction** — not user-written
   syntax but inferred result types when a variable has multiple lower/upper bounds.
4. **[U-SUBSUME] eliminated** — subtyping is built into the solver.

#### Interaction with Row Polymorphism

Row variables become bounds-carrying variables tracking which fields *must* be
present (lower bound) and which *may* be present (upper bound). Width subtyping
is built into the lattice: `{x: Int, y: Str} <: {x: Int}` without extra machinery.
Marques, Florido & Vasconcelos (2024) extend Simple-sub with row variables
specifically, providing a direct implementation template.

#### Interaction with `Any`

`Any` as both top and bottom is unsound in algebraic subtyping — it collapses
the lattice. The split:

- `Top` — supertype of everything (TypeScript's `unknown`)
- `Bottom` — subtype of everything (TypeScript's `never`)
- `Unknown` — *consistent* with everything (gradual typing, separate relation)

Every use of `Any` was audited and reclassified. See [gradual-typing.md](gradual-typing.md) (2026-05-07) and [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09).

#### Interaction with Let-Generalization

Generalization still uses levels (Kiselyov 2013), but generalized variables carry
their bounds into the type scheme. Instantiation creates fresh variables with copies
of those bounds — not just fresh unconstrained variables.

#### Error Message Strategy

Each `constrain()` call records its source span and reason. When bounds are
unsatisfiable, the error traces back through the provenance chain to show *why*
bounds conflict, not just *that* they conflict. This is harder than point-of-failure
reporting but is the standard mitigation — MLsub's poor error quality was a known
problem; Simple-sub's concrete bounds help.

## Implementation

### For Annotation-Only Unions

| Component | Change | Impact |
|-----------|--------|--------|
| `src/types.rs` | Add `Union(Vec<Type>)` + `normalize_union()` | Moderate — propagates through `is_subtype`, `apply_substitution`, `collect_type_vars`, `display` |
| `src/types.rs` `is_subtype` | Add `[UNION-INJ-L]`, `[UNION-INJ-R]`, `[UNION-ELIM]` | Minor — three new match arms |
| `src/typecheck.rs` `resolve_annotation` | Collect positional entries from `Annotation::PropertyDict` into `type:` value as a list; resolve `type: [T1 T2]` as `Union(normalize(T1), normalize(T2))` | Minor |
| `src/eval.rs` | No changes; unions erased at runtime | None |
| `src/builtins.rs` | Update dual-dispatch signatures to use `Union` types | Minor |
| `doc/05-type-annotations.md` | Add generalized annotation model: positional entries = union members, named entries = metadata; update property table; add union examples | Minor |

### For Full Algebraic Subtyping

| Component | Change | Impact |
|-----------|--------|--------|
| `src/types.rs` | `Any` → `Top`/`Bottom`/`Unknown`; vars carry `TypeVarBounds`; add `Intersection(Vec<Type>)` | **Fundamental** — every `match` on `Type` gains new arms |
| `src/types.rs` `unify` | Replace with `constrain(t1 <: t2)` + polarity-aware decomposition | **Fundamental** — every `unify()` call site changes |
| `src/typecheck.rs` | Inequality constraints; bounds-carrying type schemes | **Major** — extensive but systematic migration |
| `src/types.rs` `is_subtype` | Subsumed by constraint accumulation; `is_subtype` becomes `constrain` call | **Major** |
| Row polymorphism | Row vars carry bounds; four-case → constraint decomposition; width subtyping built in | **Moderate-to-Major** |
| Bidirectional checking | Becomes optional (improves error locality but not required) | Simplification |
| `TypeAssert` | `constrain(inferred <: asserted)` | Minor |
| `src/error.rs` | Constraint provenance chain; bounds-conflict explanation | **Major** |

## References

- Dolan, S. (2016). *Algebraic Subtyping.* PhD thesis, University of Cambridge.
  — Full theoretical treatment: row types (Chapter 6), automata simplification,
  principal type proofs (Theorem 4.1).
- Dolan, S. & Mycroft, A. (2017). "Polymorphism, subtyping, and type inference
  in MLsub." In *POPL '17*, pp. 228–242. ACM.
  — Conference distillation. Proves principal types for the system. Foundation
  for inferred unions.
- Parreaux, L. (2020). "The simple essence of algebraic subtyping." In *ICFP '20*,
  Article 124. ACM.
  — Simplified algorithm closer to Algorithm W. Reference implementation ~500 lines.
  Direct implementation reference for full algebraic subtyping.
- Marques, R., Florido, M. & Vasconcelos, P. (2024). "Towards algebraic subtyping
  for extensible records." arXiv:2407.06747.
  — Extends Simple-sub with row variables. Directly applicable to tinct's row
  polymorphism + algebraic subtyping combination.
- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press.
  — Standard subtyping rules [UNION-INJ-L], [UNION-INJ-R], [UNION-ELIM]
  (Chapter 15). Existential types (Chapter 24).
- Dunfield, J. & Pfenning, F. (2004). "Tridirectional typechecking." In *POPL '04*,
  pp. 281–292. ACM.
  — Datasort refinements and union/intersection types in bidirectional checking.
  Relevant to checking mode for union annotations.
- Dunfield, J. & Krishnaswami, N. (2021). "Bidirectional typing." *ACM Computing
  Surveys*, 54(5), Article 98.
  — Survey covering bidirectional checking and subtyping interaction. Relevant to
  whether bidirectional checking should be retained under algebraic subtyping.
- Tobin-Hochstadt, S. & Felleisen, M. (2010). "Logical types for untyped languages."
  In *ICFP '10*, pp. 117–128. ACM.
  — Occurrence typing in Typed Racket: union types with path-sensitive narrowing.
  Practical reference for unions + narrowing interaction.
- Traytel, D., Berghofer, S. & Nipkow, T. (2011). "Extending Hindley-Milner type
  inference with coercive structural subtyping." In *APLAS '11*, LNCS 7078,
  pp. 89–104. Springer.
  — Alternative approach using coercions rather than biunification. Less expressive
  but simpler; useful as a design contrast.
