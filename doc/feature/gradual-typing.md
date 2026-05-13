# Formal Gradual Typing

## Overview

Tinct implements formal gradual typing based on AGT (Garcia et al. 2016). The core change splits the former `Any` type into two distinct variants:

- **`Unknown`** (`?`) — the gradual unknown type. Relates to other types via the consistency relation (`~`), not subtyping. Used for unannotated parameters, builtin returns that cannot be precisely typed, and forward references before let-generalization.
- **`Top`** (`⊤`) — the true supertype of everything. Used for TypeAssert upper bound (`[@Any expr]`) and type bounds where universal acceptance is intended.

The consistency relation is reflexive and symmetric but not transitive. `Int ~ Unknown` and `Unknown ~ String`, but `Int ≁ String`. This prevents `Unknown` from collapsing all types into equivalence, which `Any`-as-top-and-bottom did.

Blame tracking identifies which boundary between typed and untyped code is responsible when a runtime type check fails. The blame theorem (Wadler & Findler 2009): a well-typed component is never blamed.

## Supersession Notes

- **`Type::Any` split**: `Type::Any` was replaced by two distinct types: `Type::Unknown` (gradual typing opt-out — the `?` type, consistent with anything) and `Type::Top` (the true supertype, accepts all values within the lattice). Any section referring to `Type::Any` uses stale terminology. See [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09).
- **Phase 3b (automatic guard insertion)**: Automatic guard insertion at all `Unknown → Concrete` boundaries. Phase 3a (TypeAssert-site blame tracking) is the foundation; Phase 3b extends it with systematic boundary guards.

## Design

### The Consistency Relation

In proper gradual typing, the unknown type `?` relates to other types via a **consistency relation** (`~`), not subtyping:

```
τ ~ ?        (? is consistent with everything)
? ~ τ        (symmetric)
Int ~ Int     (reflexive on concrete types)
Int ≁ String  (concrete types must match)
```

Key property: **consistency is not transitive.** `Int ~ ?` and `? ~ String`, but `Int ≁ String`. This prevents `?` from collapsing all types into equivalence.

### The Gradual Guarantee (Siek et al. 2015)

A gradually typed system satisfies the **gradual guarantee** if:
1. Removing type annotations (replacing with `?`) never causes a program to be statically rejected
2. Adding type annotations never causes a program that was statically accepted to be rejected — but may cause runtime failures at typed/untyped boundaries

### AGT (Garcia et al. 2016)

Abstracting Gradual Typing provides a systematic method to derive a gradual type system from a static one:
1. `?` represents the set of all types
2. Static typing judgments lift to operate on sets of types
3. A gradual judgment holds if some consistent concretization satisfies the static judgment

Applied to tinct: starting from the HM + row polymorphism static system, AGT systematically derives the gradual version, determining exactly where runtime checks are needed.

### Split `Any` into Three Concepts

The core change replaces `Type::Any` with three distinct types, each serving one of the roles `Any` conflated:

```rust
enum Type {
    // ... existing types ...
    Unknown,      // ? — gradual typing (consistency, not subtyping)
    Top,          // ⊤ — supertype of everything (for type bounds)
    // Bottom not needed unless algebraic subtyping adopted
}
```

Role reclassification:
- Unannotated params: `Unknown` — "I don't know the type yet"
- Builtin returns that can't be typed: `Unknown` — "could be anything"
- TypeAssert upper bound `[@Any expr]`: `Top` — "accept any type" (this is a true supertype, not gradual unknown)
- Forward references before let-generalization: fresh type variables (not `Any`)

### Consistency Relation

`is_consistent(a, b) -> bool` exists alongside `is_subtype(a, b)`:

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

> **Note:** The `is_consistent` implementation in `src/types.rs:531` also has `(Top, _) | (_, Top) => true` — any type is consistent with `Top`.

`is_consistent` replaces `is_subtype` at every point where `Any` previously triggered silent success: CALL-ANY, DOT-OPEN, BRACKET-DYN, ACCESS-ANY.

### Runtime Checks at Boundaries

Every point where `Unknown` meets a concrete type is a blame boundary requiring a runtime check:

