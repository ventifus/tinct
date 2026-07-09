# What If: Constrained Numeric Types for tinct

**State:** Accepted — 2026-05-05

What would it take to add predicate-constrained numeric types —
validated ranges, exact decimal arithmetic, and arbitrary-precision
integers — to tinct?

## Current State

tinct has two numeric types:

- **`Int`** — 64-bit signed integer (`i64` in Rust)
- **`Float`** — 64-bit IEEE 754 double (`f64` in Rust)

With a supertype:

- **`Number`** — supertype of `Int` and `Float` in the type hierarchy

Arithmetic promotion: `Int + Float → Float` (Int promotes to Float).
Cross-type comparison allowed with precision loss warning for integers
> 2^53 (doc/11-stdlib.md §Equality P3).

### What's Missing

1. **No value constraints.** No way to express "integer between 0 and
   255" or "port number 1-65535" as a type. `Int` accepts any i64.

2. **No named width types.** No `Int8`, `UInt16`, `Int32`, etc. for use
   cases that require specific widths (binary protocols, FFI, serialization).
   All integers are `i64` with no way to declare a narrower intent.

3. **No arbitrary precision.** No `BigInt` for integers exceeding i64
   range (±9.2 × 10^18).

4. **No decimal.** No `Decimal` type for exact decimal arithmetic
   (financial calculations, currency).

## What Constrained Numerics Would Provide

1. **General value constraints via `is:`.** The `is:` annotation key
   accepts any `Fn@Boolean [Any]` predicate — ranges, divisibility, sign
   checks, or domain-specific invariants:

   ```tinct
   port@[type: Int  is: [between 0 65535]]
   score@[type: Float  is: [between 0.0 100.0]]
   even@[type: Int  is: [= 0 [mod _ 2]]]
   ```

2. **Named width types.** `Int8`, `UInt8`, `Int16`, `UInt16`, `Int32`,
   `UInt32`, `Int64`, `UInt64` as stdlib type aliases — each is `Int`
   constrained by an appropriate `is:` predicate. Storage is still
   `i64` internally; the alias documents intent and validates range.

3. **Decimal arithmetic.** Exact base-10 arithmetic for financial
   data, prices, and measurements where IEEE 754 precision loss is
   unacceptable.

4. **BigInt.** Arbitrary-precision integers for values exceeding i64
   range — cryptographic keys, factorials, combinatorics.

## Design

Keep the user-facing type system simple — `Int`, `Float`, `Number` —
and express all constraints via the `is:` annotation key (see
`doc/05-type-annotations.md`). The runtime validates `is:` predicates
at TypeAssert boundaries. Storage is always `i64`/`f64` unless `BigInt`
or `Decimal` is explicitly used.

### Syntax

Constraints use `is:` with predicate functions. The `between` stdlib
function returns a 1-arg predicate — the idiomatic way to express
numeric ranges:

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

**Constraint validation.** At TypeAssert boundaries, the runtime calls
the `is:` predicate with the value. `false` produces a type error:

```tinct
port: [@Port 70000]  # Error: 70000 fails is: predicate for Port
```

**Arithmetic semantics.** Constraints do not change arithmetic
behavior. The result of arithmetic on constrained values is an
unconstrained `Int` or `Float` — the constraint applies to the
annotated binding, not to derived values:

```tinct
Port: [type Int@[is: [between 0 65535]]]
port: [@Port 8080]
next: [+ port 1]  # next is Int, not Port — no constraint propagation
```

Constraint propagation through arithmetic would require refinement
type inference (a separate future concern). Runtime validation at
boundaries is the right default for a config language.

**Explicit storage sizes.** `Int` is always `i64` internally.
Named width types (`UInt8`, `Int32`, etc.) validate range via `is:`
but use the same `i64` storage. For use cases requiring compact binary
storage (FFI, network protocols, binary serialization), an explicit
`repr:` annotation key provides explicit storage hints (Phase 4):

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

