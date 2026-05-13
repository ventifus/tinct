# Type Classes

## Overview

Type classes provide constrained polymorphism for tinct's overloaded builtins. Instead of `= : Any → Any → Bool`, overloaded operators carry class constraints:

```
= : Equatable a => a → a → Bool
< : Comparable a => a → a → Bool
+ : Numeric a => a → a → a
map : Mappable f => (a → b) → f a → f b
```

This rejects `[= [fn [] 1] [fn [] 2]]` at type-check time (Function has no Equatable instance) while accepting `[= 1 2]` (Int has Equatable).

## Supersession Notes

Parts of this feature were modified by later features:

- **§Phase 2 hierarchy (Functor/Applicative/Monad/Foldable/Traversable)**: The concrete class hierarchy was specified as part of [hkt-monads.md](hkt-monads.md) (accepted 2026-05-11). The Phase 2 descriptions in this doc are abstract; the HKT doc has the authoritative design.
- **`TypeScheme` struct**: `row_vars: Vec<String>` was removed under BAS. The current struct has `type_vars`, `constraints`, `label_vars`, `body`, and `doc` fields. See [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09).
- **`Foldable` and `Filterable` as hardcoded constraint classes**: These are not primitive/hardcoded. They are stdlib-declared classes, not built into `satisfies_constraint`.

### Required Classes

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

**NaN exception:** Float's Equatable instance violates reflexivity (`NaN != NaN`). This is universal across languages (IEEE 754) and is documented as an exception to the law.

## Equatable for Records

### Current Behavior

`=` returns `false` for all Dict and Seq comparisons (EQ-INCOMP rule in doc/11-stdlib.md §Equality and Comparison). This is intentional: structural dict equality forces all field thunks, violating lazy evaluation. Comparing `[x: [/ 1 0]]` with itself triggers the division-by-zero in an unreferenced field.

### Separate Functions

> **Note:** `deep-eq` and `shallow-eq` are standalone builtins separate from the `=` operator. Structural dict equality (order-insensitive, cycle-detecting) is implemented in `src/builtins_math.rs:268-400` as part of the `=` operator for dicts/seqs under [parameterized-dict.md](parameterized-dict.md) (2026-05-09).

`=` is unchanged (EQ-INCOMP for dicts). Two builtins provide structural record comparison:

**`deep-eq : Any → Any → Bool`** — Eager structural equality with short-circuit. Compares field-by-field, forcing each pair lazily. Returns `false` at the first difference without forcing remaining fields. Cost: O(first_difference), not O(total_size). For primitive types, behaves identically to `=`.

**`shallow-eq : Any → Any → Bool`** — Structural equality for already-materialized structure. Compares keys and materialized values; unevaluated thunks compare by pointer identity (`Rc::ptr_eq`). Cost: O(n) for n keys, no additional forcing.

Note: `deep-eq` is NOT equivalent to `[shallow-eq [eval a] [eval b]]` because the composed version forces everything in both values before comparing, while `deep-eq` short-circuits at the first difference.

### Key-Set Equality (Order-Independent)

Structural equality uses key-set comparison, not insertion-order comparison: `[a: 1, b: 2]` equals `[b: 2, a: 1]` under `deep-eq`. This is more natural for configuration data where key order is incidental.

Implementation: sort keys or use hash-set comparison before field-by-field value comparison.

### No Breaking Change to `=`

`=` remains fast, predictable, and primitive-only. Users who need structural comparison opt in explicitly with `deep-eq` or `shallow-eq`.

`deep-eq` is the Equatable method with structural derivation. `=` stays as the primitive equality operator.

## Default Derivation Strategy

`Equatable` for records uses structural derivation with key-set semantics:

Two records are equal iff:
1. They have the same set of keys (regardless of insertion order)
2. All corresponding values are equal (recursive `Equatable` check)

This requires `Equatable` on all field types. A record containing a `Function` value cannot derive `Equatable` (functions have no meaningful equality).

For sequences: two sequences are equal iff they have the same length and all corresponding elements are equal (recursive `Equatable` check). Infinite sequences are compared lazily — if they diverge at position n, `deep-eq` returns `false` at position n. Two identical infinite sequences would diverge (non-termination), which is correct: equality on infinite structures is undecidable.

## Design

