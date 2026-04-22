# What If: Type Classes for tinct

What would it take to add type classes (ad-hoc polymorphism) to tinct?

## Current State

tinct has a small, closed set of built-in types (Int, Float, Number, Str,
Bool, Null, Dict/Record, Seq, Function) with hardcoded promotion and
comparison tables:

```
# Arithmetic: hardcoded promotion table (DESIGN.md §Numeric Types)
Int + Int     → Int
Int + Float   → Float  (Int promotes to Float)
Float + Float → Float

# Comparison: hardcoded (DESIGN.md §Equality and Comparison)
$= works on Int, Float, String, Bool
$< works on Int, Float, String
Cross-type Int/Float comparison allowed
Dict, Function, Builtin return false for $=
```

Dual-dispatch builtins (`$map`, `$filter`, etc.) are typed as `Any` because
the precise type `Dict | Seq` cannot be expressed (DESIGN.md §Dual-Dispatch
Builtins).

## What Type Classes Would Provide

### Constrained Polymorphism

Instead of `$= : Any → Any → Bool`, type classes enable:

```
$= : Eq a => a → a → Bool
$< : Ord a => a → a → Bool
$+ : Num a => a → a → a
$map : Functor f => (a → b) → f a → f b
```

This rejects `[call $= [fn [] 1] [fn [] 2]]` at type-check time (Function
has no Eq instance) while accepting `[call $= 1 2]` (Int has Eq).

### Required Classes for tinct

| Class | Methods | Instances |
|-------|---------|-----------|
| `Eq` | `$=` | Int, Float, Str, Bool, Null |
| `Ord` | `$<`, `$>`, `$<=`, `$>=` | Int, Float, Str |
| `Num` | `$+`, `$-`, `$*`, `$/`, `$%`, `$neg` | Int, Float |
| `Show` | `$str` | All types |
| `Functor` | `$map` | Dict, Seq |
| `Foldable` | `$reduce`, `$length` | Dict, Seq |
| `Filterable` | `$filter` | Dict, Seq |

### Instance Laws

Type classes carry laws that instances must satisfy:

**Eq:**
- Reflexivity: `[call $= $x $x]` → true (except NaN)
- Symmetry: `[call $= $x $y]` = `[call $= $y $x]`
- Transitivity: if `$= $x $y` and `$= $y $z` then `$= $x $z`

**Ord:**
- Antisymmetry, transitivity, totality
- Consistent with Eq: `$= $x $y` iff `$<= $x $y` and `$>= $x $y`

**Num:**
- Additive identity: `$+ $x 0` = `$x`
- Additive inverse: `$+ $x [call $neg $x]` = 0
- Commutativity: `$+ $x $y` = `$+ $y $x`

**NaN exception:** Float's Eq instance violates reflexivity (`NaN ≠ NaN`).
This is universal across languages (IEEE 754) and should be documented as
an exception to the law.

## Eq for Records

### Current Behavior

`$=` returns `false` for all Dict and Seq comparisons (EQ-INCOMP rule in
DESIGN.md §Equality and Comparison). This is intentional: structural dict
equality would force all field thunks, violating lazy evaluation. Comparing
`[x: [call $/ 1 0]]` with itself would trigger the division-by-zero in an
unreferenced field.

DESIGN.md §Equality P1 flags a **future breaking change**: if `$=` gained
structural comparison, `[call $= [x: 1] [x: 1]]` would change from `false`
to `true`, breaking code that relies on dicts always being unequal.

### Proposed Approach: Separate Functions

Keep `$=` unchanged (EQ-INCOMP for dicts). Add two new builtins for
structural record comparison:

**`$deep-eq : Any → Any → Bool`** — Eager structural equality with
short-circuit. Compares field-by-field, forcing each pair lazily. Returns
`false` at the first difference without forcing remaining fields. Cost:
O(first_difference), not O(total_size). For primitive types, behaves
identically to `$=`.

