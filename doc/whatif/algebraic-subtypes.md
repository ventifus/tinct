# What If: Algebraic Subtyping for tinct

What would it take to replace tinct's current ad-hoc subtyping ([U-SUBSUME])
with full algebraic subtyping (Dolan & Mycroft 2017)?

## Current State

tinct's type system is Hindley-Milner with:

- **Robinson unification** for structural decomposition and type variable binding
- **[U-SUBSUME]** as a ground-type compatibility check: when both sides are
  concrete and related by the subtype lattice, unification succeeds without
  modifying the substitution
- **Bidirectional checking** (Pierce & Turner 2000): `check_expr` uses
  directional `is_subtype(actual, expected)` at fully concrete positions
- **Literal types** as subtypes of base types: `IntLiteral(42) <: Int <: Number`
- **`Any`** as both top and bottom (gradual typing)
- **Row polymorphism** (Remy 1994) with open/closed records and row variables

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

This works because the lattice is simple and the only subtyping that enters
unification is literal-to-base promotion. There are no union types, no
intersection types, no negative types.

### What's Missing

1. **Union types for dual-dispatch builtins.** `$map`, `$filter`, etc. are
   typed as `Any` because `Dict | Seq` cannot be expressed.
2. **Width subtyping for records.** Passing `{x: Int, y: Str}` where
   `{x: Int}` is expected requires row variables — basic width subtyping
   is not built into the constraint solver.
3. **Principal types with subtyping.** The [U-SUBSUME] check preserves the
   more specific binding (e.g., `IntLiteral(42)` rather than widening to
   `Int`), but this is a local heuristic, not a guarantee from the solver.
4. **Intersection types for record composition.** `HasName & HasAge` cannot
   be expressed without constructing an explicit record type.
5. **Uniform subtyping in inference.** [U-SUBSUME] is an ad-hoc escape from
   Robinson unification; it handles ground types but not type constructors
   with subtyping relationships.

## What Algebraic Subtyping Would Provide

Dolan & Mycroft (2017) and Parreaux (2020, Simple-sub) solve the fundamental
tension: **Robinson unification computes equality constraints, but subtyping
needs inequality constraints.** Their key insight is that type variables
appearing in *input* (negative) positions carry different constraints than those
in *output* (positive) positions.

Algebraic subtyping gives you:

1. **Union types** (`Int | Str`) — a value that is either type, emerging
   naturally from bound compaction when a variable has multiple lower bounds
2. **Intersection types** (`HasName & HasAge`) — a value that is both types,
   emerging when a variable has multiple upper bounds
3. **Principal types with subtyping** — the inferred type is the most general
   type, accounting for all subtyping relationships (proven in Dolan 2016,
   Chapter 4)
4. **No [U-SUBSUME] hack** — subtyping is built into the constraint solver
   uniformly across all type constructors
5. **Type simplification** — compact principal types via automata-theoretic
   simplification (Dolan 2016) or constraint compaction (Parreaux 2020)

## Design

Adopt Simple-sub (Parreaux 2020) as a constraint-solving replacement for
[U-SUBSUME] + Robinson unification. Parreaux distills Dolan & Mycroft (2017)
into an algorithm closer to Algorithm W — it uses the same AST-walking
structure, the same let-generalization strategy, and produces better error
messages than Dolan's biunification.

### Why Simple-sub over Dolan's Biunification

Dolan's original formulation (2017) uses biunification with automata-theoretic
type simplification. This is theoretically elegant but complex to implement
and produces hard-to-read types without simplification. Simple-sub (Parreaux
2020) achieves the same principal type guarantees with a simpler algorithm:

- **Same guarantees:** principal types for all programs typeable in ML, plus
  subtyping-aware inference
- **Simpler constraint representation:** bounds are concrete types, not
  automata states
- **Better error messages:** bounds conflict errors reference concrete types
  rather than automata transitions
- **Smaller implementation:** ~500 lines for the core algorithm (Parreaux's
  reference implementation)

### Core Mechanism

The key changes from tinct's current approach:

1. **`unify(t1, t2)` becomes `constrain(t1 <: t2)`** — inequality constraints
   instead of equality. Structural types decompose with polarity awareness:
   - `constrain(Fn(A -> B) <: Fn(C -> D))` becomes `constrain(C <: A)` + `constrain(B <: D)`
   - `constrain(Record(R1) <: Record(R2))` constrains shared fields covariantly
2. **Type variables carry upper/lower bounds instead of equality bindings:**
   ```rust
   struct TypeVarBounds {
       lower: Vec<Type>,  // types that are subtypes of this var (positive uses)
       upper: Vec<Type>,  // types that are supertypes of this var (negative uses)
   }
   ```
   When `a` appears in positive position (output), it gets a lower bound.
   When `a` appears in negative position (input), it gets an upper bound.
   The variable is satisfiable iff `join(lower) <: meet(upper)`.
3. **Union and intersection types emerge from bound compaction** — they are
   not user-written syntax but inferred result types
4. **[U-SUBSUME] is unnecessary** — subtyping is built into the solver

