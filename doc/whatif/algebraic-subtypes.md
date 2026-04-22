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
- **Row polymorphism** (Rémy 1994) with open/closed records and row variables

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

## What Algebraic Subtyping Provides

Dolan & Mycroft (2017) and Parreaux (2020, Simple-sub) solve the fundamental
tension: **Robinson unification computes equality constraints, but subtyping
needs inequality constraints.** Their key insight is that type variables
appearing in *input* (negative) positions carry different constraints than those
in *output* (positive) positions.

Algebraic subtyping gives you:

1. **Union types** (`Int | Str`) — a value that is either type
2. **Intersection types** (`HasName & HasAge`) — a value that is both types
3. **Principal types with subtyping** — the inferred type is the most general
   type, accounting for all subtyping relationships
4. **No [U-SUBSUME] hack** — subtyping is built into the constraint solver
5. **Type simplification** — compact principal types via automata-theoretic
   simplification (Dolan) or constraint compaction (Parreaux)

## What Would Change

### 1. Type Representation

**Current:**
```rust
enum Type {
    Int, Float, Number, Str, Bool, Null, Any,
    IntLiteral(i64), StringLiteral(String),
    TypeVar(String),
    Function { params, ret },
    Record(fields, RowRest),
    Seq(Box<Type>),
}
```

**Algebraic subtyping:**
```rust
enum Type {
    // Primitives unchanged
    Int, Float, Number, Str, Bool, Null,

    // Literals unchanged
    IntLiteral(i64), StringLiteral(String),

    // Type variables carry upper and lower bounds instead of equality bindings
    TypeVar(String),

    // NEW: set-theoretic type constructors
    Union(Vec<Type>),         // τ₁ | τ₂ — positive positions only
    Intersection(Vec<Type>),  // τ₁ & τ₂ — negative positions only
    Top,                      // replaces Any-as-top
    Bottom,                   // replaces Any-as-bottom (Nothing/Never)

    // Existing constructors, now with polarity awareness
    Function { params, ret },
    Record(Row),
    Seq(Box<Type>),
}
```

**Impact: Major.** Every match on `Type` in the codebase gains new arms. `Any`
splits into `Top` and `Bottom` with different semantics. The `TypeVar` binding
model changes fundamentally (see §3).

### 2. Subtype Lattice

**Current:** `is_subtype(τ, σ)` is a simple predicate with ~15 match arms
covering the literal→base→supertype chains.

**Algebraic subtyping:** Subtyping becomes a constraint-solving problem. Instead
of a boolean predicate, you accumulate constraints:

```
τ <: σ   becomes   add_constraint(τ, σ)
```

Constraints are solved lazily during type simplification. The subtyping relation
is defined algebraically via a lattice with union (join) and intersection
(meet), and the solver computes whether a type inhabits both sides of a
constraint.

**Impact: Major.** `is_subtype` is replaced by constraint accumulation. Every
call site that checks `is_subtype(a, b)` becomes `constrain(a <: b)`.

### 3. Unification → Constraint Solving

**Current:** Robinson unification with [U-SUBSUME] fallback. Type variables bind
to concrete types via substitution (`S[α → τ]`). Substitution is eagerly
applied.

**Algebraic subtyping:** Type variables carry *bounds* instead of *bindings*:

```rust
struct TypeVarBounds {
    lower: Vec<Type>,  // types that are subtypes of this var (positive uses)
    upper: Vec<Type>,  // types that are supertypes of this var (negative uses)
}
```

When `α` appears in positive position (output), it gets a lower bound.
When `α` appears in negative position (input), it gets an upper bound.
The variable is satisfiable iff `⊔ lower <: ⊓ upper` (join of lowers is a
subtype of meet of uppers).

The Simple-sub algorithm (Parreaux 2020) makes this tractable:

1. Walk the AST, generating constraints (like Algorithm W/J)
2. Instead of `unify(τ₁, τ₂)`, call `constrain(τ₁ <: τ₂)`
3. `constrain` decomposes structural types covariantly/contravariantly:
   - `constrain(Fn(A→B) <: Fn(C→D))` becomes `constrain(C <: A)` + `constrain(B <: D)`
   - `constrain(Record(R₁) <: Record(R₂))` constrains shared fields covariantly
