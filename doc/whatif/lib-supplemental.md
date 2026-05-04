# What If: Supplemental Standard Library Modules for tinct

**State:** Proposal

What would it take to ship standard library modules beyond the core
prelude, covering extended strings, math, and bitwise encoding for
common configuration and serialization tasks?

## Current State

tinct ships a single standard library in `stdlib/prelude.llt` with
52 public functions and approximately 12 Rust-native operator wrappers,
built on top of 46 Rust builtins in `src/builtins.rs`. The core
prelude covers:

- **Collections**: `map`, `filter`, `reduce`, `sort`, `sort-by`, `zip`,
  `flatten`, `group-by`, `partition`, `slice`, `fold`, `flat-map`,
  `deep-merge`, `walk`, `transpose`, `uniq`
- **Dict utilities**: `get`, `get-or`, `get-in`, `set`, `update`,
  `remove`, `entries`, `from-entries`, `values`, `has?`, `empty?`
- **List utilities**: `first`, `last`, `nth`, `rest`, `cons`, `conj`,
  `reverse`, `reindex`
- **Aggregates**: `sum`, `product`, `min`, `max`, `count`, `any?`,
  `all?`, `contains?`
- **Math**: `abs`, `sign`, `clamp`, `quot`, `mod`, `ceil`, `trunc`
  (all pure-tinct on top of `floor` and `round` builtins)
- **String builtins** (Rust): `split`, `replace`, `upper`, `lower`,
  `trim`, `str`, `join`
- **Type predicates**: `int?`, `str?`, `float?`, `bool?`, `dict?`,
  `fn?`
- **Control flow**: `when`, `unless`, `cond`, `until`, `assert`
- **Composition**: `compose`, `->`

A motivating use case — generating YAML output — illustrates the gap.
The `yaml-quote-string` helper must decide whether a string requires
quoting. Without string predicates, it must test every special character
by splitting:

```tinct
# Current: repeated split checks for each special character
yaml-needs-quoting?: [fn [s]
  [or
    [or [= "" s]
              [or
                [> 1 [length [split s ":"]]]
                [> 1 [length [split s "#"]]]]]
    [or
      [> 1 [length [split s "{"]]]
      [> 1 [length [split s "}"]]]
    ]]]
# ... continues for 15+ special characters
```

With `str-contains?` this is terser; with the regex engine from
`doc/whatif/lib-regex.md` it collapses to one `re-match` call.

### What's Missing

1. **Extended string utilities** — no `str-contains?`, `str-starts-with?`,
   `str-ends-with?`, character-indexed slicing, or string repetition.
2. **Extended math** — no `pow`, `sqrt`, `log`, `exp`, or trigonometric
   functions; tinct's math coverage stops at floor/round/abs.
3. **Encoding** — no base64, hex, or bitwise operations; blocks binary
   data handling and HTTP configuration generation.
4. **Path manipulation** — no `basename`, `dirname`, `path-join`, or
   `path-extension`; blocks file-path-heavy configuration.

Pattern matching and regex are addressed separately in
`doc/whatif/lib-regex.md`, which depends on the bitwise primitives
(Phase 3 of this doc).

## Why Supplemental Stdlib Matters for tinct

**Serialization helpers become writable.** YAML, TOML, and Nginx
config generation require knowing whether strings need quoting.
Without string predicates, every serialization helper is a pile of
`split` checks.

**Mathematical configuration.** Network configs involve subnets and
masks (bitwise math). Audio/video configs involve sample rates and
frequency ratios (trig, log). Scientific instrument configs involve
calibration curves (exp, pow). None are expressible today.

**Encoding round-trips.** Base64 is the lingua franca for embedding
binary data in config files — TLS certificates, SSH keys, container
image digests. The bitwise primitives in Phase 3 enable `base64-encode`
and `hex-encode` as pure-tinct library functions.

**Completeness relative to peers.** Jsonnet's `std` has base64 and
math. Nickel's `std.number` has trig. CUE has `math`, `encoding/base64`,
and `path` packages. Nix has 50+ `lib.strings` functions. tinct's peer
languages treat these as non-negotiable.