**`$shallow-eq : Any → Any → Bool`** — Structural equality for already-
materialized structure. Compares keys and materialized values; unevaluated
thunks compare by pointer identity (`Rc::ptr_eq`). Cost: O(n) for n keys,
no additional forcing.

Note: `$deep-eq` is NOT equivalent to `[call $shallow-eq [call $eval $a]
[call $eval $b]]` because the composed version forces *everything* in both
values before comparing, while `$deep-eq` short-circuits at the first
difference. For large config dicts that differ in one field, the difference
is significant.

### Key-Set Equality (Order-Independent)

Structural equality uses key-set comparison, not insertion-order comparison:
`[a: 1, b: 2]` equals `[b: 2, a: 1]` under `$deep-eq`. This is more
natural for configuration data where key order is incidental. It diverges
from `IndexMap` iteration order, but config semantics should not depend on
key ordering.

Implementation: sort keys or use hash-set comparison before field-by-field
value comparison.

### No Breaking Change to `$=`

This approach sidesteps the P1 breaking change entirely. `$=` remains fast,
predictable, and primitive-only. Users who need structural comparison opt in
explicitly with `$deep-eq` or `$shallow-eq`.

When typeclasses are adopted, `$deep-eq` becomes the `Eq` method with
structural derivation. `$=` stays as the primitive equality operator.

## Default Derivation Strategy

When typeclasses are adopted, `Eq` for records uses **structural derivation
with key-set semantics**:

Two records are equal iff:
1. They have the same set of keys (regardless of insertion order)
2. All corresponding values are equal (recursive `Eq` check)

This requires `Eq` on all field types. A record containing a `Function`
value cannot derive `Eq` (functions have no meaningful equality).

For sequences: two sequences are equal iff they have the same length and
all corresponding elements are equal (recursive `Eq` check). Infinite
sequences are compared lazily — if they diverge at position n, `$deep-eq`
returns `false` at position n. Two identical infinite sequences would
diverge (non-termination), which is correct: equality on infinite
structures is undecidable.

## Design Alternatives

### Alternative 1: Haskell-Style Type Classes

Full type class system with class declarations, instance declarations,
superclasses, and dictionary passing.

```
# Class declaration (hypothetical syntax)
[typeclass Eq [a]
    eq: [Fn@Bool [a a]]]

# Instance declaration
[instance Eq Int
    eq: $=]
```

**Implementation:** Type class constraints solved during type inference.
Dictionary passing at runtime — each constrained function receives an
implicit dictionary of method implementations.

**Pros:** Most expressive. Superclass hierarchy (Ord implies Eq).
**Cons:** Heavy infrastructure (dictionary passing, instance resolution,
coherence checking). Overkill for tinct's needs.

### Alternative 2: Rust-Style Traits

Similar to type classes but with coherence (orphan rules), no superclasses
(replaced by supertraits), and monomorphization instead of dictionary
passing.

**Pros:** No runtime cost (monomorphization). Coherence prevents ambiguity.
**Cons:** Monomorphization requires compile-time resolution — conflicts with
tinct's lazy evaluation model where types are resolved at materialization.

### Alternative 3: Constrained Type Variables (Elm-Style)

No explicit class declarations. A fixed set of "comparable", "appendable",
"number" type variables with built-in semantics.

```
# Built-in constraints (not user-extensible)
comparable : types that support $=, $<  (Int, Float, Str)
number     : types that support $+, $- (Int, Float)
appendable : types that support $++    (Str, Seq, Dict)
```

**Pros:** Simple. No class declarations, no instances, no dictionary passing.
**Cons:** Not extensible — user-defined types can never participate.
Elm abandoned this approach in favor of kernel functions.

### Alternative 4: Overloading via Dispatch Table (No Type System Change)

Keep `Any` typing for overloaded builtins. Add a dispatch table that maps
`(builtin, input_type) → implementation` at runtime. No type system changes.

