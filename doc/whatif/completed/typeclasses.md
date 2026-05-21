# What If: Type Classes for tinct

**State:** Accepted — 2026-05-05

What would it take to add type classes (ad-hoc polymorphism) to tinct?

## Current State

tinct has a small, closed set of built-in types (Int, Float, Number, Str,
Bool, Null, Dict/Record, Seq, Function) with hardcoded promotion and
comparison tables:

```text
# Arithmetic: hardcoded promotion table (doc/03-data-model.md §Numeric Types)
Int + Int     → Int
Int + Float   → Float  (Int promotes to Float)
Float + Float → Float

# Comparison: hardcoded (doc/11-stdlib.md §Equality and Comparison)
= works on Int, Float, String, Bool
< works on Int, Float, String
Cross-type Int/Float comparison allowed
Dict, Function, Builtin return false for =
```

Dual-dispatch builtins (`map`, `filter`, etc.) are typed as `Any` because
the precise type `Dict | Seq` cannot be expressed (doc/07-type-extensions.md §Dual-Dispatch
Builtins).

### What's Missing

1. **Constrained polymorphism.** Overloaded builtins (`=`, `+`, `map`)
   are typed as `Any` — the type system cannot express "works on types that
   support equality" or "works on types that support mapping."
2. **Static rejection of invalid operations.** `[= [fn [] 1] [fn [] 2]]`
   is accepted by the type checker (both args are `Any`-compatible) but fails
   at runtime. Type classes would reject this statically.
3. **Structured overloading.** Adding new "protocols" (e.g., Serializable,
   Comparable) requires ad-hoc runtime dispatch rather than type-level
   declaration.
4. **Precise dual-dispatch typing.** `map` over Dict vs Seq cannot be
   expressed without either union types or a `Mappable` class.

## What Type Classes Would Provide

### Constrained Polymorphism

Instead of `= : Any → Any → Bool`, type classes enable:

```text
= : Equatable a => a → a → Bool
< : Comparable a => a → a → Bool
+ : Numeric a => a → a → a
map : Mappable f => (a → b) → f a → f b
```

This rejects `[= [fn [] 1] [fn [] 2]]` at type-check time (Function
has no Equatable instance) while accepting `[= 1 2]` (Int has Equatable).

### Required Classes for tinct

| Class | Methods | Instances |
|-------|---------|-----------|
| `Equatable` | `=` | Int, Float, Str, Bool, Null |
| `Comparable` | `<`, `>`, `<=`, `>=` | Int, Float, Str |
| `Numeric` | `+`, `-`, `*`, `/`, `%`, `neg` | Int, Float |
| `Showable` | `str` | All types |
| `Appendable` | `concat`, `++` | Str, Seq, Dict |
| `Mappable` | `map` | Dict, Seq |
| `Foldable` | `reduce`, `length` | Dict, Seq |
| `Filterable` | `filter` | Dict, Seq |

### Instance Laws

Type classes carry laws that instances must satisfy:

**Equatable:**

- Reflexivity: `[= x x]` → true (except NaN)
- Symmetry: `[= x y]` = `[= y x]`
- Transitivity: if `[= x y]` and `[= y z]` then `[= x z]`

**Comparable:**

- Antisymmetry, transitivity, totality
- Consistent with Equatable: `[= x y]` iff `[<= x y]` and `[>= x y]`

**Numeric:**

- Additive identity: `[+ x 0]` = `x`
- Additive inverse: `[+ x [neg x]]` = 0
- Commutativity: `[+ x y]` = `[+ y x]`

**NaN exception:** Float's Equatable instance violates reflexivity (`NaN != NaN`).
This is universal across languages (IEEE 754) and should be documented as
an exception to the law.

## Equatable for Records

### Current Behavior

`=` returns `false` for all Dict and Seq comparisons (EQ-INCOMP rule in
doc/11-stdlib.md §Equality and Comparison). This is intentional: structural dict
equality would force all field thunks, violating lazy evaluation. Comparing
`[x: [/ 1 0]]` with itself would trigger the division-by-zero in an
unreferenced field.

doc/11-stdlib.md §Equality P1 flags a **future breaking change**: if `=` gained
structural comparison, `[= [x: 1] [x: 1]]` would change from `false`
to `true`, breaking code that relies on dicts always being unequal.

### Proposed Approach: Separate Functions

Keep `=` unchanged (EQ-INCOMP for dicts). Add two new builtins for
structural record comparison:

**`deep-eq : Any → Any → Bool`** — Eager structural equality with
short-circuit. Compares field-by-field, forcing each pair lazily. Returns
`false` at the first difference without forcing remaining fields. Cost:
O(first_difference), not O(total_size). For primitive types, behaves
identically to `=`.