## Design

Supplemental modules ship in two categories:

| Category | Implementation | Crate dependency |
|----------|----------------|-----------------|
| Pure-tinct | `stdlib/*.llt` | None |
| New Rust builtin | `src/builtins.rs` | None |
| Deferred | — | — |

Functions are delivered in three phases ordered by blocking value and
implementation cost. No new crates are introduced.

### Module Survey

Comparable configuration languages cover these domains in their
standard libraries:

| Feature | Jsonnet | Nickel | Nix | CUE | tinct |
|---------|---------|--------|-----|-----|-------|
| String search (`contains`, `starts-with`) | ✓ | ✓ | ✓ | ✓ | ✗ |
| Regex | `std.native()` | ✓ | ✓ | ✓ | see lib-regex.md |
| Math (`pow`, `sqrt`, trig) | ✓ | ✓ | partial | ✓ full | partial |
| Base64 / encoding | ✓ | ✓ | ✗ | ✓ | ✗ |
| Path utilities | ✗ | ✗ | ✓ `lib` | ✓ `path` | ✗ |
| Date/time | ✗ | ✗ | ✗ | ✓ `time` | ✗ |

### Phase 1: Extended String Utilities (stdlib/strings.llt)

String search predicates are implementable in pure-tinct using the
existing `split` builtin: `split haystack needle` returns an array
with one more element than there are occurrences, so length > 1
indicates containment.

```tinct
# stdlib/strings.llt

# str-contains? — true if needle appears anywhere in haystack
str-contains?: [fn [needle haystack]
  [> [length [split haystack needle]] 1]]

# str-starts-with? — true if haystack begins with prefix
str-starts-with?: [fn [prefix haystack]
  [= "" [get 0 [split haystack prefix]]]]

# str-ends-with? — true if haystack ends with suffix
str-ends-with?: [fn [suffix haystack]
  [= "" [last [split haystack suffix]]]]

# str-chars — split string into individual characters
str-chars: [fn [s] [split s ""]]

# str-pad-left — left-pad s to width using fill character
str-pad-left: [fn [fill width s]
  [str
    [join "" [take
      [max 0 [- width [length s]]]
      [repeat fill]]]
    s]]

# str-pad-right — right-pad s to width using fill character
str-pad-right: [fn [fill width s]
  [str s
    [join "" [take
      [max 0 [- width [length s]]]
      [repeat fill]]]]]

# str-repeat — repeat s n times
str-repeat: [fn [n s] [join "" [take n [repeat s]]]]

# str-find — character index of first occurrence of needle, or -1
str-find: [fn [needle haystack]
  [if [str-contains? needle haystack]
    [length [get 0 [split haystack needle]]]
    -1]]

# str-slice — substring by character index (half-open range)
str-slice: [fn [from to s]
  [join "" [drop from [take to [str-chars s]]]]]
```

Edge cases: `str-contains?` returns false when needle is empty
(splitting on `""` gives individual characters, never an empty-first
split). `str-starts-with?` and `str-ends-with?` behave analogously.
`str-find` returns a character offset — correct for ASCII; for
multi-byte Unicode it returns the character count of the part before
the split, not the byte offset.

**One potential Rust builtin: `str-chars`**

`str-chars`, `str-slice`, and `str-find` depend on `split s ""`
producing clean individual characters. Rust's `str::split("")` yields
`["", c₁, c₂, …, cₙ, ""]` with boundary empty strings. If tinct
passes through that raw output, a single `str-chars` Rust builtin
(`s.chars().map(|c| c.to_string()).collect()`) gives a clean
0-indexed character list. Phase 1 requires at most one new Rust
builtin.

### Phase 2: Extended Math Builtins (src/builtins.rs)

tinct's current math coverage ends at `floor`, `round`, and the
pure-tinct derivatives in prelude. Missing functions are all trivial
wrappers around Rust's `f64` methods — no new crate needed.

**New Rust builtins:**