`Float + Decimal` is an error because implicit conversion between
IEEE 754 and exact decimal loses the precision guarantee that
`Decimal` provides.

### Interaction with Type Inference

Range annotations are refinements, not distinct types. In the type
system, `Port` (defined as `Int @[min: 0  max: 65535]`) unifies with
`Int`. The range constraint is a runtime contract, not a type-level
constraint. This keeps HM inference sound — adding subranges as
distinct types would require subtype constraints that complicate
unification (Mitchell, 1991).

For the type checker:

- `Int @[min: 0  max: 65535]` has type `Int`
- `Decimal @[precision: 2]` has type `Decimal`
- Unification treats them as their base types
- Range constraints are checked at runtime boundaries only

This matches tinct's existing TypeAssert semantics: `@` annotations
are advisory for the type checker and enforced at runtime.

### Interaction with Lazy Evaluation

Range validation occurs at TypeAssert boundaries, which use proxy
contracts for lazy record fields (doc/07-type-extensions.md §TypeAssert Runtime Validation). A
range-constrained field in a lazy record is validated when accessed,
not when the record is constructed. This is consistent with tinct's
existing lazy validation behavior.

BigInt values participate in lazy evaluation normally — they are
just a different internal representation of `Int`.

### Interaction with Row Polymorphism

Range annotations do not affect row polymorphism. A record type
`[port: Int  host: String ...r]` accepts a dict with
`port: [@Port 8080]` because `Port` has type `Int`. The row
variable `r` is unaffected by range constraints on specific fields.

### Interaction with JSON Interop

JSON numbers (RFC 8259 §6) have no integer/float distinction and no
width specification. On parsing:

- Integer-valued JSON numbers map to `Int` (i64)
- Decimal-valued JSON numbers map to `Float` (f64)
- Numbers exceeding i64 range could map to `BigInt` (Phase 4)

On serialization:

- `BigInt` values serialize as JSON numbers (may exceed recipient's
  parsing range — a known JSON interop issue)
- `Decimal` values serialize as JSON numbers or JSON strings
  depending on configuration

### Rationale

1. **`is:` is more general than `min:`/`max:`.** Any predicate works —
   range, divisibility, sign, string-encoded format, domain invariant.
   Named contracts (`PortRange: [between 0 65535]`) are reusable across
   annotations, match arms, and structural contracts. `min:`/`max:`
   would only cover numeric range, and only by having the runtime
   interpret two specific annotation keys specially.

2. **Named width types without runtime machinery.** `UInt8`, `Int32`,
   etc. are type aliases whose `is:` predicates document intent and
   validate range. No `NumericRepr` union, no dispatch complexity, no
   changes to arithmetic builtins. Phase 1 is purely stdlib.

3. **Decimal is orthogonal.** `Decimal` is about exact base-10
   arithmetic, not range. Both can coexist:
   `[type Decimal@[is: [between 0.0 999.99]]]` constrains range AND
   uses exact decimal representation.

4. **BigInt is explicit.** Arbitrary-precision integers require a
   distinct `Value::BigInt` variant — they can't be a silent
   optimization of regular `Int`. Making BigInt explicit keeps the
   runtime simpler and the programmer aware of the cost.

5. **Storage hints (Phase 4).** `repr:` for compact binary storage is
   a Phase 4 annotation key for FFI/binary-encoding use cases. It is
   a storage optimization, not a semantic constraint, and can be
   added without changing the validation model.

## What Would Change

### Phase 1: Stdlib Only (`stdlib/numeric.llt`)

**No Rust changes for Phase 1.** The `between`, `non-negative`,
`positive` predicate factories and the named width type aliases are
pure tinct definitions. They use `is:` — which is already a valid
annotation property key — and `type` aliases. The TypeAssert runtime
already calls `is:` predicates at boundaries.

**Impact:** New stdlib file only.

### Phase 2: Value Representation (`src/value.rs`)

