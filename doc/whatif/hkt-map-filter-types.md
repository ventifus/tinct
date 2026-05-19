# What If: Precise HKT Types for map/filter/reduce/each

**State:** Superseded — implementation detail, not a language feature proposal.
**Resolved by:** `hkt-map-filter-types` sprint (DONE.md)

What would it take to replace the `Type::Unknown` signatures on `map`, `filter`, `reduce`, `each`, `each-key`, and `each-kv` with precise polymorphic types using the already-accepted HKT machinery?

## Current State

The `map`, `filter`, `reduce`, `each`, `each-key`, `each-kv` builtins are registered in `src/type_env.rs` with `Type::Unknown` for their collection parameters and return types:

```rust
// Current — type precision lost at these call sites
env.insert("map", TypeScheme::mono(Type::Function {
    params: vec![(None, Type::Unknown), (None, Type::Unknown)],
    ret: Box::new(Type::Unknown),
    variadic: false,
}));
```

Comments throughout `type_env.rs` mark these with `// TODO(unknown-elimination)`.

### What's Missing

1. `map` typed as `∀f a b. Mappable f ⇒ (a → b) → f a → f b`
2. `filter` typed as `∀f a. Filterable f ⇒ (a → Bool) → f a → f a`
3. `reduce` typed as `∀a b. (b → a → b) → b → Seq a → b`
4. `each`, `each-key`, `each-kv` typed precisely over their collection type

## Why Precise HKT Types Matter for tinct

**Type safety**: `[map [fn [let x@Int] [+ x 1]] my-dict]` — `x` should be the value type of `my-dict`, not `Unknown`. Currently no warning is produced if the function is applied to the wrong value type.

**Return type inference**: `[collect [map f xs]]` — without knowing `map` returns `Seq`, downstream operations on the result lack type info.

**LSP hover**: hovering over `map` should show `(a → b) → f a → f b` rather than `Unknown → Unknown → Unknown`.

## Design

Uses the `Kind::Operator`, `Type::App`, and `Mappable`/`Foldable`/`Traversable` classes from the already-accepted [Higher-Kinded Types](completed/hkt-monads.md) proposal (accepted 2026-05-11).

```rust
// map: ∀f a b. Mappable f ⇒ (a → b) → f a → f b
// As a TypeScheme with type vars a, b, f (Kind::Operator)
TypeScheme {
    type_vars: vec!["a", "b", "f"],
    operator_vars: vec!["f"],  // f has Kind::Operator
    constraints: vec![Constraint::Class { class: "Mappable", vars: vec!["f"] }],
    body: Type::Function {
        params: vec![
            (None, Type::Function { params: vec![(None, TypeVar("a"))], ret: Box::new(TypeVar("b")) }),
            (None, Type::App(Box::new(TypeVar("f")), Box::new(TypeVar("a")))),
        ],
        ret: Box::new(Type::App(Box::new(TypeVar("f")), Box::new(TypeVar("b")))),
    }
}
```

For `Seq`-specific operations (`reduce`):
```
reduce: ∀a b. (b → a → b) → b → Seq a → b
```

For `each` (side effects, return Null):
```
each: ∀a. (a → Unknown) → Seq a → Null
```

## What Would Change

### src/type_env.rs

**Current:** 6 builtins registered as Unknown.
**Proposed:** Each gets a proper TypeScheme with type vars, operator vars, constraints, and body.
**Impact:** Moderate — new TypeScheme construction; test suite will produce new type warnings for previously-untyped call sites.

### Corpus test updates

**Current:** Many tests pass without type warnings because map/filter return Unknown.
**Proposed:** 20-40 corpus tests will need `=== warn` sections added or updated.
**Impact:** Moderate — mechanical test updates.

## Prerequisites

- `Kind::Operator` and `Type::App` in type system (from `hkt-monads` — **Accepted 2026-05-11**)
- `Mappable`, `Foldable` typeclasses in prelude (from `hkt-monads` — **Accepted 2026-05-11**)
- CHR FD machinery for instance resolution (from `chr-unification` — **Accepted 2026-05-16**)

All prerequisites are met.

## References

- Jones, M.P. (1995). "Functional Programming with Overloading and Higher-Order Polymorphism." *Advanced Functional Programming*, LNCS 925. — `Functor` / `Mappable` typeclass
- Peyton Jones, S. et al. (2008). *Haskell 2010 Report*. §6.4 Functor/Foldable. — canonical `fmap` type
