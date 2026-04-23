# What If: Float Dict Keys for tinct

What would it take to allow floating-point numbers as dict keys?

## Current State

tinct dict keys are either integers or strings (doc/03-data-model.md
§Heterogeneous Keys). The `Key` enum:

```rust
pub enum Key {
    Int(i64),
    String(String),
}
```

Float values cannot be used as dict keys. Attempting `[3.14: pi]`
parses the `3.14` as a float literal value, not a key.

### Why IEEE 754 Floats Are Problematic as Keys

1. **Equality.** `0.1 + 0.2 != 0.3` in IEEE 754 floating point.
   `0.1 + 0.2` produces `0.30000000000000004`. If this is used as a
   key, it would not match `0.3`.

2. **NaN.** `NaN != NaN` by IEEE 754. A float key of `NaN` could
   never be looked up — it violates the reflexivity requirement for
   equivalence relations, which dict key equality must satisfy.

3. **Hashing.** `f64` does not implement `Eq + Hash` in Rust because
   of NaN. Wrapping in `OrderedFloat` provides `Eq + Hash` but does
   not fix the precision problem.

4. **Precision.** Two computations that should produce the same float
   may not due to floating-point rounding. This makes float keys
   silently unreliable — lookup may fail depending on how the key
   value was computed.

These problems are specific to IEEE 754 binary floating point
(`f64`). A `Decimal` type with exact decimal arithmetic would not
have these issues.

### What's Missing

1. Fractional numeric keys — data keyed by prices, measurements, or
   coordinates cannot use the natural numeric representation.
2. Sound non-integer numeric equality — `f64` equality is unsound for
   key lookup; tinct has no numeric type with exact fractional equality.
3. Decimal arithmetic — the prerequisite for sound fractional keys
   does not exist in tinct yet.

## What Float Dict Keys Would Provide

1. **Fractional indexing.** Data keyed by measurement values, prices,
   coordinates, or other non-integer numeric values.

2. **Lookup tables.** Associating computed results with fractional
   inputs (interpolation tables, calibration data).

3. **Scientific data.** Key-value structures indexed by physical
   measurements where the key is a decimal quantity.

## Design

Fractional dict keys are adopted alongside a `Decimal` type, not with
IEEE 754 `f64`. Decimal arithmetic is exact for base-10 values,
eliminating the precision problems that make `f64` keys unsound.

### Decimal Key Representation

```rust
pub enum Key {
    Int(i64),
    Decimal(d128),  // exact decimal arithmetic
    String(String),
}
```

The `Decimal` variant uses IEEE 754-2008 decimal128 (`d128`)
representation, which provides 34 significant decimal digits and
exact base-10 arithmetic:

- `0.1 + 0.2 == 0.3` — exact
- No NaN — `Decimal` construction rejects NaN inputs
- `Eq + Hash` — well-defined because equality is an equivalence
  relation over the representable decimal values

### User-Facing Syntax

```lisp
# Decimal keys in dict literals
prices: [
  [call $decimal 3.99]: "budget"
  [call $decimal 9.99]: "standard"
  [call $decimal 29.99]: "premium"
]

# Decimal key lookup
[call $get $prices [call $decimal 9.99]]  # → "standard"
```

If decimal literals are added to the language
(`doc/whatif/numeric-types.md`), the syntax becomes more natural:

```lisp
# With decimal literal syntax (future)
prices: [3.99d: "budget"  9.99d: "standard"  29.99d: "premium"]
```

### Why Not f64 Keys with OrderedFloat

`OrderedFloat<f64>` (from the `ordered-float` crate) provides `Eq +
Hash` by defining a total order over `f64` (NaN == NaN, -0 < +0).
This solves the Rust trait problem but not the semantic problem:

```lisp
# With OrderedFloat keys (unsound)
table: [0.3: "found"]
[call $get $table [call $+ 0.1 0.2]]  # → null (0.30000000000000004 != 0.3)
```

The user writes a key of `0.3` and looks it up with a computed `0.3`,
but the lookup fails silently. This violates the principle of least
surprise. Goldberg (1991) documents why this is inherent to binary
floating-point — it is not a bug that can be fixed with better
wrappers.

### Interaction with Type Inference

The type system already handles heterogeneous keys via row
polymorphism — record types track individual field types keyed by
`Key`. Adding `Key::Decimal` extends the key space but does not
change the row typing mechanism. The type checker needs a new
`Type::Decimal` base type and a rule that `Decimal` keys produce
`Decimal`-typed access expressions. This parallels the existing
`Int` key / `Int` access relationship.

### Interaction with the Evaluator

Dict lookup (`$get`, field access) uses `Key` equality. With
`Key::Decimal`, lookup is exact — two decimal values are equal iff
they represent the same mathematical number. No epsilon comparison
or approximate matching is needed. This is the key advantage over
`f64`: decimal equality is sound for the use cases that motivate
fractional keys (prices, measurements in base-10 units).

### Interaction with $from-json and $to-json

JSON numbers with decimal points currently parse to `f64` values.
With a `Decimal` type:

- `$from-json` maps JSON numbers with decimal points to `Decimal`
  values when they appear in key position (dict property names in
  JSON are always strings, so this applies only to integer key
  inference for JSON arrays — a narrow interaction).
- `$to-json` serializes `Decimal` keys as JSON strings (JSON has no
  decimal type; string keys are the safe serialization).

### Normalization and Trailing Zeros

Decimal128 distinguishes `1.0` from `1.00` (different quantum
exponents). For key equality, values must be normalized so that
`1.0 == 1.00`. The `Key::Decimal` constructor should normalize
inputs (strip trailing zeros, canonicalize exponent) before storing.
This ensures that dict lookup is insensitive to how the decimal
value was constructed.