**`shallow-eq : Any → Any → Bool`** — Structural equality for already-
materialized structure. Compares keys and materialized values; unevaluated
thunks compare by pointer identity (`Rc::ptr_eq`). Cost: O(n) for n keys,
no additional forcing.

Note: `deep-eq` is NOT equivalent to `[shallow-eq [eval a] [eval b]]`
because the composed version forces *everything* in both
values before comparing, while `deep-eq` short-circuits at the first
difference. For large config dicts that differ in one field, the difference
is significant.

### Key-Set Equality (Order-Independent)

Structural equality uses key-set comparison, not insertion-order comparison:
`[a: 1, b: 2]` equals `[b: 2, a: 1]` under `deep-eq`. This is more
natural for configuration data where key order is incidental. It diverges
from `IndexMap` iteration order, but config semantics should not depend on
key ordering.

Implementation: sort keys or use hash-set comparison before field-by-field
value comparison.

### No Breaking Change to `=`

This approach sidesteps the P1 breaking change entirely. `=` remains fast,
predictable, and primitive-only. Users who need structural comparison opt in
explicitly with `deep-eq` or `shallow-eq`.

When typeclasses are adopted, `deep-eq` becomes the `Equatable` method with
structural derivation. `=` stays as the primitive equality operator.

## Default Derivation Strategy

When typeclasses are adopted, `Equatable` for records uses **structural
derivation with key-set semantics**:

Two records are equal iff:

1. They have the same set of keys (regardless of insertion order)
2. All corresponding values are equal (recursive `Equatable` check)

This requires `Equatable` on all field types. A record containing a
`Function` value cannot derive `Equatable` (functions have no meaningful
equality).

For sequences: two sequences are equal iff they have the same length and
all corresponding elements are equal (recursive `Equatable` check). Infinite
sequences are compared lazily — if they diverge at position n, `deep-eq`
returns `false` at position n. Two identical infinite sequences would
diverge (non-termination), which is correct: equality on infinite
structures is undecidable.

## Design

Adopt a two-phase approach: Elm-style constrained type variables first,
then full Haskell-style type classes when extensibility is needed.

### Phase 1 Design: Constrained Type Variables

A fixed set of built-in constraints with no user-extensible class
declarations. This follows Elm's approach — pragmatic, avoids the
complexity of dictionary passing, and covers tinct's immediate needs:

```text
Equatable  : types that support =, !=     (Int, Float, Str, Bool, Null)
Comparable : types that support <, >, <=  (Int, Float, Str)
Numeric    : types that support +, -, *   (Int, Float)
Appendable : types that support ++        (Str, Seq, Dict)
Showable   : types that support str       (All types)
Mappable   : types that support map       (Dict, Seq)
Foldable   : types that support fold      (Dict, Seq)
Filterable : types that support filter    (Dict, Seq)
```

This replaces `Any` typing on overloaded builtins with constrained
signatures:

```text
= : Equatable a => a → a → Bool          (was: Any → Any → Bool)
+ : Numeric a => a → a → a               (was: Any → Any → Any)
map : Mappable f => (a → b) → f a → f b  (was: Any)
```

#### Type Representation

```rust
enum Constraint {
    Class(String, String),  // (class_name, type_var)
}

struct TypeScheme {
    pub type_vars: Vec<String>,
    pub row_vars: Vec<String>,
    pub constraints: Vec<Constraint>,  // NEW
    pub body: Type,
}

// Display: "Equatable a => Fn(a, a → Bool)" or "Numeric a, Mappable f => ..."
```

#### Inference Semantics

Constrained type variables generate constraints during inference. When a
variable `a` is used with `=`, the constraint `Equatable a` is recorded.
During let-generalization, constraints on generalized variables become part
of the type scheme. During instantiation, constraints are checked against
the fixed instance sets:

```text
G |- = => Equatable a => Fn(a, a -> Bool)    [instantiate with fresh a]
G |- 1 => IntLiteral(1)
unify(a, IntLiteral(1))  ->  a = IntLiteral(1)
check: Equatable IntLiteral(1)?  ->  yes (Int has Equatable, IntLiteral <: Int)
```

No class declarations, no dictionary passing, no instance resolution
search. The instance sets are hardcoded in the type checker.

### Phase 2 Design: Full Type Classes

Evolve to Wadler & Blott (1989) type classes when extensibility is needed.
The Phase 1 constraints become type classes with fixed instance sets —
this is forward-compatible by design. Phase 2 adds:

- **Class declarations** with method signatures
- **Instance declarations** (initially only for built-in types)
- **Superclass hierarchy** (Comparable implies Equatable)
- **Dictionary passing at runtime** — each class instance is compiled to a
  record of method implementations, passed as an implicit parameter

#### Dictionary Passing and Lazy Evaluation

