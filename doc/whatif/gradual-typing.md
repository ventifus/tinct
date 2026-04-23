# What If: Formal Gradual Typing for tinct

What would it take to formalize tinct's relationship with gradual typing
(Siek & Taha 2006, Garcia et al. 2016)?

## Current State

tinct uses `Any` as both top and bottom of the type lattice:

```
τ <: Any    [S-ANY-TOP]
Any <: τ    [S-ANY-BOT]
```

This violates antisymmetry (τ <: Any ∧ Any <: τ does not imply τ = Any) and
makes the subtype relation unsound as a partial order. It is documented as
intentional for tinct's gradual type system (DESIGN.md §Subtyping, Limitation
#5).

`Any` currently serves multiple roles:
1. **Unknown type** — unannotated params, forward references before
   let-generalization
2. **Untyped values** — return type of builtins that can't be precisely typed
3. **Gradual escape hatch** — TypeAssert default, `--no-typecheck` mode
4. **Top type for polymorphism** — dual-dispatch builtins accept "anything"

### What's Missing

1. **Principled `Any` semantics.** `Any` simultaneously acts as top, bottom,
   and gradual unknown — three distinct roles conflated into one variant.
   This means `Any <: Int` holds (bottom behavior), which is unsound: code
   typed as `Any` can flow into an `Int` position without any runtime check.
2. **Consistency relation.** Gradual typing uses a consistency relation (`~`)
   that is reflexive and symmetric but *not* transitive. tinct uses subtyping
   for all `Any` interactions, which is transitive, allowing type-unsafe
   chains: `Int <: Any <: String`.
3. **Blame tracking.** When a gradually typed program fails at runtime, there
   is no mechanism to identify which boundary between typed and untyped code
   is responsible. TypeAssert's `guard_span` provides partial blame for
   explicit annotations, but implicit `Any` boundaries have none.
4. **Gradual guarantee.** The system does not formally satisfy the gradual
   guarantee (Siek et al. 2015): adding type annotations can silently change
   which operations succeed rather than uniformly catching more errors.

## What Formal Gradual Typing Would Provide

### The Consistency Relation

In proper gradual typing, the unknown type `?` relates to other types via a
**consistency relation** (`~`), not subtyping:

```
τ ~ ?        (? is consistent with everything)
? ~ τ        (symmetric)
Int ~ Int     (reflexive on concrete types)
Int ≁ String  (concrete types must match)
```

