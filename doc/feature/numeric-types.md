# Constrained Numeric Types

## Overview

Tinct's numeric type system extends beyond `Int` and `Float` with three capabilities:

1. **General value constraints via `is:`.** The `is:` annotation key accepts any `Fn@Bool [Any]` predicate — ranges, divisibility, sign checks, or domain-specific invariants:
   ```tinct
   port@[type: Int  is: [between 0 65535]]
   score@[type: Float  is: [between 0.0 100.0]]
   even@[type: Int  is: [= 0 [mod _ 2]]]
   ```

2. **Named width types.** `Int8`, `UInt8`, `Int16`, `UInt16`, `Int32`, `UInt32`, `Int64`, `UInt64` as stdlib type aliases — each is `Int` constrained by an appropriate `is:` predicate. Storage is still `i64` internally; the alias documents intent and validates range.

3. **Decimal arithmetic.** Exact base-10 arithmetic for financial data, prices, and measurements where IEEE 754 precision loss is unacceptable.

4. **BigInt.** Arbitrary-precision integers for values exceeding i64 range — cryptographic keys, factorials, combinatorics.

## Design

The user-facing type system stays simple — `Int`, `Float`, `Number` — and expresses all constraints via the `is:` annotation key (see `doc/05-type-annotations.md`). The runtime validates `is:` predicates at TypeAssert boundaries. Storage is always `i64`/`f64` unless `BigInt` or `Decimal` is explicitly used.

### Syntax

Constraints use `is:` with predicate functions. The `between` stdlib function returns a 1-arg predicate — the idiomatic way to express numeric ranges:

```tinct
# Constraint via is: predicate
Port:       [type Int@[is: [between 0 65535]]]
Percentage: [type Float@[is: [between 0.0 100.0]]]
Positive:   [type Int@[is: [> _ 0]]]
Even:       [type Int@[is: [= 0 [mod _ 2]]]]
Price:      [type Decimal@[is: [>= _ 0.0]]]

# Named width types from stdlib (type aliases with is: predicates)
UInt8:  [type Int@[is: [between 0 255]]]
Int8:   [type Int@[is: [between -128 127]]]
UInt16: [type Int@[is: [between 0 65535]]]
Int16:  [type Int@[is: [between -32768 32767]]]
UInt32: [type Int@[is: [between 0 4294967295]]]
Int32:  [type Int@[is: [between -2147483648 2147483647]]]
UInt64: [type Int@[is: [>= _ 0]]]
Int64:  [type Int]   # alias for Int — documents explicit 64-bit intent
```

`between` is defined in stdlib as a predicate factory:

```tinct
between: [fn [lo hi] [fn [v] [and [>= v lo] [<= v hi]]]]
```

### Semantics

**Constraint validation.** At TypeAssert boundaries, the runtime calls the `is:` predicate with the value. `false` produces a type error:

```tinct
port: [@Port 70000]  # Error: 70000 fails is: predicate for Port
```

**Arithmetic semantics.** Constraints do not change arithmetic behavior. The result of arithmetic on constrained values is an unconstrained `Int` or `Float` — the constraint applies to the annotated binding, not to derived values:

```tinct
Port: [type Int@[is: [between 0 65535]]]
port: [@Port 8080]
next: [+ port 1]  # next is Int, not Port — no constraint propagation
```

Constraint propagation through arithmetic requires refinement type inference (a separate concern). Runtime validation at boundaries is the right default for a config language.

**Explicit storage sizes.** `Int` is always `i64` internally. Named width types (`UInt8`, `Int32`, etc.) validate range via `is:` but use the same `i64` storage. For use cases requiring compact binary storage (FFI, network protocols, binary serialization), the `repr:` annotation key provides explicit storage hints:

```tinct
port@[type: Int  is: [between 0 65535]  repr: u16]
```

**Promotion rules.** Existing promotion rules extend naturally:

| Left | Right | Result |
|------|-------|--------|
| `Int` | `Float` | `Float` (existing) |
| `Int` | `Decimal` | `Decimal` (Int promotes) |
| `Float` | `Decimal` | Error (explicit conversion required) |
| `Int` | `BigInt` | `BigInt` (Int promotes) |
| `BigInt` | `Float` | `Float` (BigInt promotes) |

`Float + Decimal` is an error because implicit conversion between IEEE 754 and exact decimal loses the precision guarantee that `Decimal` provides.

### Interaction with Type Inference

Range annotations are refinements, not distinct types. In the type system, `Port` (defined as `Int @[min: 0  max: 65535]`) unifies with `Int`. The range constraint is a runtime contract, not a type-level constraint. This keeps HM inference sound — adding subranges as distinct types requires subtype constraints that complicate unification (Mitchell, 1991).

For the type checker:
- `Int @[min: 0  max: 65535]` has type `Int`
- `Decimal @[precision: 2]` has type `Decimal`
- Unification treats them as their base types
- Range constraints are checked at runtime boundaries only

This matches tinct's existing TypeAssert semantics: `@` annotations are advisory for the type checker and enforced at runtime.

### Interaction with Lazy Evaluation

Range validation occurs at TypeAssert boundaries, which use proxy contracts for lazy record fields (doc/07-type-extensions.md §TypeAssert Runtime Validation). A range-constrained field in a lazy record is validated when accessed, not when the record is constructed. This is consistent with tinct's existing lazy validation behavior.

BigInt values participate in lazy evaluation normally — they are just a different internal representation of `Int`.

### Interaction with Row Polymorphism

Range annotations do not affect row polymorphism. A record type `[port: Int  host: String ...r]` accepts a dict with `port: [@Port 8080]` because `Port` has type `Int`. The row variable `r` is unaffected by range constraints on specific fields.