The implementation follows a two-phase approach: Elm-style constrained type variables (Phase 1), then full Haskell-style type classes (Phase 2).

### Phase 1: Constrained Type Variables

A fixed set of built-in constraints with no user-extensible class declarations. This follows Elm's approach — pragmatic, avoids the complexity of dictionary passing, and covers tinct's immediate needs:

```
Equatable  : types that support =, !=     (Int, Float, Str, Bool, Null)
Comparable : types that support <, >, <=  (Int, Float, Str)
Numeric    : types that support +, -, *   (Int, Float)
Appendable : types that support ++        (Str, Seq, Dict)
Showable   : types that support str       (All types)
Mappable   : types that support map       (Dict, Seq)
Foldable   : types that support fold      (Dict, Seq)
Filterable : types that support filter    (Dict, Seq)
```

Overloaded builtins carry constrained signatures:

```
= : Equatable a => a → a → Bool          (was: Any → Any → Bool)
+ : Numeric a => a → a → a               (was: Any → Any → Any)
map : Mappable f => (a → b) → f a → f b  (was: Any)
```

#### Type Representation

> **⚠ Superseded by [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09):** `row_vars` was removed. The current `TypeScheme` has fields: `type_vars`, `constraints`, `label_vars`, `body`, `doc`.

```rust
enum Constraint {
    Class(String, String),  // (class_name, type_var)
}

struct TypeScheme {
    pub type_vars: Vec<String>,
    pub row_vars: Vec<String>,
    pub constraints: Vec<Constraint>,
    pub body: Type,
}

// Display: "Equatable a => Fn(a, a → Bool)" or "Numeric a, Mappable f => ..."
```

#### Inference Semantics

Constrained type variables generate constraints during inference. When a variable `a` is used with `=`, the constraint `Equatable a` is recorded. During let-generalization, constraints on generalized variables become part of the type scheme. During instantiation, constraints are checked against the fixed instance sets:

```
G |- = => Equatable a => Fn(a, a -> Bool)    [instantiate with fresh a]
G |- 1 => IntLiteral(1)
unify(a, IntLiteral(1))  ->  a = IntLiteral(1)
check: Equatable IntLiteral(1)?  ->  yes (Int has Equatable, IntLiteral <: Int)
```

No class declarations, no dictionary passing, no instance resolution search. The instance sets are hardcoded in the type checker.

### Phase 2: Full Type Classes

Wadler & Blott (1989) type classes for extensibility. The Phase 1 constraints become type classes with fixed instance sets — this is forward-compatible by design. Phase 2 adds:

- **Class declarations** with method signatures
- **Instance declarations** (initially only for built-in types)
- **Superclass hierarchy** (Comparable implies Equatable)
- **Dictionary passing at runtime** — each class instance is compiled to a record of method implementations, passed as an implicit parameter

#### Dictionary Passing and Lazy Evaluation

Dictionary passing in a lazy language requires care. In Haskell, dictionaries are strict values (always evaluated before being passed). In tinct, dictionaries are lazy — a naively-constructed class dictionary defers method lookups. Resolution: class dictionaries are eagerly constructed (all methods materialized at instance creation time), not lazy thunks. This matches Haskell's behavior and avoids infinite loops when a method's default implementation references another method in the same class.

#### Higher-Kinded Types

`Mappable` requires higher-kinded types: `Mappable f` quantifies over a type constructor `f`, not a type. Jones (1993) introduces constructor classes for exactly this purpose. Phase 1 keeps `Mappable` as a built-in constraint. Phase 2 introduces constructor classes (Jones 1993) for user extensibility.

### Interaction with Row Polymorphism

Type class constraints on records require **constrained row variables**. `Equatable [name: a ...r]` means: `Equatable a` and all fields in row-rest `r` must also satisfy `Equatable`. This requires a new constraint kind — `EquatableRow(r)` or more generally `ClassRow(class, row_var)` — expressing "all fields in this row satisfy the given class."

Knock-on effects of constrained row variables:

1. **TypeScheme grows.** Constraints include both `Class(name, type_var)` and `ClassRow(name, row_var)`. Row constraints propagate through let-generalization alongside type constraints.

2. **Row unification must check constraints.** When binding `r -> [age: Int, active: Bool]`, the unifier must verify `Equatable Int` and `Equatable Bool` if `r` carries an `EquatableRow` constraint. This adds a constraint-checking step to Remy-style four-case row unification.