Key property: **consistency is not transitive.** `Int ~ ?` and `? ~ String`,
but `Int ≁ String`. This prevents `?` from collapsing all types into
equivalence (which tinct's `Any`-as-top-and-bottom currently does).

### The Gradual Guarantee (Siek et al. 2015)

A gradually typed system satisfies the **gradual guarantee** if:
1. Removing type annotations (replacing with `?`) never causes a program to
   be statically rejected
2. Adding type annotations never causes a program that was statically
   accepted to be rejected — but it may cause runtime failures at typed/untyped
   boundaries

tinct partially satisfies this: removing `@Type` annotations allows programs
through (builtins default to `Any`), but the semantics of `Any` as both top
and bottom mean some annotation additions silently succeed where they should
fail.

### AGT (Garcia et al. 2016)

Abstracting Gradual Typing provides a systematic method to derive a gradual
type system from a static one:
1. `?` represents the *set of all types*
2. Static typing judgments lift to operate on sets of types
3. A gradual judgment holds if *some* consistent concretization satisfies the
   static judgment

This would provide a principled way to determine which operations on
`Any`-typed values should succeed vs. fail. Applied to tinct: starting from
the HM + row polymorphism static system, AGT would systematically derive
the gradual version, determining exactly where runtime checks are needed.

### Blame Tracking (Wadler & Findler 2009)

When a gradually typed program fails at runtime, **blame** identifies which
boundary between typed and untyped code is responsible. The blame theorem:
a well-typed component is never blamed.

tinct's TypeAssert proxy contracts (Findler & Felleisen 2002) already provide
the structural equivalent of blame boundaries (the `guard_span` field tracks
where the contract was introduced), but non-TypeAssert `Any` boundaries
(builtin returns, unannotated params) have no blame tracking.

## Design

Adopt AGT (Garcia et al. 2016) as the systematic framework for deriving
tinct's gradual type system from its static HM + row polymorphism base.
The design proceeds in three phases, each independently useful.

### Split `Any` into Three Concepts

The core change: replace `Type::Any` with three distinct types, each
serving one of the roles `Any` currently conflates:

```rust
enum Type {
    // ... existing types ...
    Unknown,      // ? — gradual typing (consistency, not subtyping)
    Top,          // ⊤ — supertype of everything (for type bounds)
    // Bottom not needed unless algebraic subtyping adopted
}
```

Role reclassification:
- Unannotated params: `Unknown` (was `Any`) — "I don't know the type yet"
- Builtin returns that can't be typed: `Unknown` (was `Any`) — "could be
  anything"
- TypeAssert upper bound `[@Any $expr]`: `Top` — "accept any type" (this is
  a true supertype, not gradual unknown)
- Forward references before let-generalization: fresh type variables (already
  planned, not `Any`)

### Consistency Relation

New `is_consistent(a, b) -> bool` function alongside the existing
`is_subtype(a, b)`:

```rust
fn is_consistent(a: &Type, b: &Type) -> bool {
    match (a, b) {
        (Type::Unknown, _) | (_, Type::Unknown) => true,
        (a, b) if a == b => true,
        // structural decomposition for functions, records, seqs
        (Type::Function { params: p1, ret: r1 },
         Type::Function { params: p2, ret: r2 }) =>
            p1.len() == p2.len()
            && p1.iter().zip(p2).all(|(a, b)| is_consistent(a, b))
            && is_consistent(r1, r2),
        (Type::Record(f1, r1), Type::Record(f2, r2)) =>
            // shared fields must be consistent; extra fields OK
            shared_fields_consistent(f1, f2)
            && row_rests_consistent(r1, r2),
        (Type::Seq(e1), Type::Seq(e2)) => is_consistent(e1, e2),
        _ => false,
    }
}
```

`is_consistent` replaces `is_subtype` at every point where `Any` currently
triggers silent success: CALL-ANY, DOT-OPEN, BRACKET-DYN, ACCESS-ANY. The
critical distinction: consistency is *not transitive*, so `Int ~ Unknown`
and `Unknown ~ String` do not imply `Int ~ String`.

### Runtime Checks at Boundaries

Every point where `Unknown` meets a concrete type is a blame boundary that
needs a runtime check:

```
Γ ⊢ f : Fn(Int → Int),  Γ ⊢ x : Unknown
──────────────────────────────────────────
Γ ⊢ [call f x] ⇒ Int
  with runtime check: materialize(x) must be Int
```

tinct's TypeAssert proxy contracts (Findler & Felleisen 2002) already provide
the mechanism. The question is whether to insert guards:

- **Explicit-only** (pragmatic starting point): runtime checks only at
  TypeAssert sites. `Unknown -> Concrete` mismatches caught at runtime
  materialization, not at the function boundary. This is tinct's current
  behavior modulo the `Any` rename.
- **Automatic insertion** (full gradual typing): the compiler inserts
  runtime guards at every `Unknown -> Concrete` boundary. This changes
  the evaluation model significantly but provides the full blame theorem
  guarantee.

### Blame Provenance

Each blame boundary records:
- The source span where the boundary exists
- The expected concrete type
- The origin of the `Unknown` value (which unannotated param, which builtin
  return)

When a runtime check fails, the error message identifies both the failure
point and the `Unknown` origin:

```
type mismatch at line 5: argument to $add expected Int,
  but value from line 3 (untyped) was String.
  The untyped boundary at line 3 is responsible (blame).
```

This extends tinct's existing `guard_span` mechanism in TypeAssert to cover
all `Unknown -> Concrete` boundaries.

### Interaction with Lazy Evaluation

Blame boundaries interact with lazy evaluation: a blame check on a thunk
must be deferred until the thunk is forced. This means blame annotations
attach to thunks, not values. When a thunk is forced and its value doesn't
match the expected type, blame falls on the boundary where the thunk was
created, not where it was forced.

This is analogous to Findler & Felleisen's (2002) treatment of higher-order
contracts: the contract wraps the thunk, and blame is assigned when the
wrapper fires. tinct's existing proxy contract mechanism handles this
pattern.

### Interaction with Row Polymorphism

`Unknown` in record fields means "this field has unknown type." Row
consistency must handle:

```
is_consistent(Record({x: Int, y: Unknown}), Record({x: Int, y: String}))
→ true  (because Unknown ~ String)
```

Open records with `Unknown` tails are consistent with any record that has
the known fields:

```
is_consistent(Record({x: Int, ...Unknown}), Record({x: Int, y: String}))
→ true
```

This follows directly from AGT's lifting: the static row unification rules
lift to operate on sets of row types, and `Unknown` represents the set of
all possible row tails.

### Interaction with Type Classes

If type classes are adopted (see `doc/whatif/typeclasses.md`), the question
arises: is `Eq Unknown` satisfied? Two options:

- **Yes (permissive):** `Unknown` satisfies all constraints. This preserves
  the gradual guarantee (removing annotations doesn't add static errors)
  but defeats the purpose of constraints — `Eq Unknown` means "trust me."
- **No (strict):** `Unknown` does not satisfy constraints. This catches more
  errors statically but means removing a type annotation can cause a
  constraint violation, breaking the gradual guarantee.

The AGT approach resolves this: `Unknown` satisfies a constraint if *some*
concretization of `Unknown` satisfies it. Since `Unknown` represents all
types and some types have `Eq`, `Eq Unknown` is satisfied — but a runtime
check is inserted at the point where `$=` is called on the `Unknown` value.

## What Would Change

### Type Representation (`src/types.rs`)

**Current:** `Type::Any` is a single variant serving as top, bottom, and
gradual unknown.

**Proposed:** `Type::Any` splits into `Type::Unknown` (gradual) and
`Type::Top` (true supertype). Every `match` arm for `Type::Any` must be
audited and reclassified. Unification rules change: `unify(Unknown, τ)`
uses consistency, not subtyping.

**Impact:** Major. Every use of `Type::Any` in the codebase must be
individually reclassified. The `Type` enum gains one variant (`Unknown`
replaces `Any`, `Top` is new) and loses one (`Any`).

### Subtyping (`src/types.rs`)

**Current:** `is_subtype` has `[S-ANY-TOP]` and `[S-ANY-BOT]` rules making
`Any` both top and bottom.

**Proposed:** `is_subtype` loses both `Any` rules. `τ <: Top` replaces
`τ <: Any`. No type is a subtype of `Unknown` and `Unknown` is not a subtype
of any concrete type — that relationship is handled by `is_consistent`
instead. New `is_consistent()` function (~30 lines) handles `Unknown`
interactions.

**Impact:** Major. `is_subtype` becomes a proper partial order (reflexive,
transitive, antisymmetric). ~20 call sites change from `is_subtype(_, Any)`
to `is_consistent`. This is the foundational change that makes the type
system sound.

### Type Inference (`src/typecheck.rs`)

**Current:** `Any` propagates silently through inference — `unify(Any, τ)`
succeeds without constraint.

**Proposed:** `unify(Unknown, τ)` records a consistency judgment rather than
binding. The inference engine distinguishes between "I don't know this type"
(`Unknown`, deferred to runtime) and "this type is unconstrained" (fresh
type variable, resolved by inference). This distinction prevents `Unknown`
from interfering with principal type inference for well-typed subexpressions.

**Impact:** Moderate. The core inference algorithm is unchanged — the
difference is in how `Unknown` values interact with unification. Well-typed
code that doesn't use `Any` is completely unaffected.

### Evaluator (`src/eval.rs`)

**Current:** Runtime type checks occur at TypeAssert sites and builtin
argument validation. No blame tracking for implicit `Any` boundaries.

**Proposed (explicit-only):** Unchanged from current behavior. `Unknown`
values flow through the evaluator like `Any` values do today. Runtime
failures at materialization produce errors without blame provenance.

**Proposed (automatic insertion):** The type checker inserts runtime guard
thunks at every `Unknown -> Concrete` boundary. Each guard carries a blame
label (source span + expected type). The evaluator checks the guard when the
thunk is forced. This extends the TypeAssert proxy contract mechanism.

**Impact:** Minor (explicit-only), Major (automatic insertion).

### TypeAssert (`src/typecheck.rs`, `src/eval.rs`)

**Current:** `expr :: Type` checks `is_subtype(inferred, asserted)`. The
`guard_span` field provides partial blame tracking.

**Proposed:** TypeAssert with `Any` annotation becomes `expr :: Top` (accept
any type). TypeAssert with concrete annotations generates consistency checks
when the inferred type is `Unknown`. The existing `guard_span` mechanism
extends naturally to serve as the blame label.

**Impact:** Minor. TypeAssert is already the closest thing to a blame
boundary in tinct. The change is mostly renaming.

### Error Messages (`src/error.rs`)

**Current:** "expected Int, got String" at point of materialization.

**Proposed:** Blame-aware errors: "type mismatch at line 5: argument to $add
expected Int, but value from line 3 (untyped) was String. The untyped
boundary at line 3 is responsible." Requires blame provenance propagation
through the evaluation.

**Impact:** Major (for automatic blame tracking), Minor (for explicit-only).
Blame provenance is the main implementation cost of full gradual typing.

## Phased Adoption

### Phase 1: Formalize (Documentation Only)

Document the consistency relation that `Any` actually implements today.
Define what the Gradual Guarantee means for tinct. Identify blame
boundaries — every point where `Unknown` would meet concrete types. This
establishes the rules before any code changes and validates the design
against Garcia et al.'s systematic derivation.

Deliverables:
- Formal consistency relation for tinct's types (document in DESIGN.md)
- Catalog of all `Any` uses with their reclassification (Unknown vs Top)
- Identification of all blame boundaries in the current codebase

### Phase 2: Split `Any`

Replace `Type::Any` with `Unknown` + `Top` in a single migration. Audit
every use of `Any` and reclassify: unannotated params to `Unknown`, builtin
returns to `Unknown`, TypeAssert upper bound to `Top`. Add the consistency
relation as `is_consistent()` alongside `is_subtype()`. Update ~20 call
sites from `is_subtype(_, Any)` to `is_consistent`.

### Phase 3: Blame Tracking

Add blame provenance to `Unknown -> Concrete` boundaries. tinct's
TypeAssert proxy contracts (Findler & Felleisen 2002) already provide the
structural mechanism via `guard_span`. Extend this to non-TypeAssert
boundaries — builtin returns and unannotated params. Start with
explicit-only (TypeAssert sites); automatic insertion of guards at all
`Unknown -> Concrete` boundaries is the full realization.

### Prerequisites

- Phase 1: no prerequisites (documentation work)
- Phase 2:
  - `let-generalization` complete (type schemes must carry `Unknown` correctly)
  - `bidirectional-typing` complete (synthesis/checking modes interact with
    consistency relation)
- Phase 3:
  - Phase 2 complete
  - Evaluator support for blame-carrying thunks (extension of TypeAssert
    proxy contracts)

### Trigger

- Phase 1 can begin at any time — it is documentation work
- Phase 2 should begin when:
  - The type system gains union types or type classes (both are incompatible
    with `Any`-as-top-and-bottom — see `doc/whatif/algebraic-subtypes.md`)
  - `Any`-as-top-and-bottom causes a type-checking false positive
  - TypeAssert contracts prove insufficient without blame tracking
- Phase 3 should begin when:
  - Phase 2 reveals runtime failures that are difficult to diagnose without
    blame provenance
  - Users report confusion about where type mismatches originate

## References

- Siek, J.G. & Taha, W. (2006). "Gradual typing for functional languages." *Scheme Workshop*, pp. 81-92. — The foundational gradual typing paper. Defines the consistency relation and the static/dynamic typing spectrum.
- Siek, J.G., Vitousek, M.M., Cimini, M. & Boyland, J.T. (2015). "Refined criteria for gradual typing." In *SNAPL '15*, LIPIcs vol. 32, pp. 274-293. — Defines the gradual guarantee. The benchmark for whether tinct's gradual typing is principled.
- Garcia, R., Clark, A.M. & Tanter, E. (2016). "Abstracting gradual typing." In *POPL '16*, pp. 429-442. ACM. — Systematic derivation of gradual type systems from static ones. The recommended framework for deriving tinct's gradual typing from HM + row polymorphism.
- Wadler, P. & Findler, R.B. (2009). "Well-typed programs can't be blamed." In *ESOP '09*, LNCS 5502, pp. 1-16. Springer. — The blame theorem. Proves that well-typed components are never blamed for runtime failures at typed/untyped boundaries.
- Findler, R. & Felleisen, M. (2002). "Contracts for higher-order functions." In *ICFP '02*, pp. 48-59. ACM. — Higher-order contracts with blame. tinct's TypeAssert proxy contracts are based on this model.
- Cimini, M. & Siek, J.G. (2016). "The gradualizer: a methodology and algorithm for generating gradual type systems." In *POPL '16*, pp. 443-455. ACM. — Automated derivation of gradual type systems. Complementary to AGT — provides an algorithmic perspective on the same problem.
- Rastogi, A., Chaudhuri, A. & Hosmer, B. (2012). "The ins and outs of gradual type inference." In *POPL '12*, pp. 481-494. ACM. — Gradual typing with type inference (not just type checking). Directly relevant since tinct uses HM inference, not explicit annotations.
