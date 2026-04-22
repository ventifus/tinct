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

## What Formal Gradual Typing Provides

### The Consistency Relation

In proper gradual typing, the "unknown type" `?` relates to other types via a
**consistency relation** (`~`), not subtyping:

```
τ ~ ?        (? is consistent with everything)
? ~ τ        (symmetric)
Int ~ Int     (reflexive on concrete types)
Int ≁ String  (concrete types must match)
```

Key property: **consistency is not transitive.** `Int ~ ?` and `? ~ String`,
but `Int ≁ String`. This prevents `?` from collapsing all types into
equivalence (which tinct's `Any`-as-top-and-bottom does).

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
`Any`-typed values should succeed vs. fail.

### Blame Tracking (Wadler & Findler 2009)

When a gradually typed program fails at runtime, **blame** identifies which
boundary between typed and untyped code is responsible. The blame theorem:
a well-typed component is never blamed.

tinct's TypeAssert provides the structural equivalent of blame boundaries
(the `guard_span` field tracks where the contract was introduced), but
non-TypeAssert `Any` boundaries (builtin returns, unannotated params) have
no blame tracking.

## What Would Change

### 1. Split `Any` into Three Concepts

**Current:** `Type::Any` — one type, three roles.

**Gradual typing:**
```rust
enum Type {
    // ... existing types ...
    Unknown,      // ? — gradual typing (consistency, not subtyping)
    Top,          // ⊤ — supertype of everything (for type bounds)
    // Bottom not needed unless algebraic subtyping adopted
}
```

- Unannotated params: `Unknown` (was `Any`)
- Builtin returns that can't be typed: `Unknown` (was `Any`)
- TypeAssert upper bound: `Top` (for `[@Any $expr]` meaning "accept anything")

**Impact: Major.** Every use of `Type::Any` must be audited and reclassified.
Unification rules change: `unify(Unknown, τ)` uses consistency, not subtyping.
`is_subtype` no longer has `[S-ANY-TOP]` and `[S-ANY-BOT]` — those become
`τ <: Top` and nothing for `Unknown` (consistency is a separate relation).

### 2. Consistency Relation

**New function:**
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
        // ... record, seq cases ...
        _ => false,
    }
}
```

Used where `Any` currently triggers silent success: CALL-ANY, DOT-OPEN,
BRACKET-DYN, ACCESS-ANY.

**Impact: Moderate.** New function, ~20 call sites updated from
`is_subtype(_, Any)` to `is_consistent`.

### 3. Runtime Checks at Boundaries

Every point where `Unknown` meets a concrete type needs a runtime check:

```
Γ ⊢ f : Fn(Int → Int),  Γ ⊢ x : Unknown
──────────────────────────────────────────
Γ ⊢ [call f x] ⇒ Int
  with runtime check: materialize(x) must be Int
```

tinct's TypeAssert proxy contracts (Findler & Felleisen 2002) already provide
the mechanism. The question is whether to automatically insert guards at
`Unknown → Concrete` boundaries or only at explicit TypeAssert sites.

**Impact: Major if automatic, Minor if explicit-only.** Automatic insertion
changes the evaluation model significantly. Explicit-only (current behavior)
means `Any`→`Concrete` mismatches are caught only at runtime materialization,
not at the function boundary.

### 4. Error Messages

**Current:** "expected Int, got String" at the point of materialization.

**Gradual typing with blame:** "type mismatch at line 5: argument to $add
expected Int, but value from line 3 (untyped) was String. The untyped
boundary at line 3 is responsible (blame)."

**Impact: Major.** Blame tracking requires provenance through the entire
evaluation, not just point-of-failure reporting.

## What We'd Gain

1. **Sound gradual typing** — `Any` no longer collapses the type lattice
2. **Principled boundary semantics** — clear rules for what happens when
   typed meets untyped
3. **Better error messages** — blame identifies the untyped origin, not just
   the failure point
4. **Gradual guarantee** — annotations can be added/removed without
   surprising static behavior changes

## What We'd Lose / Risk

1. **Complexity** — three type concepts instead of one, new consistency
   relation, blame tracking infrastructure
2. **Performance** — automatic runtime checks at boundaries add overhead
3. **`Any` ergonomics** — current behavior is simple and predictable;
   users may not understand the distinction between `Unknown` and `Top`
4. **Breaking change** — programs relying on `Any <: τ` would break when
   `Unknown ≁ τ` for incompatible concrete types

## Recommendation

**Don't adopt now.** tinct's `Any`-as-top-and-bottom has not caused a
soundness problem in practice. The main risk (type lattice collapse) is
theoretical — no user has reported a program that type-checked but shouldn't
have due to `Any` semantics.

**Revisit when:**
- A user reports a program that type-checked incorrectly due to `Any`
  semantics (the collapse causes a false positive)
- TypeAssert proxy contracts prove insufficient for runtime type safety at
  boundaries (blame tracking needed for practical debugging)
- The type system gains union types or type classes, which interact badly
  with `Any`-as-top-and-bottom (algebraic subtyping requires splitting `Any`
  into Top/Bottom — see `doc/whatif/algebraic.md` §6 "Any Type")

**If we do adopt,** formalize the consistency relation first (documentation
only, no code change). This establishes the rules before implementation.
Then split `Type::Any` into `Unknown` + `Top` as a single migration. Use
AGT (Garcia et al. 2016) to derive the gradual rules systematically from
the existing static type system.

## Trigger for Phase 2 Formalization

Before reaching Phase 3, the type system extension roadmap (DESIGN.md §Type
System Extension Roadmap) calls for formalizing `Any`'s semantics in Phase 2
as documentation work: document the consistency relation that `Any` actually
implements, define what the Gradual Guarantee means for tinct, and identify
blame boundaries. This formalization does not require code changes but
establishes the foundation for Phase 3 if it's ever triggered.

## References

- Siek, J.G. & Taha, W. (2006). "Gradual typing for functional languages."
  *Scheme Workshop*, pp. 81–92.
- Siek, J.G., Vitousek, M.M., Cimini, M. & Boyland, J.T. (2015). "Refined
  criteria for gradual typing." In *SNAPL '15*, LIPIcs vol. 32, pp. 274–293.
- Garcia, R., Clark, A.M. & Tanter, E. (2016). "Abstracting gradual typing."
  In *POPL '16*, pp. 429–442. ACM.
- Wadler, P. & Findler, R.B. (2009). "Well-typed programs can't be blamed."
  In *ESOP '09*, LNCS 5502, pp. 1–16. Springer.
- Findler, R. & Felleisen, M. (2002). "Contracts for higher-order functions."
  In *ICFP '02*, pp. 48–59. ACM.