```
Γ ⊢ f : Fn(Int → Int),  Γ ⊢ x : Unknown
──────────────────────────────────────────
Γ ⊢ [f x] ⇒ Int
  with runtime check: materialize(x) must be Int
```

Tinct's TypeAssert proxy contracts (Findler & Felleisen 2002) provide the mechanism. Two modes:

- **Explicit-only** (Phase 3a): runtime checks only at TypeAssert sites. `Unknown -> Concrete` mismatches caught at runtime materialization, not at the function boundary.
- **Automatic insertion** (Phase 3b): the compiler inserts runtime guards at every `Unknown -> Concrete` boundary.

### Blame Provenance

Each blame boundary records:
- *Origin span*: where the `Unknown` value was produced
- *Boundary span*: where the `Unknown` value entered typed territory
- *Polarity*: positive (typed boundary made a promise the `Unknown` value didn't fulfill) or negative (`Unknown` value's provider violated a contract)

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

`ThunkState::Guarded` carries `blame_label: Option<BlameLabel>`. TypeAssert guards use `Some(BlameLabel { polarity: Positive, ... })`. Automatic guards at implicit boundaries carry the appropriate label from the type checker's elaboration pass.

When a runtime check fails, the error message identifies both the failure point and the `Unknown` origin:

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

Blame boundaries interact with lazy evaluation: a blame check on a thunk is deferred until the thunk is forced. Blame labels attach to thunks, not values — the boundary wraps the thunk, and blame fires when the wrapper is forced.

This is the **eager contract wrapping** strategy from Findler & Felleisen (2002): `guard(inner, τ, blame_label)` creates a `ThunkState::Guarded` thunk that, when forced, materializes `inner` and validates the result against τ. This is already how TypeAssert works via `guard_span`.

**Space-efficient blame.** The co-natural strategy (Greenman et al. 2019) is used: when a value with an existing blame label crosses a second boundary, the outer label is discarded and the inner (most recent) label is kept. This preserves the most actionable information while maintaining constant space overhead per thunk.

**`---` pipeline boundary blame.** Each `---` boundary is a natural blame point. With blame tracking, a type failure in section 3 can report "value from section 1 (untyped), passed through the `---` boundary at line 45, did not match the expected type." Each `---` boundary is treated as an implicit TypeAssert.

### Interaction with Row Polymorphism

`Unknown` in record fields means "this field has unknown type." Row consistency handles:

```
is_consistent(Record({x: Int, y: Unknown}), Record({x: Int, y: String}))
→ true  (because Unknown ~ String)
```

Open records with `Unknown` tails are consistent with any record that has the known fields:

```
is_consistent(Record({x: Int, ...Unknown}), Record({x: Int, y: String}))
→ true
```

This follows directly from AGT's lifting: the static row unification rules lift to operate on sets of row types, and `Unknown` represents the set of all possible row tails.

### Interaction with Type Classes

The AGT approach resolves the constraint interaction: `Unknown` satisfies a constraint if some concretization of `Unknown` satisfies it. Since `Unknown` represents all types and some types have `Eq`, `Eq Unknown` is satisfied — but a runtime check is inserted at the point where `=` is called on the `Unknown` value. This preserves the gradual guarantee while maintaining constraint soundness.

## Implementation

### Type Representation (`src/types.rs`)

`Type::Any` is replaced by `Type::Unknown` (gradual) and `Type::Top` (true supertype). The `Type` enum gains one variant (`Unknown` replaces `Any`, `Top` is new) and loses one (`Any`). Every match arm for the former `Type::Any` has been audited and reclassified. Unification rules: `unify(Unknown, τ)` uses consistency, not subtyping.

### Subtyping (`src/types.rs`)

`is_subtype` has no `Any` rules. `τ <: Top` replaces the former `τ <: Any`. No type is a subtype of `Unknown` and `Unknown` is not a subtype of any concrete type — that relationship is handled by `is_consistent`. `is_subtype` is a proper partial order (reflexive, transitive, antisymmetric).

### Type Inference (`src/typecheck.rs`)

`unify(Unknown, τ)` records a consistency judgment rather than binding. The inference engine distinguishes between "I don't know this type" (`Unknown`, deferred to runtime) and "this type is unconstrained" (fresh type variable, resolved by inference). This prevents `Unknown` from interfering with principal type inference for well-typed subexpressions.

