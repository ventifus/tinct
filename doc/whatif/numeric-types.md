# What If: Width-Specific Numeric Types for tinct

What would it take to add range-constrained numeric types with
automatic internal representation sizing to tinct?

## Current State

tinct has two numeric types:

- **`Int`** — 64-bit signed integer (`i64` in Rust)
- **`Float`** — 64-bit IEEE 754 double (`f64` in Rust)

With a supertype:
- **`Number`** — supertype of `Int` and `Float` in the type hierarchy

Arithmetic promotion: `Int + Float → Float` (Int promotes to Float).
Cross-type comparison allowed with precision loss warning for integers
> 2^53 (DESIGN.md §Equality P3).

### What's Missing

1. **No range constraints.** No way to express "integer between 0 and
   255" or "port number 1-65535" as a type.

2. **No representation optimization.** All integers use 64 bits
   regardless of value range. A port number (0-65535) uses the same
   storage as a nanosecond timestamp.

3. **No arbitrary precision.** No `BigInt` for integers exceeding i64
   range (±9.2 × 10^18).

4. **No decimal.** No `Decimal` type for exact decimal arithmetic
   (financial calculations, currency).

## What Range-Constrained Numerics Would Provide

1. **Declarative constraints.** Users express what they mean:
   `@[min: 0  max: 65535]` says "this is a port number." The runtime
   validates and optimizes.

2. **Automatic representation.** The runtime chooses the smallest
   internal representation that covers the declared range:
   - `@[min: 0  max: 255]` → `u8` internally
   - `@[min: -32768  max: 32767]` → `i16` internally
   - `@[min: 0  max: 65535]` → `u16` internally
   - No range constraint → `i64` (current default)
   - Range exceeding i64 → `BigInt` automatically

3. **Decimal arithmetic.** Exact base-10 arithmetic for financial
   data, prices, and measurements where IEEE 754 precision loss is
   unacceptable.

4. **Documentation.** Range annotations serve as documentation — a
   function parameter annotated `@[min: 1  max: 100]` tells the reader
   exactly what values are valid.

## Approaches

### Approach B: Range Contracts with Automatic Sizing

Keep the user-facing type system simple — `Int`, `Float`, `Number` —
but add range annotations via the `@` system that both validate and
drive internal representation:

```lisp
Port: [type Int @[min: 0  max: 65535]]
Byte: [type Int @[min: 0  max: 255]]
Percentage: [type Float @[min: 0.0  max: 100.0]]
BigCounter: [type Int @[min: 0]]  # no upper bound → BigInt
Price: [type Decimal @[precision: 2]]
```

The runtime inspects the range annotation and selects the smallest
internal representation:

| Range | Internal representation |
|-------|----------------------|
| `min: 0, max: 255` | `u8` |
| `min: -128, max: 127` | `i8` |
| `min: 0, max: 65535` | `u16` |
| `min: -32768, max: 32767` | `i16` |
| `min: -2^31, max: 2^31-1` | `i32` |
| `min: -2^63, max: 2^63-1` | `i64` (current default) |
| Any range exceeding i64 | `BigInt` |
| No range specified | `i64` |
| `type: Decimal` | `d128` |

The representation is transparent to the user — arithmetic works
uniformly regardless of internal size. Promotion follows the existing
rules plus BigInt promotion when either operand exceeds i64 range.

```rust
// Internal representation (hidden from user)
pub enum NumericRepr {
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),      // current default
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    Big(BigInt),
    F32(f32),
    F64(f64),      // current default
    Dec(d128),
}

// User-facing Value stays clean
pub enum Value {
    Int(NumericRepr),    // was: Int(i64)
    Float(NumericRepr),  // was: Float(f64)
    Decimal(NumericRepr), // new
    // ...
}
```

**Pros:**
- Users think in constraints, not widths
- Representation optimization is automatic
- BigInt falls out naturally (no upper bound → BigInt)
- Decimal is a first-class type, not an afterthought
- No combinatorial arithmetic explosion — promotion table works on
  the abstract `Int`/`Float`/`Decimal` level, internal representation
  is handled transparently
- Range annotations serve as documentation and runtime validation
- JSON interop unchanged — JSON numbers map to the appropriate
  internal size on parse

**Cons:**
- `NumericRepr` dispatch adds complexity to arithmetic operations
  (though this is internal, not user-facing)
- Memory layout changes — `Value::Int` grows from 8 bytes (i64)
  to a tagged union
- Benchmark needed to verify the representation dispatch overhead
  is acceptable

## Recommendation

**Approach B: Range contracts with automatic internal representation
sizing.**

### Rationale

1. **Users declare intent, runtime optimizes.** `@[min: 0  max: 65535]`
   says "this is a port number." The user doesn't need to know about
   i16 vs u16 vs i32. The runtime picks the smallest representation
   that covers the range.