```rust
// Runtime dispatch (already how tinct works for $map, $filter, etc.)
fn builtin_map(args: &[Rc<Thunk>], ...) -> ... {
    match materialize(args[1])? {
        Value::Dict(_) => map_dict(...),
        Value::Seq { .. } => map_seq(...),
        _ => error("$map requires Dict or Seq"),
    }
}
```

**Pros:** Zero type system complexity. Already implemented for dual-dispatch.
**Cons:** No static type checking for overloaded operations. `Any` typing
hides real errors.

## What Would Change

### Type Representation

```rust
// New: constraints on type variables
enum Constraint {
    Class(String, String),  // (class_name, type_var)
}

struct TypeScheme {
    pub type_vars: Vec<String>,
    pub row_vars: Vec<String>,
    pub constraints: Vec<Constraint>,  // NEW
    pub body: Type,
}

// Display: "Eq a => Fn(a, a → Bool)" or "Num a, Functor f => ..."
```

### Type Inference

Constrained type variables generate constraints during inference. When a
variable `α` is used with `$=`, the constraint `Eq α` is recorded. During
let-generalization, constraints on generalized variables become part of the
type scheme. During instantiation, constraints are checked against known
instances.

```
Γ ⊢ $= ⇒ Eq α => Fn(α, α → Bool)    [instantiate with fresh α]
Γ ⊢ 1 ⇒ IntLiteral(1)
unify(α, IntLiteral(1))  →  α = IntLiteral(1)
check: Eq IntLiteral(1)?  →  yes (Int has Eq, IntLiteral <: Int)
```

**Impact: Major.** Inference engine gains constraint tracking. TypeScheme
grows. Instantiation becomes constraint-checking. Let-generalization must
propagate constraints.

### Builtin Type Signatures

All overloaded builtins gain constrained types:

```
$= : Eq a => a → a → Bool        (was: Any → Any → Bool)
$+ : Num a => a → a → a          (was: Any → Any → Any)
$map : Functor f => (a → b) → f a → f b  (was: Any)
```

**Impact: Major.** `TypeEnv::with_builtins()` must register constrained
type schemes, not bare types. All builtins need type signatures.

### Interaction with Row Polymorphism

Type class constraints on records require **constrained row variables**.
`Eq [name: a ...r]` means: `Eq a` and all fields in row-rest `r` must
also satisfy `Eq`. This requires a new constraint kind — `EqRow(r)` or
more generally `ClassRow(class, row_var)` — expressing "all fields in this
row satisfy the given class."

Knock-on effects of constrained row variables:

1. **TypeScheme grows.** Constraints include both `Class(name, type_var)`
   and `ClassRow(name, row_var)`. Row constraints propagate through
   let-generalization alongside type constraints.

2. **Row unification must check constraints.** When binding
   `r → [age: Int, active: Bool]`, the unifier must verify `Eq Int`
   and `Eq Bool` if `r` carries an `EqRow` constraint. This adds a
   constraint-checking step to Rémy-style four-case row unification.

3. **Open records and constraints.** `Eq [name: Str ...]` (open record
   with unknown fields) can only satisfy `Eq` if the open tail carries
   an `EqRow` constraint. Without constrained row variables, open records
   can never satisfy `Eq`.

4. **`Any` interaction.** Is `Eq Any` always satisfied? If yes, it's a
   blanket bypass that defeats static checking. If no, gradual typing
   breaks — code using `Any` can't use equality. This tension between
   gradual typing and constrained polymorphism needs resolution.

5. **Error provenance.** "field `callback` of type `Function` does not
   implement `Eq`" — errors must trace from the `$deep-eq` call through
   the record type to the specific field. Requires constraint provenance
   tracking in the solver.

6. **Higher-order propagation.** Functions like `$filter` taking predicates
   that use `$deep-eq` gain `Eq` constraints that propagate through the
   call chain. Every higher-order function touching equality becomes
   constrained.