### Evaluator (`src/eval.rs`)

**Phase 3a (explicit blame):** `ThunkState::Guarded` carries `blame_label: Option<BlameLabel>`. TypeAssert guards carry `BlameLabel { polarity: Positive, origin_span, boundary_span }`. `Unknown` values that don't cross a TypeAssert boundary still produce point-of-failure errors without blame provenance.

**Phase 3b (automatic insertion):** The type checker elaborates every `Unknown -> Concrete` boundary into an explicit `ThunkState::Guarded` with a blame label. The `---` pipeline boundary creates implicit guards at the start of each consuming section.

### TypeAssert (`src/typecheck.rs`, `src/eval.rs`)

`expr :: Type` with `Any` annotation becomes `expr :: Top` (accept any type). TypeAssert with concrete annotations generates consistency checks when the inferred type is `Unknown`. The existing `guard_span` mechanism serves as the blame label.

### Error Messages (`src/error.rs`)

Blame-aware errors identify both the failure point and the origin: "type mismatch at line 5: argument to add expected Int, but value from line 3 (untyped) was String. The untyped boundary at line 3 is responsible." Blame provenance threads through the evaluation for full attribution.

## References

- Siek, J.G. & Taha, W. (2006). "Gradual typing for functional languages." *Scheme Workshop*, pp. 81-92. — The foundational gradual typing paper. Defines the consistency relation and the static/dynamic typing spectrum.
- Siek, J.G., Vitousek, M.M., Cimini, M. & Boyland, J.T. (2015). "Refined criteria for gradual typing." In *SNAPL '15*, LIPIcs vol. 32, pp. 274-293. — Defines the gradual guarantee. The benchmark for whether tinct's gradual typing is principled.
- Garcia, R., Clark, A.M. & Tanter, E. (2016). "Abstracting gradual typing." In *POPL '16*, pp. 429-442. ACM. — Systematic derivation of gradual type systems from static ones. The recommended framework for deriving tinct's gradual typing from HM + row polymorphism.
- Wadler, P. & Findler, R.B. (2009). "Well-typed programs can't be blamed." In *ESOP '09*, LNCS 5502, pp. 1-16. Springer. — The blame theorem. Proves that well-typed components are never blamed for runtime failures at typed/untyped boundaries.
- Findler, R. & Felleisen, M. (2002). "Contracts for higher-order functions." In *ICFP '02*, pp. 48-59. ACM. — Higher-order contracts with blame. tinct's TypeAssert proxy contracts are based on this model.
- Cimini, M. & Siek, J.G. (2016). "The gradualizer: a methodology and algorithm for generating gradual type systems." In *POPL '16*, pp. 443-455. ACM. — Automated derivation of gradual type systems. Complementary to AGT — provides an algorithmic perspective on the same problem.
- Rastogi, A., Chaudhuri, A. & Hosmer, B. (2012). "The ins and outs of gradual type inference." In *POPL '12*, pp. 481-494. ACM. — Gradual typing with type inference (not just type checking). Directly relevant since tinct uses HM inference, not explicit annotations.
- Greenman, B., Felleisen, M. & Dimoulas, C. (2019). "Complete monitors for gradual types." *Proc. ACM Program. Lang.* 3, OOPSLA, Article 122. doi:10.1145/3360548. — Defines natural, co-natural, and forgetful blame strategies. Proves that co-natural is sufficient for the blame theorem while maintaining O(1) space overhead. tinct adopts co-natural.
- Ahmed, A., Findler, R., Siek, J. & Wadler, P. (2011). "Blame for all." In *POPL '11*, pp. 201-214. ACM. — Extends blame to polymorphic languages. Relevant because tinct has parametric polymorphism via annotated type variables; blame for polymorphic function boundaries requires sealing the type variable's instantiation.
- Vitousek, M., Kent, A., Siek, J. & Baker, J. (2014). "Design and evaluation of gradual typing for Python." In *DLS '14*, pp. 45-56. ACM. — Implementation experience (Reticulated Python). Shows that automatic guard insertion at implicit boundaries is feasible at scale; the key cost is guard creation on hot paths.