### Workaround for Current tinct

Until `Decimal` is available, string keys provide a sound alternative:

```lisp
prices: ["3.99": "budget"  "9.99": "standard"  "29.99": "premium"]
[call $get $prices "9.99"]  # → "standard"
```

String comparison is exact, so lookup is reliable. The cost is that
arithmetic on keys requires explicit conversion.

## What Would Change

### Key Enum (src/value.rs)

**Current:** `Key` has two variants: `Int(i64)` and `String(String)`.
**Proposed:** Add `Decimal(d128)` variant. Implement `Eq`, `Hash`,
and `Ord` for the new variant with normalization (trailing zero
stripping).
**Impact:** Moderate — new variant propagates to all `match` arms
on `Key` throughout the codebase (value.rs, eval.rs, builtins.rs,
types.rs).

### Type System (src/types.rs)

**Current:** `Type` has `Int`, `Float`, `Number` but no `Decimal`.
**Proposed:** Add `Type::Decimal` base type. Add subtyping rule
`Decimal <: Number`. Add unification rules for `Decimal` with
`Int` (no implicit promotion — require explicit conversion) and
`Float` (no implicit promotion).
**Impact:** Moderate — new type variant, new subtyping rules, new
unification cases.

### Type Checker (src/typecheck.rs)

**Current:** Infers `Int` or `String` for key access expressions.
**Proposed:** Infer `Decimal` for `Decimal`-keyed access. Extend
record type inference to handle `Key::Decimal` fields.
**Impact:** Minor — follows existing pattern for `Int` keys.

### Evaluator (src/eval.rs)

**Current:** Dict lookup matches on `Key::Int` and `Key::String`.
**Proposed:** Add `Key::Decimal` case to lookup. Decimal comparison
is exact (no epsilon).
**Impact:** Minor — new match arm in existing lookup logic.

### Builtins (src/builtins.rs)

**Current:** `$get`, `$keys`, `$values`, `$has`, `$merge` operate
on `Int` and `String` keys.
**Proposed:** Extend all key-operating builtins to handle `Decimal`
keys. Add `$decimal` and `$to-decimal` conversion builtins.
**Impact:** Moderate — every builtin that pattern-matches on `Key`
needs a new arm. New builtins for decimal construction.

### Parser (src/parser.rs)

**Current:** Integer literals in key position produce `Key::Int`.
Float literals in key position are not recognized as keys.
**Proposed:** If decimal literal syntax is added (`3.99d`), recognize
it in key position and produce `Key::Decimal`. Otherwise, decimal
keys are constructed via `[call $decimal ...]` expressions in key
position.
**Impact:** Minor to Moderate — depends on whether decimal literal
syntax is adopted.

### Serialization ($from-json, $to-json)

**Current:** JSON numbers map to `Int` or `Float` values.
**Proposed:** JSON numbers in key position remain strings (JSON
object keys are always strings). `Decimal` keys serialize to
string representations in JSON output.
**Impact:** Minor — no change to JSON number parsing; only key
serialization is affected.

## Phased Adoption

### Phase 1: Decimal Type

Add `Decimal` as a new `Value` variant with explicit conversion
builtins (`$decimal`, `$to-decimal`). Decimal arithmetic builtins
(`$+`, `$-`, `$*`, `$/` overloads or dedicated decimal versions).
See `doc/whatif/numeric-types.md` for full design.

### Phase 2: Decimal Keys

Extend `Key` to include `Decimal`. Implement normalized equality
and hashing. Update all key-matching builtins. This phase is
independently useful even without decimal literal syntax — users
construct decimal keys via `[call $decimal ...]`.

### Phase 3: Decimal Literal Syntax (Optional)

Add `3.99d` syntax for decimal literals, making decimal keys as
concise as integer keys: `[3.99d: "budget"]`. This phase depends
on parser changes and is optional — Phase 2 is fully functional
without it.

### Prerequisites

- `Decimal` type (`doc/whatif/numeric-types.md`) — the `d128`
  representation, conversion builtins, and arithmetic operations.
- Decision on decimal literal syntax (whether to add `d` suffix or
  require explicit `$decimal` construction).
- `rust_decimal` or `decNumber` crate evaluation for the `d128`
  implementation.

### Trigger

- When the `Decimal` type is implemented.
- When a use case requires associating data with fractional numeric
  keys (prices, measurements, scientific calibration data).
- When JSON interop with decimal-heavy data sources (financial APIs,
  measurement databases) becomes a concrete need.

## References

- IEEE 754-2019. "IEEE Standard for Floating-Point Arithmetic."
  §3.5 (binary64), §3.6 (decimal128). — Binary64 NaN != NaN
  violates equivalence relation requirements for keys; decimal128
  provides exact base-10 arithmetic.
- Goldberg, D. (1991). "What every computer scientist should know
  about floating-point arithmetic." *ACM Computing Surveys*, 23(1),
  pp. 5–48. — Definitive reference on why binary floating-point
  produces surprising equality failures. Directly explains why `f64`
  keys are unsound.
- Cowlishaw, M.F. (2003). "Decimal floating-point: algorism for
  computers." *IEEE Symposium on Computer Arithmetic*, pp. 104–111.
  — Design rationale for IEEE 754 decimal floating-point. Explains
  the quantum exponent and normalization considerations relevant to
  `Key::Decimal` equality.
- doc/03-data-model.md §Heterogeneous Keys — "Integer and string keys can
  coexist in the same dict."
- doc/11-stdlib.md §Equality P3 — WARNING about transitivity violation at
  2^53 for cross-type Int/Float comparison. Decimal keys avoid this
  class of problem entirely.
- doc/whatif/numeric-types.md — Decimal type proposal that this
  feature depends on.
