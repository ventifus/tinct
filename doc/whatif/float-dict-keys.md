# What If: Float Dict Keys for tinct

What would it take to allow floating-point numbers as dict keys?

## Current State

tinct dict keys are either integers or strings (DESIGN.md §Heterogeneous
Keys). The `Key` enum:

```rust
pub enum Key {
    Int(i64),
    String(String),
}
```

Float values cannot be used as dict keys. Attempting `[3.14: pi]` parses
the `3.14` as a float literal value, not a key.

### Why IEEE 754 Floats Are Problematic as Keys

1. **Equality.** `0.1 + 0.2 ≠ 0.3` in IEEE 754 floating point.
   `0.1 + 0.2` produces `0.30000000000000004`. If this is used as a
   key, it would not match `0.3`.

2. **NaN.** `NaN ≠ NaN` by IEEE 754. A float key of `NaN` could never
   be looked up.

3. **Hashing.** `f64` does not implement `Eq + Hash` in Rust because
   of NaN.

4. **Precision.** Two computations that should produce the same float
   may not due to floating-point arithmetic.

These problems are specific to IEEE 754 binary floating point (`f64`).
A `Decimal` type with exact decimal arithmetic would not have these
issues.

## What Float Dict Keys Would Provide

1. **Fractional indexing.** Data keyed by measurement values, prices,
   coordinates, or other non-integer numeric values.

2. **Lookup tables.** Associating computed results with fractional
   inputs (interpolation tables, calibration data).

3. **Scientific data.** Key-value structures indexed by physical
   measurements.

## Approaches

### Approach B: Decimal Keys (Alongside Decimal Type)

Add float keys when tinct gains a `Decimal` type
(`doc/whatif/numeric-types.md`). Decimal arithmetic is exact for
base-10 values, eliminating the precision problems that make IEEE 754
float keys unreliable:

```rust
pub enum Key {
    Int(i64),
    Decimal(d128),  // exact decimal arithmetic
    String(String),
}
```

With `Decimal`:
- `0.1 + 0.2 == 0.3` — exact decimal arithmetic
- No NaN — `Decimal` types don't have NaN
- `Eq + Hash` — well-defined equality and hashing
- Safe for currency, measurements, and any base-10 fractional value

```lisp
# With Decimal keys
prices: [
  [call $decimal 3.99]: "budget"
  [call $decimal 9.99]: "standard"
  [call $decimal 29.99]: "premium"
]
```

**Pros:**
- No precision bugs — exact base-10 arithmetic
- No NaN problems — Decimal has no NaN
- Sound equality and hashing
- Covers the real use cases (prices, measurements)

**Cons:**
- Depends on Decimal type adoption
- Decimal is slower than f64 for arithmetic
- New `Key` variant and dependency (e.g., `rust_decimal` crate)

## Recommendation

**Approach B: Adopt float dict keys alongside the Decimal type.**

### Rationale

1. **IEEE 754 floats are fundamentally unsound as keys.** The `0.1 + 0.2
   ≠ 0.3` problem is not a bug — it's inherent to binary floating-point
   representation. Wrapping in `OrderedFloat` provides `Eq + Hash` but
   doesn't fix the precision problem. Users will write `[0.3: value]`
   and then fail to look it up with a computed `0.3`.

2. **Decimal solves the problem correctly.** `Decimal` uses base-10
   representation, so `0.1 + 0.2 == 0.3` exactly. This makes Decimal
   keys sound — the value you write is the value you get.

3. **Real use cases need Decimal, not Float.** Prices, measurements,
   and calibration data — the actual reasons to want fractional keys —
   are base-10 values. `f64` is the wrong representation for them
   regardless of whether they're used as keys.

4. **If users need float keys today.** String keys work:
   `["3.14": pi]` or a dict of `[key: 3.14  value: pi]` records.

### Phased Adoption

#### Phase 1: Decimal Type

Add `Decimal` as a new `Value` variant with explicit conversion
(`$decimal`, `$to-decimal`). See `doc/whatif/numeric-types.md`.

#### Phase 2: Decimal Keys

Extend `Key` to include `Decimal`:

```rust
pub enum Key {
    Int(i64),
    Decimal(d128),
    String(String),
}
```

Parser recognizes decimal literals in key position. `$from-json`
maps JSON numbers with decimal points to `Decimal` keys when used
as dict keys.

### Prerequisites

- `Decimal` type (`doc/whatif/numeric-types.md`).
- Parser support for decimal literals in key position.

### Trigger

Adopt when:
- The `Decimal` type is implemented
- A use case requires associating data with fractional numeric keys
  (prices, measurements, scientific data)

## References

- IEEE 754-2019: Binary64 (f64) floating-point format. NaN ≠ NaN,
  `0.1 + 0.2 ≠ 0.3`.
- IEEE 754-2019: Decimal128 (d128) format. Exact base-10 arithmetic.
- DESIGN.md §Heterogeneous Keys: "Integer and string keys can coexist
  in the same dict."
- DESIGN.md §Equality P3: WARNING about transitivity violation at 2^53
  for cross-type Int/Float comparison.
