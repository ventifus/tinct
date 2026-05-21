# What If: Union Types and Algebraic Subtyping

**State:** Accepted — 2026-05-05

What would it take to add union types to tinct, and where does the full endpoint lead?

## Current State

tinct's type system is Hindley-Milner with Robinson unification, **[U-SUBSUME]**
as a ground-type compatibility escape hatch, bidirectional checking, literal
subtypes (`IntLiteral(42) <: Int <: Number`), `Any` as both top and bottom, and
Rémy-style row polymorphism for open/closed records.

The subtype lattice is small and closed:

```
Any (top and bottom — gradual)
├── Number
│   ├── Int
│   │   └── IntLiteral(n)
│   └── Float
├── Str
│   └── StringLiteral(s)
├── Bool
├── Null
├── Fn(params → ret)       (contravariant params, covariant ret)
├── Seq(τ)                 (covariant element type)
└── Record(fields, tail)   (covariant fields, open/closed tail)
```

### What's Missing

1. **No `Type::Union` variant.** The only escape hatch for "A or B" is `Any`.
2. **No union subtyping rules.** `is_subtype` has no injection or elimination rules.
3. **No syntax for union annotations.** The grammar cannot parse `Int | Str` in type position.
4. **`if` returns `Any`.** Branch result types are joined via `unify`, which falls back to `Any`.
5. **Dual-dispatch builtins untyped.** `map`, `filter`, etc. are typed `Any → Any`
   because `Dict | Seq` cannot be expressed.
6. **[U-SUBSUME] is ad hoc.** It handles ground types only; there is no uniform
   subtyping in the constraint solver for type constructors.
7. **No intersection types.** `HasName & HasAge` cannot be expressed without
   constructing an explicit record type.

## What It Would Provide

### Annotation-Only Unions (Phase 2 — conservative)

- **Precise typing for dual-dispatch builtins.** `map : (a → b) → (Dict a | Seq a) → (Dict b | Seq b)`
- **Nullable value types.** `Int | Null` instead of collapsing to `Any`
- **Tagged union / result types.** `[ok: a] | [err: String]` with static checking
- **More precise `if` return types.** Annotated unions on user-defined functions
- **Foundation for multi-entry `[type ...]` ADTs.** `Type::Union` is the prerequisite for
  `doc/whatif/algebraic-data-types.md` Phase 2

### Full Algebraic Subtyping (Phase 3 — endpoint)

Dolan & Mycroft (2017) and Parreaux (2020, Simple-sub) resolve the fundamental
tension: **Robinson unification computes equality constraints, but subtyping needs
inequality constraints.** Key insight: type variables in *input* (negative) positions
carry different constraints than those in *output* (positive) positions.

This gives:

1. **Inferred union types** — `[if true 1 "hello"]` infers `Int | Str`, not `Any`
2. **Inferred intersection types** — variables with multiple upper bounds compact to `A & B`
3. **Principal types with subtyping** — the most general type, proven (Dolan 2016, Theorem 4.1)
4. **No [U-SUBSUME]** — subtyping built into the solver uniformly across all type constructors
5. **Width subtyping for records** — `{x: Int, y: Str} <: {x: Int}` without requiring
   row variables at every call site
6. **Type simplification** — compact principal types via constraint compaction (Parreaux 2020)

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

```
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

```
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

`Any` remains the inferred join for incompatible types. Splitting `Any` into
`Unknown` (gradual) + `Top` (subtyping ceiling) is required for full union
semantics and is scheduled for Phase 3 — coordinated with
`doc/whatif/gradual-typing.md` Phase 2.

### Full Algebraic Subtyping: Simple-sub

Adopt Simple-sub (Parreaux 2020) as a constraint-solving replacement for
[U-SUBSUME] + Robinson unification.

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

Every use of `Any` must be audited and reclassified. This is the highest-risk
single change. See `doc/whatif/gradual-typing.md` Phase 2.

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

## What Would Change

### For Annotation-Only Unions (Phase 2)

| Component | Current | Change | Impact |
|-----------|---------|--------|--------|
| `src/types.rs` | No union variant | Add `Union(Vec<Type>)` + `normalize_union()` | Moderate — propagates through `is_subtype`, `apply_substitution`, `collect_type_vars`, `display` |
| `src/types.rs` `is_subtype` | No union rules | Add `[UNION-INJ-L]`, `[UNION-INJ-R]`, `[UNION-ELIM]` | Minor — three new match arms |
| `src/typecheck.rs` `resolve_annotation` | No union annotations | Collect positional entries from `Annotation::PropertyDict` into `type:` value as a list; resolve `type: [T1 T2]` as `Union(normalize(T1), normalize(T2))` | Minor |
| `src/eval.rs` | — | No changes; unions erased at runtime | None |
| `src/builtins.rs` | Dual-dispatch typed `Any → Any` | Update signatures to use `Union` types | Minor |
| `doc/05-type-annotations.md` | Only `type:` key + shorthand documented | Add generalized annotation model: positional entries = union members, named entries = metadata; update property table; add union examples | Minor |