**Current:** `Value::Int(i64)` and `Value::Float(f64)`.

**Proposed:** New `Value::Decimal(d128)` variant for exact decimal.
`Value::Int` and `Value::Float` are unchanged — no `NumericRepr`
union needed.

**Impact:** Moderate. New `Value` variant; every exhaustive match on
`Value` gains a `Decimal` arm. Arithmetic builtins gain `Int × Decimal`
promotion. Serialization gains Decimal → JSON handling.

### Phase 3: BigInt (`src/value.rs`, `src/builtins.rs`)

**Current:** `Value::Int(i64)` overflows silently or wraps.

**Proposed:** New `Value::BigInt(BigInt)` variant. Promotion: when
arithmetic on `Int` would overflow, promote to `BigInt`. Or explicit
`[big-int n]` builtin.

**Impact:** Moderate. New `Value` variant; arithmetic builtins gain
overflow detection + promotion. Dependencies: `num-bigint` crate.

### Type Checker (`src/typecheck.rs`)

**Phase 1:** No changes — `is:` predicates are already handled as
runtime contracts, not type-level constraints. `Port` (defined as
`Int@[is: ...]`) has type `Int`. Unification treats them as `Int`.

**Phase 2:** `Decimal` becomes a new atomic type. One new arm in
`resolve_type_name`. No change to unification or generalization.

**Phase 3:** `BigInt` is a new atomic type, subtype of `Number`.

### Parser (`src/parser.rs`)

**Phase 1:** No changes.

**Phase 2:** Decimal literals — explicit `[decimal 9.99]` builtin
call avoids new syntax. No parser changes.

**Phase 3:** No parser changes — `[big-int n]` is a builtin call.

## Phased Adoption

### Phase 1: `is:` Numeric Constraints + Width Type Aliases

Add `between` and related predicate factories to stdlib. Add named
width type aliases (`UInt8`, `Int16`, `UInt32`, etc.) as type
definitions using `is:` predicates. No Rust changes — purely stdlib.

```tinct
# stdlib/numeric.llt (new)
between:      [fn [lo hi] [fn [v] [and [>= v lo] [<= v hi]]]]
non-negative: [fn [v] [>= v 0]]
positive:     [fn [v] [> v 0]]

UInt8:  [type Int@[is: [between 0 255]]]
Int8:   [type Int@[is: [between -128 127]]]
UInt16: [type Int@[is: [between 0 65535]]]
Int16:  [type Int@[is: [between -32768 32767]]]
UInt32: [type Int@[is: [between 0 4294967295]]]
Int32:  [type Int@[is: [between -2147483648 2147483647]]]
UInt64: [type Int@[is: non-negative]]
Int64:  [type Int]
```

Usage:

```tinct
Port:  [type Int@[is: [between 0 65535]]]
port:  [@Port config.port]   # validates at runtime — error if out of range
score: [@UInt8 95]           # validated 0-255
```

### Phase 2: Decimal Type

Add `Decimal` as a new `Value` variant with explicit conversion:

```tinct
price: [decimal 9.99]
total: [+ price [decimal 1.00]]  # exact: 10.99
```

- `decimal` builtin: converts Int or Float to Decimal
- `to-decimal` / `from-decimal` for explicit conversion
- Decimal + Decimal → Decimal (no cross-type promotion with Float)
- Decimal + Int → Decimal (Int promotes to Decimal)

### Phase 3: BigInt

`BigInt` for integers that must exceed i64 range (cryptographic keys,
exact factorial, arbitrary-precision arithmetic):

```tinct
BigId: [type Int@[is: non-negative]]   # any non-negative — BigInt variant
factorial: [fn [n]
    [if [= n 0]
        [big-int 1]
        [* n [factorial [- n 1]]]]]
```

Requires a new `Value::BigInt` variant and promotion rules. `Int`
promotes to `BigInt` when an operation would overflow i64. Arithmetic
is exact — no overflow, no precision loss.