Dictionary passing in a lazy language requires care. In Haskell, dictionaries
are strict values (they are always evaluated before being passed). In tinct,
dictionaries are lazy — a naively-constructed class dictionary would defer
method lookups. The simplest resolution: class dictionaries are eagerly
constructed (all methods materialized at instance creation time), not lazy
thunks. This matches Haskell's behavior and avoids infinite loops when a
method's default implementation references another method in the same class.

#### Higher-Kinded Types

`Mappable` requires higher-kinded types: `Mappable f` quantifies over a type
constructor `f`, not a type. This is a significant extension to tinct's type
system. Jones (1993) introduces constructor classes for exactly this purpose.
Phase 1 keeps `Mappable` as a built-in constraint. Phase 2 introduces
constructor classes (Jones 1993) for user extensibility — the phased
approach is deliberate.

### Interaction with Row Polymorphism (Phase 3 / D1 scope)

Type class constraints on records require **constrained row variables**.
`Equatable [name: a ...r]` means: `Equatable a` and all fields in row-rest
`r` must also satisfy `Equatable`. This requires a new constraint kind —
`EquatableRow(r)` or more generally `ClassRow(class, row_var)` — expressing
"all fields in this row satisfy the given class."

Knock-on effects of constrained row variables:

1. **TypeScheme grows.** Constraints include both `Class(name, type_var)`
   and `ClassRow(name, row_var)`. Row constraints propagate through
   let-generalization alongside type constraints.

2. **Row unification must check constraints.** When binding
   `r -> [age: Int, active: Bool]`, the unifier must verify `Equatable Int`
   and `Equatable Bool` if `r` carries an `EquatableRow` constraint. This adds a
   constraint-checking step to Remy-style four-case row unification.

3. **Open records and constraints.** `Equatable [name: Str ...]` (open
   record with unknown fields) can only satisfy `Equatable` if the open
   tail carries an `EquatableRow` constraint. Without constrained row
   variables, open records can never satisfy `Equatable`.

4. **`Any` interaction.** Is `Equatable Any` always satisfied? If yes, it's a
   blanket bypass that defeats static checking. If no, gradual typing
   breaks — code using `Any` can't use equality. This tension between
   gradual typing and constrained polymorphism needs resolution. See
   `doc/whatif/gradual-typing.md` for the broader `Any` question.

5. **Error provenance.** "field `callback` of type `Function` does not
   implement `Equatable`" — errors must trace from the `deep-eq` call through
   the record type to the specific field. Requires constraint provenance
   tracking in the solver.

6. **Higher-order propagation.** Functions like `filter` taking predicates
   that use `deep-eq` gain `Equatable` constraints that propagate through the
   call chain. Every higher-order function touching equality becomes
   constrained.

**Precedent:** PureScript handles row-level constraints via qualified row
variables (building on Gaster & Jones 1996). It is the most mature
implementation of type classes + row polymorphism. Jones (1995, §8.3)
provides the formal framework for propagating qualified constraints
through row-polymorphic inference — constraint entailment must be
decidable for the row fragment, which Gaster & Jones prove for their
*lacks* predicate system.

## What Would Change

### Type Representation (`src/types.rs`)

**Current:** `TypeScheme` contains `type_vars`, `row_vars`, and `body`. No
constraint tracking. Overloaded builtins are typed as `Any`.

**Proposed:** `TypeScheme` gains a `constraints: Vec<Constraint>` field.
Constraints are pairs of (class name, type variable name). Display format:
`Equatable a => Fn(a, a -> Bool)`.

**Impact:** Moderate. The `TypeScheme` struct grows one field. Display
formatting changes. No impact on `Type` enum itself.

### Type Inference (`src/typecheck.rs`)

**Current:** Inference produces types without constraints. Overloaded
builtins match any type via `Any`.

**Proposed:** Inference generates constraints when overloaded builtins are
used. Let-generalization propagates constraints on generalized variables into
type schemes. Instantiation checks constraints against known instances.

**Impact:** Major. The inference engine gains constraint tracking as a new
dimension alongside type inference. Every call to an overloaded builtin
generates constraints that must be solved.

### Builtin Type Signatures (`src/typecheck.rs`)

**Current:** `TypeEnv::with_builtins()` registers builtins with `Any`-typed
signatures (or concrete types for non-overloaded builtins).

**Proposed:** Overloaded builtins gain constrained type schemes:
`$= : Equatable a => a -> a -> Bool`, `$+ : Numeric a => a -> a -> a`, etc. Non-
overloaded builtins are unchanged.

**Impact:** Major. All overloaded builtins need constrained type schemes.
The builtin registration code must construct `TypeScheme` values with
constraints.

### Evaluator (`src/eval.rs`)

**Current:** Dual-dispatch builtins use runtime type inspection to select
behavior (e.g., `map` checks whether its argument is a Dict or Seq).