| Function | Rust equivalent | Notes |
|----------|----------------|-------|
| `pow base exp` | `f64::powf` | Both args coerced to float |
| `sqrt x` | `f64::sqrt` | Returns `Float` |
| `log x` | `f64::ln` | Natural log |
| `log2 x` | `f64::log2` | |
| `log10 x` | `f64::log10` | |
| `exp x` | `f64::exp` | `e^x` |
| `sin x` | `f64::sin` | Radians |
| `cos x` | `f64::cos` | Radians |
| `tan x` | `f64::tan` | Radians |
| `asin x` | `f64::asin` | Returns radians |
| `acos x` | `f64::acos` | Returns radians |
| `atan x` | `f64::atan` | Returns radians |
| `atan2 y x` | `f64::atan2` | Two-argument form |

Pure-tinct additions to `stdlib/math.llt`:

```tinct
# stdlib/math.llt
pi:        3.141592653589793
e:         2.718281828459045
hypot:     [fn [x y] [sqrt [+ [pow x 2] [pow y 2]]]]
deg->rad:  [fn [d] [* d [/ pi 180.0]]]
rad->deg:  [fn [r] [* r [/ 180.0 pi]]]
log-base:  [fn [b x] [/ [log x] [log b]]]
```

`pi` and `e` are Float literals — no Rust builtin needed. All trig
functions operate in radians, consistent with every other language
surveyed. Degree conversion is a pure-tinct helper.

### Phase 3: Bitwise Primitives (src/builtins.rs, no new crate)

Rather than shipping specific encoding builtins, Phase 3 provides the
primitive bitwise operations from which users can implement base64, hex,
subnet masks, permission flags, or any other bit-level algorithm in
pure-tinct. The Rust builtins are the smallest useful layer; derived
operations live in `stdlib/encoding.llt`. Phase 3 is also a prerequisite
for `doc/whatif/lib-regex.md`, which needs `char-code` for character
class range comparisons.

**New Rust builtins:**

**`band a b`** — bitwise AND of two integers.

**`bor a b`** — bitwise OR of two integers.

**`bxor a b`** — bitwise XOR of two integers.

**`shl a n`** — left-shift `a` by `n` bits.

**`shr a n`** — logical right-shift `a` by `n` bits (zero-fills high
bits, treating the value as unsigned 64-bit).

**`char-code s`** — Unicode codepoint of the first character of string
`s` as an `Int`. For ASCII characters this equals the byte value.

**`chr n`** — single-character string whose Unicode codepoint is `n`.
Inverse of `char-code`.

**`str-bytes s`** — UTF-8 byte sequence of `s` as a 0-indexed `Dict`
of integers (0–255). Each entry is one byte of the UTF-8 encoding, not
one Unicode character. For ASCII strings, `char-code` and `str-bytes`
agree; for multi-byte Unicode they differ.

**`bytes-str bytes`** — string whose UTF-8 encoding is the given
0-indexed `Dict` of byte integers. Inverse of `str-bytes`. Errors on
invalid UTF-8.

**Derived operations in `stdlib/encoding.llt` (pure-tinct):**

```tinct
# stdlib/encoding.llt

b64-alpha: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

# encode one 6-bit index to a base64 character
b64-char: [fn [n] [get n [split b64-alpha ""]]]

# encode three bytes to four base64 characters
b64-triple: [fn [b0 b1 b2]
  [str
    [b64-char [shr b0 2]]
    [b64-char [band [bor [shl b0 4] [shr b1 4]] 63]]
    [b64-char [band [bor [shl b1 2] [shr b2 6]] 63]]
    [b64-char [band b2 63]]]]

# base64-encode — full implementation (handles padding)
base64-encode: [fn [s]
  # ... fold over str-bytes in groups of 3, handle 1-2 byte remainders
  ]

# hex-encode — one byte per two hex chars
hex-digit: [fn [n] [get n [split "0123456789abcdef" ""]]]
hex-byte:  [fn [b] [str [hex-digit [shr b 4]]
                        [hex-digit [band b 15]]]]
hex-encode: [fn [s] [join "" [map hex-byte [str-bytes s]]]]

# subnet mask application (common config use case)
mask-apply: [fn [ip mask] [band ip mask]]
```

The nine Rust builtins — five bitwise ops, two char↔code conversions,
two string↔bytes conversions — are each independently useful and compose
freely.