### Interaction with Row Polymorphism

Row variables become bounds-carrying variables that track which fields *must*
be present (lower bound) and which fields *may* be present (upper bound).
Record subtyping is *width subtyping* — `{x: Int, y: Str}` is a subtype of
`{x: Int}` because the wider record has more fields.

```
constrain(Record({x: Int, y: Str}), Record({x: Int}))
-> constrain(Int <: Int)                          # shared field x
  # y: Str has no counterpart — OK, width subtyping
```

Remy-style four-case unification becomes constraint decomposition, and row
variable binding becomes row variable bounding. Marques, Florido &
Vasconcelos (2024) extend Simple-sub with row variables specifically, showing
the approach is viable and providing a direct implementation template.

### Interaction with `Any`

`Any` as both top and bottom is unsound in algebraic subtyping — it collapses
all types to be equivalent. The standard approach splits:

- `Top` — supertype of everything (like TypeScript's `unknown`)
- `Bottom` — subtype of everything (like TypeScript's `never`)
- `Unknown` — *consistent* with everything (gradual typing, separate relation)

This split is the single highest-risk change. Every use of `Any` must be
audited and reclassified. See `doc/whatif/gradual-typing.md` Phase 2 for
the migration plan.

### Interaction with Let-Generalization

Generalization still works with levels (Kiselyov 2013), but generalized
variables carry their bounds into the type scheme. Instantiation creates
fresh variables with *copies* of those bounds — not just fresh unconstrained
variables. This is more complex than current instantiation but preserves
the principal type guarantee (Dolan 2016, Theorem 4.1).

### Interaction with Bidirectional Checking

Bidirectional checking (Pierce & Turner 2000) is *not needed* when algebraic
subtyping is in place — the constraint solver handles subtyping uniformly.
Bidirectional checking can still be used for better error locality
(Dunfield & Krishnaswami 2021), but it becomes optional rather than required
for soundness.

### Error Message Strategy

Constraint provenance is the main risk. Each constraint records its source
span and the reason it was generated (function call, field access, type
annotation). When bounds are unsatisfiable, the error traces back through
the provenance chain to show *why* the conflict arose, not just *that* it
exists. Parreaux's simpler constraint representation helps — bounds are
concrete types, not automata states.

Error quality is the primary complaint about algebraic subtyping
implementations. MLsub's errors are famously bad. Simple-sub improves on
this, but constraint provenance tracking is inherently harder than
point-of-failure error reporting. The provenance chain approach
(recording the source span of each `constrain()` call) is the standard
mitigation.

## What Would Change

### Type Representation (`src/types.rs`)

**Current:** `Type` enum with `Any` as a single variant covering top, bottom,
and gradual typing. Type variables bind to concrete types via substitution.

**Proposed:** Type variables carry upper/lower bounds instead of equality
bindings. `Any` splits into `Top`, `Bottom`, and `Unknown`. New variants
`Union(Vec<Type>)` and `Intersection(Vec<Type>)` for bound compaction results.
Every `match` on `Type` gains new arms.

**Impact: Fundamental.** The `Type` enum is the foundation of the type system.
Every consumer of `Type` in the codebase must handle new variants. The
substitution model changes from `Map<Var, Type>` to `Map<Var, Bounds>`.

### Unification / Constraint Solver (`src/types.rs`)

**Current:** Robinson unification with [U-SUBSUME] fallback. `unify()` produces
equality bindings in a substitution map. Eager application.

**Proposed:** `unify()` replaced by `constrain(t1 <: t2)` with polarity-aware
structural decomposition. Substitution application (`apply`) replaced by bound
compaction. The occurs check extends to bound cycles.

**Impact: Fundamental.** Every call to `unify()` becomes `constrain()`. The
entire substitution threading model changes.

### Type Inference (`src/typecheck.rs`)

**Current:** Algorithm W-style bottom-up inference threading substitutions.
Let-generalization via Kiselyov levels.

**Proposed:** Same AST-walking structure, but generating inequality constraints
instead of equality bindings. Let-generalization produces schemes with
bounds-carrying variables. Instantiation copies and freshens bounds.

**Impact: Major.** The inference engine restructure is extensive but follows
the same AST-walking pattern. Simple-sub's algorithm has the same structure
as Algorithm W, making the migration systematic.

### Subtype Lattice (`src/types.rs`)

**Current:** `is_subtype(a, b)` is a simple predicate with ~15 match arms.

**Proposed:** Subtyping becomes constraint accumulation. `is_subtype` is
replaced by `constrain(a <: b)`. The subtyping relation is defined
algebraically via a lattice with union (join) and intersection (meet).

**Impact: Major.** All call sites that check `is_subtype(a, b)` become
`constrain(a <: b)`.

### Row Polymorphism (`src/types.rs`)

**Current (planned):** Remy (1994) kinded row unification with dict+tail
representation, four-case remainder unification (Wand 1987).

**Proposed:** Row variables carry bounds like type variables. Four-case
unification becomes constraint decomposition. Row variable binding becomes
row variable bounding. Width subtyping is built into the lattice.

**Impact: Moderate-to-Major.** Dolan's thesis (2016, Chapter 6) covers row
types explicitly. Marques et al. (2024) provide a direct template for the
Simple-sub extension.

### Bidirectional Checking (`src/typecheck.rs`)

**Current (planned):** Pierce & Turner (2000) bidirectional checking with
synthesis and checking modes.

**Proposed:** Bidirectional checking becomes optional. `check_expr` could be
removed, with all checking done via constraints. Retaining it improves error
locality but is not required for soundness.

**Impact: Simplification.** The planned bidirectional checking becomes
optional rather than required.

### TypeAssert (`src/typecheck.rs`, `src/eval.rs`)

**Current:** `expr :: Type` checks `is_subtype(inferred, asserted)` at the
type checker, and optionally at runtime.

**Proposed:** TypeAssert becomes `constrain(inferred <: asserted)`. Static
checking works naturally. Runtime checking is unchanged.

**Impact: Minor.** Direct mapping.

### Error Messages (`src/error.rs`)

**Current:** Type errors reported at point of unification failure with
concrete types.

**Proposed:** Errors reported when bounds are unsatisfiable. Messages must
explain *why* bounds conflict, requiring constraint provenance tracking.

**Impact: Major.** Error quality is the primary risk. Constraint provenance
tracking is harder than point-of-failure reporting.

## Phased Adoption

### Phase 1: Constraint Infrastructure

Add the constraint representation alongside existing unification. Type
variables gain `TypeVarBounds { lower: Vec<Type>, upper: Vec<Type> }`
in addition to the current substitution map. New `constrain(t1 <: t2)`
function that decomposes structural types with polarity awareness
(covariant fields, contravariant params). The existing `unify()` continues
to work — this phase adds infrastructure without removing anything.

### Phase 2: Migrate Unification Call Sites

Replace `unify()` calls with `constrain()` calls, one subsystem at a time:

1. Literal-to-base promotion (currently [U-SUBSUME]) — `constrain(IntLiteral <: Int)`
2. Function application — `constrain(arg <: param)`, `constrain(ret <: expected)`
3. Record width subtyping — `constrain(wider <: narrower)` with field decomposition
4. Let-generalization — bounds-carrying type schemes

### Phase 3: Union/Intersection Types

With the constraint solver in place, add `Type::Union` and
`Type::Intersection` as bound compaction results. These appear in inferred
types when a variable has multiple lower bounds (union) or multiple upper
bounds (intersection).

### Phase 4: `Any` Split

Split `Type::Any` into `Top`, `Bottom`, and `Unknown` (gradual). This is
required for lattice soundness — `Any`-as-top-and-bottom collapses the
algebraic subtyping lattice. Coordinate with `doc/whatif/gradual-typing.md`
Phase 2.

### Prerequisites

- `let-generalization` complete (bounds must propagate through type schemes)
- `bidirectional-typing` complete (checking mode provides better constraint
  generation, though algebraic subtyping makes it optional)
- `gradual-typing` Phase 2 (`Any` split into Unknown + Top) — see
  `doc/whatif/gradual-typing.md`
- `row-polymorphism` implementation stable (Marques et al. 2024 extends
  Simple-sub with row variables, but the base row system must be solid)

### Trigger

- Union types become necessary for precise dual-dispatch typing (and type
  classes alone are insufficient — see `doc/whatif/typeclasses.md`)
- `Any`-as-top-and-bottom causes a soundness problem in the type checker
- Remy-style row unification interacts badly with [U-SUBSUME], creating
  false positives or missed errors at record boundaries

## References

- Dolan, S. (2016). *Algebraic Subtyping.* PhD thesis, University of Cambridge. — Full theoretical treatment including row types (Chapter 6), simplification via automata theory, and principal type proofs.
- Dolan, S. & Mycroft, A. (2017). "Polymorphism, subtyping, and type inference in MLsub." In *POPL '17*, pp. 228-242. ACM. — Conference paper distilling the thesis. Proves principal types for the system.
- Parreaux, L. (2020). "The simple essence of algebraic subtyping: principal type inference with subtyping made easy." In *ICFP '20*, Article 124. ACM. — Simplified algorithm closer to Algorithm W. Reference implementation ~500 lines. Recommended as tinct's starting point.
- Marques, R., Florido, M. & Vasconcelos, P. (2024). "Towards algebraic subtyping for extensible records." arXiv:2407.06747. — Extends Simple-sub with row variables. Directly applicable to tinct's row polymorphism + algebraic subtyping combination.
- Traytel, D., Berghofer, S. & Nipkow, T. (2011). "Extending Hindley-Milner type inference with coercive structural subtyping." In *APLAS '11*, LNCS 7078, pp. 89-104. Springer. — Alternative approach using coercions rather than biunification. Less expressive but simpler.
- Dunfield, J. & Krishnaswami, N. (2021). "Bidirectional typing." *ACM Computing Surveys*, 54(5), Article 98. — Survey covering the interaction between bidirectional checking and subtyping. Relevant to the question of whether to retain bidirectional checking under algebraic subtyping.