### Phase 4: Explicit Storage Hints (Future)

For binary serialization, FFI, and memory-mapped structures where
the exact bit width matters: `repr:` annotation key specifying `u8`,
`i32`, `u64`, etc. This is a STORAGE hint only — validation is still
via `is:`. Both can coexist:

```tinct
port@[is: [between 0 65535]  repr: u16]   # validate AND pack as u16
```

Specified in detail when binary-encoding use cases drive the design.
Phase 4 gates on Phase 3.

### Prerequisites

- **Phase 1:** `is:` annotation key specified and the match macro or
  TypeAssert boundary enforcement treats it as a runtime predicate.
  Purely stdlib — no Rust changes.
- **Phase 2:** Independent of Phase 1. Can be implemented in parallel.
- **Phase 3:** Phase 2 complete or BigInt independently motivated.
- **Phase 4:** Phase 3 complete; a concrete binary-encoding use case.

### Trigger

**Phase 1:** Numeric range validation and width type aliases are
stdlib-only — no Rust changes required. Adopt immediately.

**Phase 2:** Exact decimal arithmetic is required for financial data,
currency, and pricing. Adopt after Phase 1.

**Phase 3:** BigInt prevents silent i64 overflow in cryptographic,
mathematical, and financial computations. Adopt after Phase 2.

**Phase 4:** Compact storage for binary serialization and FFI. Adopt
after Phase 3.

## References

**Standards:**

- IEEE 754-2019. "IEEE Standard for Floating-Point Arithmetic." —
  Binary64 (f64), binary32 (f32), and decimal128 (d128) formats.
  Governs tinct's current Float and proposed Decimal representation.
- JSON RFC 8259 §6. "Numbers." — No distinction between integer and
  floating point, no width specification. Constrains tinct's
  serialization choices for BigInt and Decimal.

**Range types and refinement types:**

- Ada Reference Manual §3.5.4. "Integer Types." — Declarative range
  constraints (`type Port is range 0 .. 65535`) with compiler-chosen
  representation. Direct precedent for tinct's approach.
- Freeman, T. & Pfenning, F. (1991). "Refinement types for ML."
  *PLDI*, pp. 268-277. — Refinement types that refine base types
  with predicates. Range constraints are a simple instance of
  refinement types. Tinct's approach (runtime contracts, not
  type-level refinements) avoids the inference complexity.
- Rondon, P., Kawaguchi, M. & Jhala, R. (2008). "Liquid types."
  *PLDI*, pp. 159-169. — Logically-qualified types combining HM
  inference with SMT-checked refinements. A more powerful system
  than tinct needs — cited to document the design space.

**Arbitrary precision:**

- GMP (GNU Multiple Precision Arithmetic Library). — Standard
  BigInt implementation. Rust's `num-bigint` crate wraps this.
- Python PEP 237. "Unifying Long Integers and Integers." — Seamless
  promotion from fixed-width to arbitrary precision. Precedent for
  tinct's transparent BigInt promotion.

**Decimal arithmetic:**

- Cowlishaw, M. (2003). "Decimal floating-point: algorism for
  computers." *IEEE ARITH*, pp. 104-111. — Decimal arithmetic
  specification underlying IEEE 754 decimal formats.

**Type system interaction:**

- Mitchell, J.C. (1991). "Type inference with simple subtypes."
  *Journal of Functional Programming*, 1(3), 245-285. — Decidability
  of type inference with subtype constraints. Explains why tinct
  keeps range constraints at the runtime level rather than integrating
  them into HM inference.

**Language-specific:**

- doc/03-data-model.md §Numeric Types. — Current Int (i64), Float (f64), Number
  supertype, promotion table.
- `doc/whatif/structural-contracts.md` — `validate` schema validation,
  `@` annotation system.
- `doc/whatif/float-dict-keys.md` — Decimal type enables sound
  fractional dict keys.