3. **Open records and constraints.** `Equatable [name: Str ...]` (open record with unknown fields) can only satisfy `Equatable` if the open tail carries an `EquatableRow` constraint. Without constrained row variables, open records can never satisfy `Equatable`.

4. **`Unknown` interaction.** The AGT approach resolves this: `Unknown` satisfies a constraint if some concretization of `Unknown` satisfies it. Since `Unknown` represents all types and some types have `Eq`, `Eq Unknown` is satisfied — but a runtime check is inserted at the point where `=` is called on the `Unknown` value.

5. **Error provenance.** "field `callback` of type `Function` does not implement `Equatable`" — errors trace from the `deep-eq` call through the record type to the specific field. Constraint provenance tracking in the solver is required.

6. **Higher-order propagation.** Functions like `filter` taking predicates that use `deep-eq` gain `Equatable` constraints that propagate through the call chain.

**Precedent:** PureScript handles row-level constraints via qualified row variables (building on Gaster & Jones 1996). Jones (1995, §8.3) provides the formal framework for propagating qualified constraints through row-polymorphic inference.

## Implementation

### Type Representation (`src/types.rs`)

`TypeScheme` contains `type_vars`, `row_vars`, `constraints`, and `body`. `constraints` is `Vec<Constraint>` — pairs of (class name, type variable name). Display format: `Equatable a => Fn(a, a -> Bool)`. The `TypeScheme` struct gained one field; the `Type` enum itself is unchanged.

### Type Inference (`src/typecheck.rs`)

Inference generates constraints when overloaded builtins are used. Let-generalization propagates constraints on generalized variables into type schemes. Instantiation checks constraints against known instances. Every call to an overloaded builtin generates constraints solved alongside type inference.

### Builtin Type Signatures (`src/typecheck.rs`)

`TypeEnv::with_builtins()` registers overloaded builtins with constrained type schemes: `$= : Equatable a => a -> a -> Bool`, `$+ : Numeric a => a -> a -> a`, etc. Non-overloaded builtins are unchanged.

### Evaluator (`src/eval.rs`)

**Phase 1:** Runtime dispatch continues unchanged — type classes add static checking but do not change evaluation.

**Phase 2:** Dictionary passing replaces runtime dispatch. Overloaded builtins receive an implicit dictionary argument containing the method implementations for the specific type. The evaluator threads dictionaries through function calls.

### Row Polymorphism (`src/types.rs`)

Row variables carry class constraints (`ClassRow(class, row_var)`) expressing that all fields in the row must satisfy the given class. Remy-style four-case unification adds constraint checking when binding row variables to concrete field sets.

### Error Messages (`src/error.rs`)

Constraint violation errors: "type `Function` does not satisfy constraint `Equatable`" with provenance showing how the constraint arose. Requires constraint provenance tracking.

## References

- Wadler, P. & Blott, S. (1989). "How to make ad-hoc polymorphism less ad hoc." In *POPL '89*, pp. 60-76. ACM. — The foundational type classes paper. Defines the dictionary-passing translation.
- Jones, M.P. (1993). "A system of constructor classes: overloading and implicit higher-order polymorphism." In *FPCA '93*, pp. 52-61. ACM. — Extends type classes to higher-kinded type constructors. Required for `Mappable` class over Dict/Seq.
- Jones, M.P. (1995). *Qualified types: Theory and practice.* Cambridge University Press. — Comprehensive treatment of qualified types (type classes as a special case). Covers constraint propagation through inference.
- Gaster, B.R. & Jones, M.P. (1996). "A polymorphic type system for extensible records and variants." TR NOTTCS-TR-96-3, Nottingham. — Row-level constraints for type classes. Directly applicable to tinct's row polymorphism + type classes interaction.
- Hall, C.V., Hammond, K., Peyton Jones, S.L. & Wadler, P. (1996). "Type classes in Haskell." *ACM TOPLAS*, 18(2), pp. 109-138. — Implementation-oriented treatment covering dictionary passing, default methods, and superclasses. Relevant to Phase 2 design.
- Elm Language Guide. "Constrained type variables." — Precedent for the Phase 1 approach: fixed built-in constraints without user-extensible classes.