No new crate dependency is introduced in Phase 3.

### Deferred: Path Utilities (stdlib/path.llt)

Path manipulation is entirely implementable in pure-tinct using `split`
and `join`. Deferred because it is not blocking any known use case and
the semantics (POSIX vs Windows paths) need a decision.

```tinct
# Future stdlib/path.llt — all pure-tinct
path-parts:   [fn [p] [split p "/"]]
basename:     [fn [p] [last [path-parts p]]]
dirname:      [fn [p] [join "/" [rest [reverse [path-parts p]]]]]
extension:    [fn [p] [last [split [basename p] "."]]]
path-join:    [fn [...parts] [join "/" parts]]
```

### Deferred: Date/Time

Date/time handling requires the `chrono` crate and substantial design
work (RFC 3339 subset, duration representation, timezone handling).
Deferred until a concrete use case emerges.

## What Would Change

### Dependencies (`Cargo.toml`)

**Current:** No new crates required.

**Proposed:** No new crates across all three phases. All builtins use
only Rust standard library; derived encoding and regex operations are
pure-tinct.

**Impact:** None — zero new dependencies.

### Evaluator Builtins (`src/builtins.rs`)

**Current:** 46 Rust builtins registered in `standard_builtins()`.

**Proposed:** Phase 1 adds at most 1 Rust builtin (`str-chars`, only
if `split s ""` edge case requires it — otherwise 0). Phase 2 adds
13 math builtins (`pow`, `sqrt`, `log`, `log2`, `log10`, `exp`, `sin`,
`cos`, `tan`, `asin`, `acos`, `atan`, `atan2`; `pi` and `e` are Float
literals). Phase 3 adds 9 bitwise primitive builtins (`band`, `bor`,
`bxor`, `shl`, `shr`, `char-code`, `chr`, `str-bytes`, `bytes-str`).
Total: 13–14 new Rust builtins across three phases.

All new builtins follow the existing registration pattern in
`standard_builtins()` and the `builtin_*` naming convention.

**Impact:** Moderate — each phase is an incremental addition.

### Standard Library Files

**Current:** Single `stdlib/prelude.llt`.

**Proposed:** New files added alongside prelude:

- `stdlib/strings.llt` — `str-contains?`, `str-starts-with?`,
  `str-ends-with?`, `str-chars`, `str-pad-left`, `str-pad-right`,
  `str-repeat`, `str-find`, `str-slice` (Phase 1)
- `stdlib/math.llt` — `pi`, `e` (Float literals), `hypot`, `deg->rad`,
  `rad->deg`, `log-base` (Phase 2)
- `stdlib/encoding.llt` — `base64-encode`, `base64-decode`,
  `hex-encode`, `hex-decode`, `mask-apply` and other bit-level
  utilities (Phase 3, pure-tinct on top of bitwise primitives)
- `stdlib/regex.llt` — Thompson NFA regex engine (separate doc:
  `doc/whatif/lib-regex.md`; depends on Phase 1 + Phase 3 of this doc)
- `stdlib/path.llt` — `basename`, `dirname`, `extension`, `path-join`,
  `path-parts` (deferred)

**Loading:** Additional stdlib files are loaded by `llt eval` and
`llt format` at startup alongside `prelude.llt`. No user-facing import
syntax is needed — all stdlib functions are in scope by default.

**Impact:** Minor — the stdlib loading mechanism already handles
multiple files.

### Type Checker (`src/typecheck.rs`)

**Proposed:** Register new builtins in `TypeEnv::with_builtins()` with
precise signatures:

```
# Phase 1 (optional — only if split s "" is unreliable)
str-chars : String → Dict   -- 0-indexed list of single-char strings

# Phase 2
pow : Float → Float → Float
sqrt : Float → Float
sin : Float → Float   (and cos, tan, asin, acos, atan)
atan2 : Float → Float → Float
# pi and e are Float literals in stdlib/math.llt, not registered builtins

# Phase 3 — bitwise primitives
band : Int → Int → Int
bor : Int → Int → Int
bxor : Int → Int → Int
shl : Int → Int → Int
shr : Int → Int → Int
char-code : String → Int
chr : Int → String
str-bytes : String → Dict   -- 0-indexed list of Int (0-255)
bytes-str : Dict → String   -- inverse of str-bytes
```

