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
> 2^53 (doc/11-stdlib.md §Equality P3).

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

## Design

Keep the user-facing type system simple — `Int`, `Float`, `Number` —
but add range annotations via the `@` system that both validate and
drive internal representation. Users declare intent; the runtime
optimizes.

### Syntax

Range constraints use the existing `@` annotation system, connecting
to structural contracts (`doc/whatif/structural-contracts.md`):

```tinct
Port:       [type Int@[min: 0  max: 65535]]
Byte:       [type Int@[min: 0  max: 255]]
Percentage: [type Float@[min: 0.0  max: 100.0]]
BigCounter: [type Int@[min: 0]]   # no upper bound -> BigInt
Price:      [type Decimal@[precision: 2]]
```

No new type constructors are needed. `Int`, `Float`, and `Number`
remain the user-facing types. Range annotations refine them.

### Semantics

**Range validation.** At TypeAssert boundaries (`@` annotations),
the runtime checks that a value falls within the declared range.
Out-of-range values produce a type error:

```lisp
port: [@Port 70000]  # Error: 70000 exceeds max 65535 for Port
```

**Arithmetic semantics.** Range annotations do not change arithmetic
behavior. `[+ port 1]` works regardless of internal representation.
The result of arithmetic on range-constrained values is an
unconstrained `Int` (or `Float`) — the range applies to the
annotated binding, not to derived values:

```tinct
Port: [type Int@[min: 0  max: 65535]]
port: [@Port 8080]
next: [+ port 1]  # next is Int, not Port — no range constraint
```

This avoids the complexity of range arithmetic (computing the output
range of `Port + Port` as `0..131070`). Range inference is an
optimization for a future phase, not a semantic requirement.

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

### Internal Representation

The runtime inspects range annotations and selects the smallest
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

The representation is transparent to the user — all arithmetic
dispatches through `NumericRepr` internally but behaves as `Int`,
`Float`, or `Decimal` at the user level.

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
   directly. `[+ port 1]` works whether `port` is internally `u16`
   or `i64`. Promotion handles cross-size arithmetic.

5. **Aligns with structural contracts.** Range annotations use the
   same `@` system as structural contracts
   (`doc/whatif/structural-contracts.md`). The `validate` builtin
   can check range constraints alongside structural shape.

## What Would Change

### Value Representation (`src/value.rs`)

**Current:** `Value::Int(i64)` and `Value::Float(f64)` — fixed-width
numeric variants.

**Proposed:** `Value::Int(NumericRepr)` and `Value::Float(NumericRepr)`
where `NumericRepr` is a tagged union of width-specific
representations. New `Value::Decimal(NumericRepr)` variant.

**Impact:** Major. Every pattern match on `Value::Int` or
`Value::Float` must handle `NumericRepr` dispatch. The `Value` enum
grows in size (from 8 bytes per numeric to a tagged union). This
affects every builtin that handles numbers.

### Builtins (`src/builtins.rs`)

**Current:** Arithmetic builtins (`+`, `-`, `*`, `/`) match on
`Value::Int(i64)` and `Value::Float(f64)` directly.

**Proposed:** Arithmetic dispatch through `NumericRepr`, handling
promotion between widths. A promotion table determines the output
representation for each operand pair.

**Impact:** Moderate. The arithmetic logic itself is unchanged —
addition is still addition. The dispatch layer adds indirection but
no new semantics. This can be implemented as a `NumericRepr::promote`
method that both operands call before the operation.

### Type Checker (`src/typecheck.rs`)

**Current:** `Int`, `Float`, and `Number` are atomic types.
Unification is straightforward.

**Proposed:** `Decimal` becomes a new atomic type. Range annotations
are parsed but not represented in the type — they remain runtime
contracts. The type checker treats `Port` as `Int` for inference
purposes.

**Impact:** Minor. One new base type (`Decimal`). No change to
unification or generalization.

### Parser (`src/parser.rs`, `src/grammar.pest`)

**Current:** Numeric literals parse as `Int` or `Float` based on
decimal point presence.

**Proposed:** (1) Parse `Decimal` literals (syntax TBD — possibly
`9.99d` suffix or explicit `[decimal 9.99]`). (2) Parse
`@[min: N  max: M]` annotations on type definitions.

**Impact:** Minor. Range annotations already use existing `@` syntax.
Decimal literal syntax is a small parser addition.

### Serialization

**Current:** `Int` serializes as JSON integer, `Float` as JSON number.

**Proposed:** `BigInt` serializes as JSON number (may exceed
recipient's parsing range). `Decimal` serializes as JSON number or
string. `NumericRepr` dispatch ensures correct serialization
regardless of internal width.

**Impact:** Minor. Serialization is a thin layer over the internal
representation.

## Phased Adoption

### Phase 1: Range Annotations (Validation Only)

Add `@[min: N  max: M]` as a contract on `Int` and `Float`:

```tinct
Port: [type Int@[min: 0  max: 65535]]
port: [@Port config.port]  # validates at runtime
```

No internal representation change — still `i64`/`f64` under the hood.
Range validation happens at TypeAssert boundaries via the `@`
annotation system.

### Phase 2: Decimal Type

Add `Decimal` as a new `Value` variant with explicit conversion:

```lisp
price: [decimal 9.99]
total: [+ price [decimal 1.00]]  # exact: 10.99
```

- `decimal` builtin: converts Int or Float to Decimal
- `to-decimal` / `from-decimal` for explicit conversion
- Decimal + Decimal → Decimal (no cross-type promotion with Float)
- Decimal + Int → Decimal (Int promotes to Decimal)

### Phase 3: Automatic Representation Sizing

The runtime inspects range annotations and selects internal
representation:

- `@[min: 0  max: 255]` → store as `u8` internally
- No range → `i64` (backwards compatible)
- Range exceeding i64 → `BigInt`

This is a performance optimization — semantics are unchanged from
Phase 1. Programs that worked before continue to work identically.

### Phase 4: BigInt

For ranges exceeding i64 (or with no upper bound), the runtime
automatically uses `BigInt`:

```tinct
BigId: [type Int@[min: 0]]  # no upper bound -> BigInt
factorial: [fn [n]
    [if [= n 0]
        1
        [* n [factorial [- n 1]]]]]
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