**Proposed (Phase 1):** Unchanged — runtime dispatch continues. Type classes
add static checking but don't change evaluation.

**Proposed (Phase 2):** Dictionary passing replaces runtime dispatch.
Overloaded builtins receive an implicit dictionary argument containing the
method implementations for the specific type. The evaluator must thread
dictionaries through function calls.

**Impact:** Minor (Phase 1), Major (Phase 2). Phase 1 is purely a type
system change. Phase 2 changes the calling convention for overloaded
functions.

### Row Polymorphism (`src/types.rs`)

**Current:** Row variables are unconstrained — they represent unknown
record tails without any requirements on field types.

**Proposed:** Row variables gain class constraints (`ClassRow(class, row_var)`)
expressing that all fields in the row must satisfy the given class.
Remy-style four-case unification adds constraint checking when binding
row variables to concrete field sets.

**Impact:** Moderate-to-Major. Constrained row variables are the main
complexity cost. They affect unification, let-generalization, and error
reporting. The constraint propagation through row variables is well-studied
(Gaster & Jones 1996, PureScript) but nontrivial to implement.

### Error Messages (`src/error.rs`)

**Current:** Type errors report concrete type mismatches at specific source
locations.

**Proposed:** Constraint violation errors: "type `Function` does not satisfy
constraint `Equatable`" with provenance showing how the constraint arose (e.g.,
"required because `deep-eq` was called on a record containing a Function
field"). Requires constraint provenance tracking.

**Impact:** Moderate. New error category for constraint violations. Error
quality depends on provenance tracking quality.

## Phased Adoption

### Phase 1: Immediate Builtins (Pre-Typeclass)

Add `deep-eq` and `shallow-eq` as builtins typed `Any -> Any -> Bool`.
These solve the practical need (comparing config dicts) without any type
system changes. Key-set equality semantics (order-independent). This phase
is independently useful and has no prerequisites beyond implementation work.

### Phase 2: Constrained Type Variables (Elm-Style)

Add `Vec<Constraint>` to `TypeScheme`. Register overloaded builtins with
constrained signatures. Implement constraint generation during inference
and constraint checking during instantiation. Fixed instance sets, no
class declarations, no dictionary passing.

### Phase 3: Full Type Classes (Haskell-Style)

Class declarations, instance declarations, superclass hierarchy, dictionary
passing. Gated on user-defined types or structural contracts needing
type-level protocol validation.

### Prerequisites

- Phase 1 (`deep-eq`, `shallow-eq`): no prerequisites
- Phase 2 (constrained type variables):
  - Gradual typing Phase 2 / `Any` split (B2) — constraints interact with
    `Unknown` semantics
  - `let-generalization` complete (constraints propagate through type schemes)
  - `builtin-type-signatures` complete (constrained builtins need signatures)
- Phase 3 (full type classes):
  - Phase 2 complete
  - Parameterized type aliases Phase 2 (B3) — provides higher-kinded type
    variables for `Mappable f` (Jones 1993)

### Trigger

Phase 1 should begin immediately — config dict comparison is a known
need and `deep-eq`/`shallow-eq` have no dependencies.

Phase 2 should begin after let-generalization and builtin-type-signatures
are complete. `Any` typing for dual-dispatch builtins already causes
false positives.

Phase 3 follows Phase 2 and enables user-extensible protocols for
equality, comparison, arithmetic, and structural contract validation.

## References

- Wadler, P. & Blott, S. (1989). "How to make ad-hoc polymorphism less ad hoc." In *POPL '89*, pp. 60-76. ACM. — The foundational type classes paper. Defines the dictionary-passing translation.
- Jones, M.P. (1993). "A system of constructor classes: overloading and implicit higher-order polymorphism." In *FPCA '93*, pp. 52-61. ACM. — Extends type classes to higher-kinded type constructors. Required for `Mappable` class over Dict/Seq.
- Jones, M.P. (1995). *Qualified types: Theory and practice.* Cambridge University Press. — Comprehensive treatment of qualified types (type classes as a special case). Covers constraint propagation through inference.
- Gaster, B.R. & Jones, M.P. (1996). "A polymorphic type system for extensible records and variants." TR NOTTCS-TR-96-3, Nottingham. — Row-level constraints for type classes. Directly applicable to tinct's row polymorphism + type classes interaction.
- Hall, C.V., Hammond, K., Peyton Jones, S.L. & Wadler, P. (1996). "Type classes in Haskell." *ACM TOPLAS*, 18(2), pp. 109-138. — Implementation-oriented treatment covering dictionary passing, default methods, and superclasses. Relevant to Phase 2 design.
- Elm Language Guide. "Constrained type variables." — Precedent for the Phase 1 approach: fixed built-in constraints without user-extensible classes.