**Impact:** Minor — follows the `TypeEnv::with_builtins()` pattern
established in the `type-extensions` sprint.

## Phased Adoption

### Phase 1: Extended String Utilities

**What:** `stdlib/strings.llt` with all string utilities as pure-tinct:
`str-contains?`, `str-starts-with?`, `str-ends-with?`, `str-chars`,
`str-pad-left`, `str-pad-right`, `str-repeat`, `str-find`, `str-slice`.
At most one Rust builtin (`str-chars`) if `split s ""` edge-case
behavior is unreliable; otherwise zero.

**What it enables:** String predicates for conditional config generation,
character-indexed access, string construction with padding. Also
prerequisite for the regex engine (`doc/whatif/lib-regex.md`).

**No new crates.**

### Phase 2: Extended Math Builtins

**What:** 13 new Rust builtins covering pow, sqrt, log, exp, and
trigonometric functions. `pi` and `e` are Float literals in
`stdlib/math.llt`. Pure-tinct helpers: `hypot`, `deg->rad`, `rad->deg`,
`log-base`.

**What it enables:** Mathematical configuration (network calculations,
frequency ratios, calibration curves).

**No new crates.** Pure `f64` method wrappers.

### Phase 3: Bitwise Primitives

**What:** 9 new Rust builtins — five bitwise integer operations (`band`,
`bor`, `bxor`, `shl`, `shr`) and four string↔bytes conversions
(`char-code`, `chr`, `str-bytes`, `bytes-str`). Derived operations
(`base64-encode`, `hex-encode`, `hex-decode`, subnet masking, permission
flags) in pure-tinct `stdlib/encoding.llt`.

**What it enables:** Any bit-level algorithm — base64, hex, subnet mask
application, Unix permission flags, user-defined bit-packed formats.
Also prerequisite for `doc/whatif/lib-regex.md` (needs `char-code`).

**No new crates.**

### Prerequisites

- **Phase 1:** No prerequisites. String additions are purely additive.
- **Phase 2:** No prerequisites. Math builtins are independent.
- **Phase 3:** No hard prerequisites. Can deliver after Phase 2 or
  independently. `lib-regex.md` requires Phase 1 + Phase 3 complete.

### Trigger

- **Phase 1:** When the first serialization helper (`yaml-quote-string`,
  TOML string escaping, Nginx config generation) needs `str-contains?`
  or `str-starts-with?`.
- **Phase 2:** When the first mathematical configuration file is
  attempted (subnetting, audio config, scientific instrumentation).
- **Phase 3:** When any bit-level configuration task arises — subnet
  mask calculation, Unix permission flags, base64 or hex encoding, or
  when `lib-regex.md` adoption is planned (it depends on `char-code`).

## References

- Jsonnet Standard Library. jsonnet.org/ref/stdlib.html. —
  `std.base64()`, `std.manifestYamlDoc()`, `std.parseInt()` as
  precedent for essential encoding and serialization builtins.
- Nickel Standard Library. nickel-lang.org/stdlib/. —
  `std.string` (30+ functions) and `std.number` module for trig/math
  coverage.
- Nix Built-in Functions. nix.dev/manual/nix/2.26/language/builtins.
  — `lib.strings` (50+ functions including `escapeShellArg`,
  `escapeRegex`, `versionOlder`) as reference for config-specific
  string utilities.
- CUE Standard Library. cuelang.org/docs/tour/packages/standard-library/. —
  `math` package, `encoding/base64` package as examples of a
  comprehensive modular stdlib for configuration languages.
- Josefsson, S. (2006). "The Base16, Base32, and Base64 Data
  Encodings." RFC 4648. — Normative reference for the base64 alphabet
  and padding behavior used in `stdlib/encoding.llt`.
- doc/whatif/lib-regex.md — regex engine that depends on Phase 1
  (`str-chars`) and Phase 3 (`char-code`) of this doc.
