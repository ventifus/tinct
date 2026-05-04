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
intentional for tinct's gradual type system (doc/06-type-inference.md §Subtyping, Limitation
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
- TypeAssert upper bound `[@Any expr]`: `Top` — "accept any type" (this is
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
Γ ⊢ [f x] ⇒ Int
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

The **blame theorem** (Wadler & Findler 2009): when a runtime type check fails
in a gradually typed program, blame falls on the boundary that introduced the
`Unknown` value — not on the typed code that expected something concrete. A
well-typed component is never blamed.

Each blame boundary records:
- *Origin span*: where the `Unknown` value was produced (unannotated param
  definition, builtin return site, or untyped caller)
- *Boundary span*: where the `Unknown` value entered typed territory (TypeAssert
  annotation, typed argument position, or typed field access)
- *Polarity*: positive (the typed boundary made a promise the `Unknown` value
  didn't fulfill — the typed side is blamed) or negative (the `Unknown` value's
  provider violated a contract — the untyped side is blamed)

**Boundary catalog for tinct:**

| Boundary | Positive blame | Negative blame |
|----------|---------------|----------------|
| `[@Int x]` TypeAssert | The TypeAssert annotation | Provider of `x` |
| `[f x]`, `f: Int→Int`, `x: Unknown` | The argument position | Provider of `x` |
| `[f x]`, `f: Unknown` | The call site (expected callable) | Provider of `f` |
| Builtin return typed `Unknown` consumed as typed | The consuming expression | The builtin |
| `---` pipeline boundary crossing | The consuming section | The producing section |

```rust
struct BlameLabel {
    origin_span: Span,    // where Unknown value was created
    boundary_span: Span,  // where Unknown→Concrete boundary was inserted
    polarity: Polarity,
}

enum Polarity { Positive, Negative }
```

`ThunkState::Guarded` gains `blame_label: Option<BlameLabel>`. Existing
TypeAssert guards use `Some(BlameLabel { polarity: Positive, ... })`. New
automatic guards at implicit boundaries carry the appropriate label from the
type checker's elaboration pass.

When a runtime check fails, the error message identifies both the failure
point and the `Unknown` origin:

```
type assertion failed at line 5: expected Int, got String
  asserted by: [@Int ...] at line 5
  value originated from: unannotated parameter x at line 3
```

For automatic insertion at implicit boundaries:

```
type mismatch at line 12: add expected Int for first argument
  blame: value from line 7 (from-json result, Unknown type)
  untyped boundary at line 7 is responsible
```

### Interaction with Lazy Evaluation

Blame boundaries interact with lazy evaluation: a blame check on a thunk must
be deferred until the thunk is forced. Blame labels therefore attach to
*thunks*, not values — the boundary wraps the thunk, and blame fires when the
wrapper is forced. When a thunk is forced and its value doesn't match the
expected type, blame falls on the boundary where the thunk was created, not
where it was forced.

This is the **eager contract wrapping** strategy from Findler & Felleisen
(2002): `guard(inner, τ, blame_label)` creates a `ThunkState::Guarded` thunk
that, when forced, materializes `inner` and validates the result against τ. If
the result is wrong, the blame label identifies the boundary. This is already
how TypeAssert works via `guard_span` — Phase 3 extends the mechanism to
implicit boundaries.

The alternative — **deferred blame** (carry a label with the value, check only
at force sites) — is more complex and unnecessary: tinct's `ThunkState::Guarded`
already thread blame through the lazy evaluation chain.

**Space-efficient blame.** Naive blame tracking accumulates labels as values
cross multiple boundaries, consuming O(n) space. Greenman, Felleisen & Dimoulas
(2019) identify three strategies:

- *Natural*: all labels preserved — full provenance, O(n) space
- *Co-natural*: only the innermost label — O(1) space, inner boundaries shadow outer
- *Forgetful*: outermost label only — O(1) space, may assign blame to wrong boundary

**Co-natural** is the right default for tinct: keep the innermost blame label
(the most specific boundary, e.g., the TypeAssert annotation) and discard outer
labels when a value crosses a second boundary. This preserves the most
actionable information while maintaining constant space overhead per thunk.

**`---` pipeline boundary blame.** The pipeline `---` boundary between document
sections is a natural blame point. With blame tracking, a type failure in
section 3 can report "value from section 1 (untyped), passed through the `---`
boundary at line 45, did not match the expected type." Each `---` boundary is
treated as an implicit TypeAssert — the downstream section's type expectations
create a blame boundary for the upstream section's output.

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
check is inserted at the point where `=` is called on the `Unknown` value.

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

**Proposed (explicit-only, Phase 3a):** Extend `ThunkState::Guarded` with
`blame_label: Option<BlameLabel>`. Existing TypeAssert guards gain a
`BlameLabel { polarity: Positive, origin_span, boundary_span }`. `Unknown`
values that don't cross a TypeAssert boundary still produce point-of-failure
errors without blame provenance. The evaluator is otherwise unchanged.

**Proposed (automatic insertion, Phase 3b):** The type checker elaborates every
`Unknown -> Concrete` boundary into an explicit `ThunkState::Guarded` with a
blame label. Whenever synthesis produces `Unknown` and the checking context
expects a concrete type, elaboration inserts a guard thunk. This requires
the type checker to run in elaboration mode, embedding guards into the AST
before evaluation. The evaluator unwraps guards at force time (already done
for TypeAssert guards). The `---` pipeline boundary creates implicit guards at
the start of each consuming section.

**Impact:** Minor (Phase 3a — extend existing guard mechanism). Major (Phase
3b — elaboration pass, guards at all implicit boundaries).

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

**Proposed:** Blame-aware errors: "type mismatch at line 5: argument to add
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
- Formal consistency relation for tinct's types (document in doc/06-type-inference.md)
- Catalog of all `Any` uses with their reclassification (Unknown vs Top)
- Identification of all blame boundaries in the current codebase

### Phase 2: Split `Any`

Replace `Type::Any` with `Unknown` + `Top` in a single migration. Audit
every use of `Any` and reclassify: unannotated params to `Unknown`, builtin
returns to `Unknown`, TypeAssert upper bound to `Top`. Add the consistency
relation as `is_consistent()` alongside `is_subtype()`. Update ~20 call
sites from `is_subtype(_, Any)` to `is_consistent`.

### Phase 3: Blame Tracking

Full blame tracking proceeds in two sub-phases. Phase 3a extends the existing
TypeAssert mechanism; Phase 3b adds automatic guard insertion at all implicit
`Unknown -> Concrete` boundaries.

**Phase 3a: Explicit blame (TypeAssert sites).** Add `BlameLabel` to
`ThunkState::Guarded`. Every TypeAssert-generated guard already has the needed
information (`guard_span`, expected type, field path). The change:

```rust
ThunkState::Guarded {
    inner: Rc<Thunk>,
    expected: Type,
    field_path: Vec<String>,
    guard_span: Span,        // existing
    blame_label: BlameLabel, // new: polarity + origin_span + boundary_span
}
```

Update error reporting to emit the blame provenance chain. Error format:

```
type assertion failed at line 5: expected Int, got String
  asserted by: [@Int ...] at line 5
  value originated from: unannotated parameter x at line 3
```

Deliverables: `BlameLabel` struct in `src/eval.rs`, guard construction updated
in TypeAssert elaboration, error formatting updated in `src/error.rs`.

**Phase 3b: Automatic insertion at implicit boundaries.** The type checker
elaborates every `Unknown -> Concrete` boundary — not just TypeAssert — into
a `ThunkState::Guarded`. Boundaries:

- Function argument: `[f x]` where `x: Unknown` and `f` has typed params
- Builtin argument: `[add x y]` where `x: Unknown` and `add: Int→Int→Int`
- Field access on Unknown: `x.name` where `x: Unknown` (when row constraint
  is generated, a guard wraps x's resolution)
- `---` pipeline crossing: typed expressions consuming values from prior sections

Guard insertion follows the blame calculus rules (Wadler & Findler 2009):
positive blame on the typed boundary, negative on the Unknown origin. The
elaboration pass runs after type inference and before evaluation, embedding
guards into the AST. The evaluator is unchanged — guards are forced as part
of normal thunk materialization.

**Space-efficient blame.** Use co-natural strategy (Greenman et al. 2019):
when a value with an existing blame label crosses a second boundary, the outer
label is discarded and the inner (most recent) label is kept. This ensures
O(1) space overhead per thunk regardless of how many boundaries a value
crosses. The innermost label is the most actionable: it identifies the specific
boundary closest to the consumer that expected a concrete type.

**Blame at `---` boundaries.** The pipeline model gives blame tracking
particular value. With Phase 3b, each `---` boundary is an implicit
TypeAssert over the document section interface. A type failure deep in section
3 can report:

```
type mismatch: transform expected [name: Str  age: Int], got Unknown
  blame: value produced in section 1 (line 12, untyped from-json result)
  untyped boundary: --- at line 30
```

This makes tinct's multi-section pipeline errors actionable — the user knows
exactly which section produced the untyped value and which boundary failed to
protect the downstream section from it.

### Prerequisites

- Phase 1: no prerequisites (documentation work)
- Phase 2:
  - `let-generalization` complete (type schemes must carry `Unknown` correctly)
  - `bidirectional-typing` complete (synthesis/checking modes interact with
    consistency relation)
- Phase 3a:
  - Phase 2 complete
  - `ThunkState::Guarded` already in place (TypeAssert proxy contracts)
- Phase 3b:
  - Phase 3a complete
  - Type checker supports elaboration mode (embeds guards into AST)
  - `---` boundary blame requires pipeline-level elaboration pass

### Trigger

- Phase 1 can begin at any time — it is documentation work
- Phase 2 should begin when:
  - The type system gains union types or type classes (both are incompatible
    with `Any`-as-top-and-bottom — see `doc/whatif/algebraic-subtypes.md`)
  - `Any`-as-top-and-bottom causes a type-checking false positive
  - TypeAssert contracts prove insufficient without blame tracking
- Phase 3a should begin when:
  - Phase 2 is complete and the first real-world type failures are reported
    without clear blame attribution
  - TypeAssert errors reference source lines that are far from the actual
    origin of mismatched values
- Phase 3b should begin when:
  - Phase 3a is in place and implicit boundaries (unannotated params, builtin
    returns) are the dominant source of confusing type errors
  - The `---` pipeline boundary is a common site of type mismatches that users
    cannot trace to the originating section

## References

- Siek, J.G. & Taha, W. (2006). "Gradual typing for functional languages." *Scheme Workshop*, pp. 81-92. — The foundational gradual typing paper. Defines the consistency relation and the static/dynamic typing spectrum.
- Siek, J.G., Vitousek, M.M., Cimini, M. & Boyland, J.T. (2015). "Refined criteria for gradual typing." In *SNAPL '15*, LIPIcs vol. 32, pp. 274-293. — Defines the gradual guarantee. The benchmark for whether tinct's gradual typing is principled.
- Garcia, R., Clark, A.M. & Tanter, E. (2016). "Abstracting gradual typing." In *POPL '16*, pp. 429-442. ACM. — Systematic derivation of gradual type systems from static ones. The recommended framework for deriving tinct's gradual typing from HM + row polymorphism.
- Wadler, P. & Findler, R.B. (2009). "Well-typed programs can't be blamed." In *ESOP '09*, LNCS 5502, pp. 1-16. Springer. — The blame theorem. Proves that well-typed components are never blamed for runtime failures at typed/untyped boundaries.
- Findler, R. & Felleisen, M. (2002). "Contracts for higher-order functions." In *ICFP '02*, pp. 48-59. ACM. — Higher-order contracts with blame. tinct's TypeAssert proxy contracts are based on this model.
- Cimini, M. & Siek, J.G. (2016). "The gradualizer: a methodology and algorithm for generating gradual type systems." In *POPL '16*, pp. 443-455. ACM. — Automated derivation of gradual type systems. Complementary to AGT — provides an algorithmic perspective on the same problem.
- Rastogi, A., Chaudhuri, A. & Hosmer, B. (2012). "The ins and outs of gradual type inference." In *POPL '12*, pp. 481-494. ACM. — Gradual typing with type inference (not just type checking). Directly relevant since tinct uses HM inference, not explicit annotations.
- Greenman, B., Felleisen, M. & Dimoulas, C. (2019). "Complete monitors for gradual types." In *ICFP '19*, Article 122. ACM. — Defines natural, co-natural, and forgetful blame strategies. Proves that co-natural is sufficient for the blame theorem while maintaining O(1) space overhead. tinct Phase 3 adopts co-natural.
- Ahmed, A., Findler, R., Siek, J. & Wadler, P. (2011). "Blame for all." In *POPL '11*, pp. 201-214. ACM. — Extends blame to polymorphic languages. Relevant because tinct has parametric polymorphism via annotated type variables; blame for polymorphic function boundaries requires sealing the type variable's instantiation.
- Vitousek, M., Kent, A., Siek, J. & Baker, J. (2014). "Design and evaluation of gradual typing for Python." In *DLS '14*, pp. 45-56. ACM. — Implementation experience (Reticulated Python). Shows that automatic guard insertion at implicit boundaries is feasible at scale; the key cost is guard creation on hot paths.