2. **BigInt falls out naturally.** A range with no upper bound
   (`@[min: 0]`) or a range exceeding i64 automatically uses `BigInt`.
   No separate BigInt type needed — it's just an Int with a large
   range.

3. **Decimal is orthogonal.** `Decimal` is a separate concern from
   integer sizing — it's about exact base-10 arithmetic, not range.
   Both can coexist: `[type Decimal @[min: 0  max: 999.99  precision: 2]]`
   constrains range AND uses exact decimal representation.

4. **Transparent arithmetic.** Users never interact with `NumericRepr`
   directly. `$+ $port 1` works whether `$port` is internally `u16`
   or `i64`. Promotion handles cross-size arithmetic: the result uses
   the smallest representation that can hold both operands' ranges.

5. **Aligns with structural contracts.** Range annotations use the
   same `@` system as structural contracts
   (`doc/whatif/structural-contracts.md`). The `$validate` builtin
   can check range constraints alongside structural shape.

6. **Enables float dict keys.** `doc/whatif/float-dict-keys.md`
   recommends adopting fractional keys alongside the Decimal type.
   Range-constrained Decimals provide sound fractional keys.

### Phased Adoption

#### Phase 1: Range Annotations (Validation Only)

Add `@[min: N  max: M]` as a contract on `Int` and `Float`:

```lisp
Port: [type Int @[min: 0  max: 65535]]
port: [@Port $config.port]  # validates at runtime
```

No internal representation change — still `i64`/`f64` under the hood.
Range validation happens at TypeAssert boundaries via the `@`
annotation system.

#### Phase 2: Decimal Type

Add `Decimal` as a new `Value` variant with explicit conversion:

```lisp
price: [call $decimal 9.99]
total: [call $+ $price [call $decimal 1.00]]  # exact: 10.99
```

- `$decimal` builtin: converts Int or Float to Decimal
- `$to-decimal` / `$from-decimal` for explicit conversion
- Decimal + Decimal → Decimal (no cross-type promotion with Float)
- Decimal + Int → Decimal (Int promotes to Decimal)

#### Phase 3: Automatic Representation Sizing

The runtime inspects range annotations and selects internal
representation:

- `@[min: 0  max: 255]` → store as `u8` internally
- No range → `i64` (backwards compatible)
- Range exceeding i64 → `BigInt`

This is a performance optimization — semantics are unchanged from
Phase 1. Programs that worked before continue to work identically.

#### Phase 4: BigInt

For ranges exceeding i64 (or with no upper bound), the runtime
automatically uses `BigInt`:

```lisp
BigId: [type Int @[min: 0]]  # no upper bound → BigInt
factorial: [fn [n]
    [call $if [call $= $n 0]
        1
        [call $* $n [call $factorial [call $- $n 1]]]]]
```

BigInt arithmetic is exact — no overflow, no precision loss.

### Prerequisites

- **Phase 1:** `@` annotation system mature enough for runtime
  validation (connects to `doc/whatif/structural-contracts.md`).
- **Phase 2:** Independent of Phase 1. Can be implemented in parallel.
- **Phase 3:** Phase 1 complete (range annotations define the sizing
  policy).
- **Phase 4:** Phase 3 complete (BigInt is the "unlimited range" case
  of automatic sizing).

### Trigger

Phase 1 (range annotations): adopt when:
- Structural contracts (`doc/whatif/structural-contracts.md`) are
  implemented — range constraints are a natural extension
- Users need to validate numeric ranges in config data

Phase 2 (Decimal): adopt when:
- A use case requires exact decimal arithmetic (financial data,
  currency, pricing)
- Float dict keys are needed (`doc/whatif/float-dict-keys.md`)

Phase 3 (representation sizing): adopt when:
- Memory efficiency matters (large datasets with known-range values)
- Binary serialization or FFI requires specific integer widths

Phase 4 (BigInt): adopt when:
- Cryptographic, scientific, or mathematical use cases exceed i64
- Factorial, Fibonacci, or combinatorial computations are needed

## References

- IEEE 754-2019: Binary64 (f64) and binary32 (f32) floating-point
  formats. Decimal128 (d128) format.
- JSON RFC 8259 §6: Numbers — no distinction between integer and
  floating point, no width specification.
- DESIGN.md §Numeric Types: Int (i64), Float (f64), Number supertype,
  promotion table.
- Ada range types: `type Port is range 0 .. 65535;` — declarative
  range constraints with compiler-chosen representation.
- Pascal subrange types: `type Byte = 0..255;` — range constraints
  on ordinal types.
- doc/whatif/structural-contracts.md — `$validate` schema validation,
  `@` annotation system.