### Interaction with JSON Interop

JSON numbers (RFC 8259 §6) have no integer/float distinction and no width specification. On parsing:
- Integer-valued JSON numbers map to `Int` (i64)
- Decimal-valued JSON numbers map to `Float` (f64)
- Numbers exceeding i64 range map to `BigInt`

On serialization:
- `BigInt` values serialize as JSON numbers (may exceed recipient's parsing range — a known JSON interop issue)
- `Decimal` values serialize as JSON numbers or JSON strings depending on configuration

### Rationale

1. **`is:` is more general than `min:`/`max:`.** Any predicate works — range, divisibility, sign, string-encoded format, domain invariant. Named contracts (`PortRange: [between 0 65535]`) are reusable across annotations, match arms, and structural contracts.

2. **Named width types without runtime machinery.** `UInt8`, `Int32`, etc. are type aliases whose `is:` predicates document intent and validate range. No `NumericRepr` union, no dispatch complexity, no changes to arithmetic builtins.

3. **Decimal is orthogonal.** `Decimal` is about exact base-10 arithmetic, not range. Both can coexist: `[type Decimal@[is: [between 0.0 999.99]]]` constrains range AND uses exact decimal representation.

4. **BigInt is explicit.** Arbitrary-precision integers require a distinct `Value::BigInt` variant — they can't be a silent optimization of regular `Int`. Making BigInt explicit keeps the runtime simpler and the programmer aware of the cost.

5. **Storage hints.** `repr:` for compact binary storage is an annotation key for FFI/binary-encoding use cases. It is a storage optimization, not a semantic constraint.

## Implementation

### Phase 1: Stdlib Only (`stdlib/numeric.llt`)

No Rust changes for Phase 1. The `between`, `non-negative`, `positive` predicate factories and the named width type aliases are pure tinct definitions using `is:` — which is already a valid annotation property key — and `type` aliases. The TypeAssert runtime already calls `is:` predicates at boundaries.

### Phase 2: Value Representation (`src/value.rs`)

`Value::Decimal(d128)` is a new variant for exact decimal. `Value::Int` and `Value::Float` are unchanged — no `NumericRepr` union needed. Every exhaustive match on `Value` carries a `Decimal` arm. Arithmetic builtins carry `Int × Decimal` promotion. Serialization handles Decimal → JSON.

### Phase 3: BigInt (`src/value.rs`, `src/builtins.rs`)

`Value::BigInt(BigInt)` is a new variant. When arithmetic on `Int` overflows, promotion to `BigInt` occurs. Alternatively, explicit `[big-int n]` builtin. Dependencies: `num-bigint` crate.

### Type Checker (`src/typecheck.rs`)

**Phase 1:** No changes — `is:` predicates are runtime contracts, not type-level constraints. `Port` (defined as `Int@[is: ...]`) has type `Int`. Unification treats it as `Int`.

**Phase 2:** `Decimal` is a new atomic type. One new arm in `resolve_type_name`. No change to unification or generalization.

**Phase 3:** `BigInt` is a new atomic type, subtype of `Number`.

### Parser (`src/parser.rs`)

**Phases 1-3:** No parser changes. `[decimal 9.99]` and `[big-int n]` are builtin calls, not new syntax.

## References

**Standards:**
- IEEE 754-2019. "IEEE Standard for Floating-Point Arithmetic." — Binary64 (f64), binary32 (f32), and decimal128 (d128) formats. Governs tinct's Float and Decimal representations.
- JSON RFC 8259 §6. "Numbers." — No distinction between integer and floating point, no width specification. Constrains tinct's serialization choices for BigInt and Decimal.

**Range types and refinement types:**
- Ada Reference Manual §3.5.4. "Integer Types." — Declarative range constraints (`type Port is range 0 .. 65535`) with compiler-chosen representation. Direct precedent for tinct's approach.
- Freeman, T. & Pfenning, F. (1991). "Refinement types for ML." *PLDI*, pp. 268-277. — Refinement types that refine base types with predicates. Range constraints are a simple instance of refinement types. Tinct's approach (runtime contracts, not type-level refinements) avoids the inference complexity.
- Rondon, P., Kawaguchi, M. & Jhala, R. (2008). "Liquid types." *PLDI*, pp. 159-169. — Logically-qualified types combining HM inference with SMT-checked refinements. A more powerful system than tinct needs — cited to document the design space.

**Arbitrary precision:**
- GMP (GNU Multiple Precision Arithmetic Library). — Standard BigInt implementation. Rust's `num-bigint` crate wraps this.
- Python PEP 237. "Unifying Long Integers and Integers." — Seamless promotion from fixed-width to arbitrary precision. Precedent for tinct's transparent BigInt promotion.

**Decimal arithmetic:**
- Cowlishaw, M. (2003). "Decimal floating-point: algorism for computers." *IEEE ARITH*, pp. 104-111. — Decimal arithmetic specification underlying IEEE 754 decimal formats.

**Type system interaction:**
- Mitchell, J.C. (1991). "Type inference with simple subtypes." *Journal of Functional Programming*, 1(3), 245-285. — Decidability of type inference with subtype constraints. Explains why tinct keeps range constraints at the runtime level rather than integrating them into HM inference.

**Language-specific:**
- doc/03-data-model.md §Numeric Types. — Current Int (i64), Float (f64), Number supertype, promotion table.
- `doc/feature/structural-contracts.md` — `validate` schema validation, `@` annotation system.
- `doc/whatif/float-dict-keys.md` — Decimal type enables sound fractional dict keys.