### For Full Algebraic Subtyping (Phase 3)

| Component | Current | Change | Impact |
|-----------|---------|--------|--------|
| `src/types.rs` | `Any` as single top/bottom; vars bind to concrete types | `Any` → `Top`/`Bottom`/`Unknown`; vars carry `TypeVarBounds`; add `Intersection(Vec<Type>)` | **Fundamental** — every `match` on `Type` gains new arms |
| `src/types.rs` `unify` | Robinson + [U-SUBSUME] | Replace with `constrain(t1 <: t2)` + polarity-aware decomposition | **Fundamental** — every `unify()` call site changes |
| `src/typecheck.rs` | Algorithm W threading substitutions | Same AST-walking structure; inequality constraints; bounds-carrying type schemes | **Major** — extensive but systematic migration |
| `src/types.rs` `is_subtype` | Simple predicate ~15 arms | Subsumed by constraint accumulation; `is_subtype` becomes `constrain` call | **Major** |
| Row polymorphism | Rémy four-case unification | Row vars carry bounds; four-case → constraint decomposition; width subtyping built in | **Moderate-to-Major** |
| Bidirectional checking | Required for soundness | Becomes optional (improves error locality but not required) | Simplification |
| `TypeAssert` | `is_subtype(inferred, asserted)` | `constrain(inferred <: asserted)` | Minor |
| `src/error.rs` | Point-of-unification-failure errors | Constraint provenance chain; bounds-conflict explanation | **Major** |

## Phased Adoption

### Phase 1: Type Classes (no union types)

Solve the dual-dispatch typing problem without union types via constrained
polymorphism (see `doc/whatif/typeclasses.md`):

```
map : Functor f => (a → b) → f a → f b
```

Covers the primary motivation (precise `map`, `filter`, etc. typing) without
any union type machinery.

### Phase 2: Annotation-Only Unions

Add `Type::Union(Vec<Type>)` with subtyping rules `[UNION-INJ-L]`, `[UNION-INJ-R]`,
`[UNION-ELIM]`. Unions appear only in explicit type annotations and builtin
signatures — `unify` never produces them.

This enables: `Int | Null` for nullable values, `[ok: a] | [err: String]` for
result types, explicit union annotations on user-defined functions, and is the
prerequisite for multi-entry `[type ...]` ADT declarations (see `doc/whatif/algebraic-data-types.md`).

### Phase 3: Full Algebraic Subtyping (Simple-sub)

If annotation-only unions prove insufficient — specifically when `if` return
types and other inferred positions need unions — adopt Simple-sub for full
algebraic subtyping with inferred union and intersection types. Four sub-phases:

**3a. Constraint Infrastructure:** Add `TypeVarBounds { lower, upper }` alongside
existing substitution. New `constrain(t1 <: t2)` function with polarity-aware
structural decomposition. Existing `unify()` continues to work — adds
infrastructure without removing anything.

**3b. Migrate Call Sites:** Replace `unify()` with `constrain()` one subsystem at
a time: literal-to-base promotion, function application, record width subtyping,
let-generalization.

**3c. Union/Intersection from Bound Compaction:** With constraint solver in place,
`Type::Union` and `Type::Intersection` appear in inferred types when a variable
has multiple lower (union) or upper (intersection) bounds.

**3d. `Any` Split:** Split `Type::Any` into `Top`, `Bottom`, and `Unknown`.
Required for lattice soundness. **Note:** In the typing-cluster plan, the
`Any` split ships as B2 (`gradual-typing-split`) *before* algebraic
subtyping (D2). See `doc/whatif/gradual-typing.md` Phase 2.

### Prerequisites

- Phase 2: No hard dependencies. Annotation-only unions do not conflict
  with `Any`-as-top-and-bottom since `unify` never produces them. `Any`
  split (B2) follows after B1.
- Phase 3: `row-polymorphism` stable; `gradual-typing` Phase 2 complete;
  Phase 2 complete (Phase 3 requires `Type::Union` already existing)

### Trigger

**Phase 2:** Nullable types (`Int | Null`), tagged unions, and `try` result
types (`[ok: a] | [err: String]`) all require annotation-only unions. Adopt
after Phase 1.

**Phase 3:** Inferred unions eliminate annotation burden and fix [U-SUBSUME]
false positives. Adopt after annotation-only unions are established and their
limitations are quantified.

## References

- Dolan, S. (2016). *Algebraic Subtyping.* PhD thesis, University of Cambridge.
  — Full theoretical treatment: row types (Chapter 6), automata simplification,
  principal type proofs (Theorem 4.1).
- Dolan, S. & Mycroft, A. (2017). "Polymorphism, subtyping, and type inference
  in MLsub." In *POPL '17*, pp. 228–242. ACM.
  — Conference distillation. Proves principal types for the system. Foundation
  for Phase 3 inferred unions.
- Parreaux, L. (2020). "The simple essence of algebraic subtyping." In *ICFP '20*,
  Article 124. ACM.
  — Simplified algorithm closer to Algorithm W. Reference implementation ~500 lines.
  Direct implementation reference for Phase 3.
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