4. When a type variable meets a bound, record it (don't eagerly substitute)
5. After inference, *compact* the bounds into readable types

**Impact: Fundamental.** This replaces `unify()` entirely. The substitution model
changes from `Map<Var, Type>` to `Map<Var, Bounds>`. Every call to `unify()`
becomes `constrain()`. Substitution application (`apply`) is replaced by bound
compaction.

### 4. Let-Generalization

**Current:** Kiselyov (2013) levels-based generalization. Variables above the
current level are generalized. `TypeScheme` wraps a type with quantified
variables.

**Algebraic subtyping:** Generalization still works, but generalized variables
carry their bounds into the scheme. Instantiation creates fresh variables with
*copies* of those bounds (not just fresh unconstrained variables). This is more
complex than current instantiation.

**Impact: Moderate.** The `TypeScheme` representation changes. `instantiate()`
becomes more complex — it must copy and freshen bounds, not just rename
variables.

### 5. Row Polymorphism

**Current (planned):** Rémy (1994) kinded row unification with dict+tail
representation, four-case remainder unification (Wand 1987).

**Algebraic subtyping:** Row variables carry bounds like type variables. Record
subtyping is *width subtyping* — `{x: Int, y: Str}` is a subtype of `{x: Int}`
because the wider record has more fields. This is built into the lattice, not
a separate check.

```
constrain(Record({x: Int, y: Str}), Record({x: Int}))
→ constrain(Int <: Int)                          # shared field x
  # y: Str has no counterpart — OK, width subtyping
```

Row variables become bounds-carrying variables that track which fields *must*
be present (lower bound) and which fields *may* be present (upper bound).

**Impact: Moderate-to-Major.** The planned Rémy model works well with algebraic
subtyping — Dolan's thesis (2016) covers this explicitly. But the four-case
unification becomes constraint decomposition, and row variable binding becomes
row variable bounding.

Recent work by Marques, Florido & Vasconcelos (2024, "Towards Algebraic Subtyping
for Extensible Records") extends Simple-sub with row variables specifically,
showing the approach is viable.

### 6. `Any` Type

**Current:** `Any` is both top and bottom — `τ <: Any` and `Any <: τ` for all
τ. This is the gradual typing escape hatch.

**Algebraic subtyping:** `Any` as both top and bottom is unsound in algebraic
subtyping (it collapses all types to be equivalent). The standard approach
splits:

- `Top` — supertype of everything (like TypeScript's `unknown`)
- `Bottom` — subtype of everything (like TypeScript's `never`)
- `Any` — *consistent* with everything (gradual typing, not part of the lattice)

Gradual typing + algebraic subtyping is an active research area. Siek & Taha's
gradual typing (2006) can be integrated, but `Any` becomes a consistency
relation (`~`), not a subtype/supertype.

**Impact: Major.** Every use of `Any` must be audited. Currently `Any` serves
multiple roles — unknown type, untyped values, gradual escape hatch. These
would need separate representations. This is the single highest-risk change.

### 7. Bidirectional Checking

**Current (planned):** Pierce & Turner (2000) bidirectional checking with
synthesis (⇒) and checking (⇐) modes.

**Algebraic subtyping:** Bidirectional checking is *not needed* — the constraint
solver handles subtyping uniformly. You can still use bidirectional checking
for better error messages (Dunfield & Krishnaswami 2021), but it's optional.

**Impact: Simplification.** The planned bidirectional checking becomes optional.
`check_expr` could be removed entirely, with all checking done via constraints.
However, keeping it improves error locality.

### 8. Error Messages

**Current:** Type errors are reported at the point of unification failure.
Messages reference concrete types.

**Algebraic subtyping:** Errors are reported when bounds are unsatisfiable
(`⊔ lower ≱ ⊓ upper`). Messages must explain *why* bounds conflict, which
requires tracking constraint provenance — where each bound came from.

**Impact: Major.** Error quality is the #1 complaint about algebraic subtyping
implementations. MLsub's errors are famously bad. Simple-sub improves on this
via simpler constraint representation, but constraint provenance tracking is
still harder than point-of-failure error reporting.

### 9. TypeAssert

**Current:** `expr :: Type` checks `is_subtype(inferred, asserted)` at the
type checker, and optionally at runtime.

**Algebraic subtyping:** TypeAssert becomes `constrain(inferred <: asserted)`.
Static checking works naturally. Runtime checking is unchanged (structural
comparison against materialized value).

**Impact: Minor.** Direct mapping.

## What We'd Gain

1. **Union types for dual-dispatch builtins.** Currently `$map`, `$filter` etc.
   are typed as `Any` because they accept both Dict and Seq. With unions:
   `$map : (τ₁ → τ₂, Dict[τ₁] | Seq[τ₁]) → Dict[τ₂] | Seq[τ₂]`

2. **Width subtyping for records.** Passing `{x: Int, y: Str}` where
   `{x: Int}` is expected works without row variables. Row variables are still
   useful for *preserving* extra fields through a function, but basic width
   subtyping is free.

3. **No [U-SUBSUME] needed.** Literal-to-base subtyping is handled by the
   constraint solver uniformly.

4. **Intersection types for record extension patterns.** `HasName & HasAge`
   expresses "has both name and age fields" without explicit record types.

5. **Principal types with subtyping.** The inferred type is guaranteed to be
   the most general — no precision loss, no arbitrary choices.

## What We'd Lose / Risk

1. **Complexity.** Simple-sub is ~500 lines for a minimal implementation. tinct's
   type checker with row polymorphism, literal types, let-generalization,
   TypeAssert, and gradual typing would be significantly more complex.

2. **Error message quality.** Constraint provenance is hard. tinct currently
   has clear "expected X, got Y" errors at specific source locations. Algebraic
   subtyping errors are "bounds unsatisfiable" with chains of reasons.

3. **`Any` semantics.** Splitting `Any` into Top/Bottom/Gradual affects the
   entire evaluator (runtime type checks, builtin signatures, untyped values).
   This is a pervasive change with high risk of subtle breakage.

4. **Row polymorphism interaction.** Rémy-style row polymorphism + algebraic
   subtyping is viable (Marques et al. 2024) but less mature than either
   approach alone. The implementation burden is higher.

5. **Implementation timeline.** Algebraic subtyping is a rewrite of the type
   inference engine, not an incremental change. It would delay other planned
   work (CEK machine, sequences, sandboxing).

## Recommendation

**Adopt Simple-sub (Parreaux 2020) as a constraint-solving replacement for
[U-SUBSUME] + Robinson unification.**

### Why Simple-sub over Dolan's Biunification

Parreaux (2020) distills Dolan & Mycroft (2017) into an algorithm closer to
Algorithm W — it uses the same AST-walking structure, the same
let-generalization strategy, and produces better error messages. The key
differences from tinct's current approach:

1. `unify(τ₁, τ₂)` becomes `constrain(τ₁ <: τ₂)` — inequality constraints
   instead of equality
2. Type variables carry upper/lower bounds instead of equality bindings
3. Union and intersection types emerge naturally from bound compaction
4. [U-SUBSUME] is unnecessary — subtyping is built into the solver

Simple-sub is ~500 lines for a minimal implementation. tinct's full system
(row polymorphism, literal types, let-generalization, TypeAssert, gradual
typing) would be larger, but the Lorenz et al. (2024) extension for row
variables provides a direct template.

### Migration Path

#### Step 1: Constraint Infrastructure

Add the constraint representation alongside existing unification. Type
variables gain `TypeVarBounds { lower: Vec<Type>, upper: Vec<Type> }`
in addition to the current substitution map. New `constrain(τ₁ <: τ₂)`
function that decomposes structural types with polarity awareness
(covariant fields, contravariant params).

#### Step 2: Migrate Unification Call Sites

Replace `unify()` calls with `constrain()` calls, one subsystem at a time:
1. Literal-to-base promotion (currently [U-SUBSUME]) → `constrain(IntLiteral <: Int)`
2. Function application → `constrain(arg <: param)`, `constrain(ret <: expected)`
3. Record width subtyping → `constrain(wider <: narrower)` with field decomposition
4. Let-generalization → bounds-carrying type schemes

#### Step 3: Union/Intersection Types

With the constraint solver in place, add `Type::Union` and
`Type::Intersection` as bound compaction results. These appear in inferred
types when a variable has multiple lower bounds (union) or multiple upper
bounds (intersection).

#### Step 4: `Any` Split

Split `Type::Any` into `Top`, `Bottom`, and `Unknown` (gradual). This is
required for lattice soundness — `Any`-as-top-and-bottom collapses the
algebraic subtyping lattice. Coordinate with `doc/whatif/gradual-typing.md`
Phase 2.

### Error Message Strategy

Constraint provenance is the main risk. Each constraint records its source
span and the reason it was generated (function call, field access, type
annotation). When bounds are unsatisfiable, the error traces back through
the provenance chain to show *why* the conflict arose, not just *that* it
exists. Parreaux's simpler constraint representation helps — bounds are
concrete types, not automata states.

### Prerequisites

- `let-generalization` complete (bounds must propagate through type schemes)
- `bidirectional-typing` complete (checking mode provides better constraint
  generation, though algebraic subtyping makes it optional)
- `gradual-typing` Phase 2 (`Any` split into Unknown + Top)
- `row-polymorphism` implementation stable (Lorenz et al. 2024 extends
  Simple-sub with row variables, but the base row system must be solid)

### Trigger

Adopt when:
- Union types become necessary for precise dual-dispatch typing (and type
  classes alone are insufficient — see `doc/whatif/union-types.md` Phase 3)
- `Any`-as-top-and-bottom causes a soundness problem in the type checker
- Rémy-style row unification interacts badly with [U-SUBSUME], creating
  false positives or missed errors at record boundaries

## References

- Dolan, S. (2016). *Algebraic Subtyping.* PhD thesis, University of Cambridge.
- Dolan, S. & Mycroft, A. (2017). Polymorphism, subtyping, and type inference in MLsub. In *POPL '17*, pp. 228–242. ACM.
- Parreaux, L. (2020). The simple essence of algebraic subtyping: principal type inference with subtyping made easy. In *ICFP '20*, Article 124. ACM.
- Marques, R., Florido, M. & Vasconcelos, P. (2024). Towards algebraic subtyping for extensible records. arXiv:2407.06747.
- Traytel, D., Berghofer, S. & Nipkow, T. (2011). Extending Hindley-Milner type inference with coercive structural subtyping. In *APLAS '11*, LNCS 7078, pp. 89–104. Springer.