These effects are intrinsic to adopting typeclasses on row-polymorphic
records — they are not a separate design decision. The constrained row
variable analysis is deferred to typeclass adoption.

**Pre-typeclass pragmatics:** `$deep-eq` and `$shallow-eq` are typed as
`Any → Any → Bool` and work via runtime dispatch. No type system changes
needed. When typeclasses are adopted, `$deep-eq` becomes the `Eq` method
and gains constrained typing.

**Precedent:** PureScript handles row-level constraints (Gaster & Jones
1996 "qualified row variables"). It is the most mature implementation of
typeclasses + row polymorphism.

## Recommendation

**Two-phase adoption: constrained type variables, then full type classes.**

### Phase 1: Constrained Type Variables (Elm-style)

Start with Alternative 3 — a fixed set of built-in constraints with no
user-extensible class declarations:

```
comparable : types that support $=, $<  (Int, Float, Str)
number     : types that support $+, $- (Int, Float)
appendable : types that support $++    (Str, Seq, Dict)
mappable   : types that support $map   (Dict, Seq)
```

This replaces `Any` typing on overloaded builtins with constrained
signatures:

```
$= : comparable → comparable → Bool   (was: Any → Any → Bool)
$+ : number → number → number         (was: Any → Any → Any)
$map : (a → b) → mappable a → mappable b  (was: Any)
```

Implementation: add `Vec<Constraint>` to `TypeScheme`. During inference,
using `$=` on a type variable `α` records `Comparable α`. During
let-generalization, constraints on generalized variables become part of
the scheme. During instantiation, constraints are checked against the
fixed instance sets. No class declarations, no dictionary passing, no
instance resolution.

**Immediate value (pre-typeclass):** Add `$deep-eq` and `$shallow-eq` as
builtins typed `Any → Any → Bool`. This solves the practical need
(comparing config dicts) without type system changes. Key-set equality
semantics (order-independent). When Phase 1 lands, `$deep-eq` gains
constrained typing.

### Phase 2: Full Type Classes (Haskell-style)

Evolve to Alternative 1 when extensibility is needed. The Phase 1
constraints become type classes with fixed instance sets — this is
forward-compatible by design. Phase 2 adds:

- Class declarations with method signatures
- Instance declarations (initially only for built-in types)
- Superclass hierarchy (Ord implies Eq)
- Dictionary passing at runtime

Phase 2 is gated on user-defined types or structural contracts needing
type-level protocol validation.

### Prerequisites

- `let-generalization` complete (constraints propagate through type schemes)
- `builtin-type-signatures` complete (constrained builtins need signatures
  to check against)
- `$deep-eq` / `$shallow-eq` builtins (immediate value, no type system
  changes)

### Trigger

Phase 1 should begin when:
- `Any` typing for dual-dispatch builtins causes a real false positive
- Dual-dispatch builtins need precise static checking
- The type system needs to distinguish "supports equality" from "any type"

Phase 2 should follow when:
- User-defined types need to participate in equality, comparison, or
  arithmetic protocols
- Structural contracts need type-level validation (e.g., "this record
  implements the Serializable protocol")

## References

- Wadler, P. & Blott, S. (1989). "How to make ad-hoc polymorphism less ad
  hoc." In *POPL '89*, pp. 60–76. ACM.
- Jones, M.P. (1995). "Qualified types: Theory and practice." *Cambridge
  University Press.*
- Gaster, B.R. & Jones, M.P. (1996). "A polymorphic type system for
  extensible records and variants." TR NOTTCS-TR-96-3, Nottingham.
- Jones, M.P. (1993). "A system of constructor classes: overloading and
  implicit higher-order polymorphism." In *FPCA '93*, pp. 52–61. ACM.
  — Extends type classes to higher-kinded type constructors (relevant to
  `Functor` class for Dict/Seq).
