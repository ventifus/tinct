# Data Model

## Dicts Are Fundamental

The lowest-level unit is the dictionary (key-value pairs), not the list. First-class key-value pair syntax is core to the language.

A list is equivalent to a dict with integer keys:

```tinct
[a b c]  ≡  [0: a  1: b  2: c]
=== error
type errors:
  undefined variable: a at 1:2-1:3
  undefined variable: ≡ at 1:10-1:11
  undefined variable: a at 1:17-1:18
  undefined variable: b at 1:23-1:24
  undefined variable: c at 1:29-1:30

```

**Why this design:**

- **Unification** — One fundamental data structure. Functions like `map`, `filter`, `get` work uniformly on all data.
- **Flexibility** — Mixed integer and string keys naturally supported. Natural extension to keyword arguments.
- **First-class key-value pairs** — Matches the configuration language use case. Keys are names, not duplicated strings.

**Implementation:** May use different internal representations (dense vector for list-like data, HashMap for sparse/string keys) as a transparent performance optimization. Users never see the difference.

## Dict Properties

### Heterogeneous Keys

**Computed keys and the type checker:** Dict keys can be variable references (`[$k: value]`). The evaluator resolves computed keys at runtime. The type checker resolves them at compile time via literal types: if `$k` has type `StringLiteral("name")`, the field name is `"name"`. If the type is not a literal (e.g., plain `String`), the field is excluded from the Record type. See [Type Annotations](05-type-annotations.md) §Literal Types for details.

### Insertion Order

**Dicts preserve insertion order for iteration and display.** Semantically, entry order doesn't matter (letrec scoping). But iteration via `$keys`, `$values`, `$map` etc. follows the order entries appear in source. `$merge` preserves left order, appends new keys from right.

### Duplicate Keys Are Errors

**Duplicate keys in dict literals are an error.** Use `merge` for intentional overrides.

```tinct
[name: "Alice"  name: "Bob"]              # → Error: duplicate key "name"
[merge [name: "Alice"] [name: "Bob"]]     # → [name: "Bob"]  (right-biased, intentional)
=== error
error: duplicate key "name"
 --> block 2:1:17
  |
  1 | [name: "Alice"  name: "Bob"]              # → Error: duplicate key "name"
    |                 ^^^^
```

**Why:** Duplicate keys + lazy evaluation creates confusing semantics — depending on the scoping model, derived values may see different bindings of the same key. Prohibiting duplicates eliminates the ambiguity entirely and catches copy-paste errors.

## Record and Map

At the runtime level, all dicts are `Value::Dict`. At the type level, the type checker distinguishes two forms:

**Record** — a dict whose field names are statically known. Annotated as `@[name: String  age: Int]`. The type checker tracks each field and its type precisely. `get` on a Record field with a known key returns the field type directly.

**Map@[K: V]** — a homogeneous dict where all keys have type K and all values have type V. Annotated as `@[Map [K: V]]` — the compact form reads as "map from K to V". Key type K must be `Int`, `Str`, or `Int | Str`. `get` on a `Map@[K: V]` returns `V | Null` (the key may be absent). Bare `@Map` means `Map@[Any: Any]`. The explicit named form `@[Map [key: K  value: V]]` is also accepted when maximum clarity is needed.

**`Dict`** is the union of both — `@Dict` accepts either form.

```tinct
# Named Record type alias — define once, use everywhere
Config: [type [record host: String  port: Int]]
process: [fn [config@Config] ...]                              # using the alias

# Inline annotation at the parameter or type-assertion site
process: [fn [config@[host: String  port: Int]] ...]          # parameter annotation
validate: [fn [r] [@[host: String  port: Int] r]]             # type assertion

# Shorthand @[...] form also works for parameters (no collision with type/default/doc fields)
process: [fn [config@[host: String  port: Int]] ...]

# Map with unconstrained key type — iterate values, key type is Any
T1: [type [record host: String  port: Int]]
Hosts: [type [Map Any T1]]                        # a bag of T1 values, any keys
process-all: [fn [hosts@Hosts] [map do-work hosts]]

# Map with explicit key type — string-keyed lookup table
Scoreboard: [type [Map String Int]]
lookup: [fn@Int [s@Scoreboard  key@String] [get-or key 0 s]]

# Inline forms
hosts@[Map Any T1]                                # collection of T1 values (key: Any)
index@[Map String Any]                            # string-keyed, untyped values
transitions@[Map Int [Seq Int]]                   # int-keyed sequences
cache@Map                                         # bare: Map@[Any: Any]

# Dict — either form accepted
process: [fn@Null [d@Dict] ...]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 3:2:7
  |
  2 | Config: [type [record host: String  port: Int]]
    |       ^
```

The distinction is purely static. At runtime, both are the same `Value::Dict`. Most dict literals are Records unless annotated elsewhere.

## Union Types and Sum Types

`[type ...]` with multiple positional entries declares a **named union type** — a set of accepted dict shapes. The value must satisfy at least one of the declared variants:

```tinct
# Structural union — two record shapes
HttpResponse: [type [ok: Str  status: Int] [err: Str  status: Int]]

# Tag-only union — string literal types
Status: [type "ok" "err" "pending"]

# Mixed — payload and tag
Event: [type
  [click: [x: Int  y: Int]]
  [key:   [code: Str]]
  "resize"]
```

`Status` values are plain strings: `current: "ok"`. `HttpResponse` values are dicts: `res: [ok: "body" status: 200]`. No new runtime representation — the value is still a `Value::Dict` or `Value::String`.

`[match]` on a union-typed scrutinee checks exhaustiveness — the type checker verifies that all variants are handled:

```tinct
[match res
  [ok: body status: code]:  [emit body]
  [err: msg status: code]:  [error msg]]
# Type error if either arm is missing
```

**Limitation under BAS:** Single-field structural variants (`{ok: T} | {err: S}`) collapse to `Type::Top` under BAS's S-RcdTop rule. For single-field discriminated unions, use **nominal variants** (`[union [Ok a] [Err Str]]` — see [Patterns](14-patterns.md) §Nominal Variants). Multi-field structural variants (`{ok: Bool, value: T} | {err: Bool, msg: S}`) are not affected.

## Equality

**Dict equality is order-insensitive and structural.** Two dicts are equal if they have the same key set and equal values at each key, regardless of insertion order. This follows from the extensional (finite-map) semantics of Dict: a dict is a partial function from keys to values, and two functions are equal when they agree on every point in their domain.

```tinct
[= [a: 1  b: 2] [b: 2  a: 1]]   # → true  (same keys and values, different order)
[= [a: 1] [a: 2]]                 # → false (value at "a" differs)
[= [a: 1  b: 2] [a: 1]]          # → false (different key sets)
[= [] []]                          # → true  (empty dicts are equal)
=== error
type errors:
  expected record type, got Bool at 1:1-1:30
  cannot unify 1 with 2 at 2:1-2:18
  expected record type, got Bool at 3:1-3:24

```

Both Record and Map forms use the same order-insensitive comparison — the runtime representation is the same `Value::Dict`, so `=` treats them identically. Cycle detection via a visited-pair set prevents infinite loops on self-referential structures.

Functions and builtins always compare as unequal to each other (no meaningful closure equality).

## Null and Missing Keys

**Null is the empty dict `[]`.** There is no separate null type — null is simply `[]` with type `@Null` (`Type::Record(Row::Empty)`). The `null?` predicate tests for it:

```tinct
[null? []]         # → true
[null? [a: 1]]     # → false
[null? "hello"]    # → false
=== error
type errors:
  expected record type, got Bool at 1:1-1:11
  expected record type, got Bool at 2:1-2:15

```

Functions that may return nothing return `[]`. Annotate with `@Null`:

```tinct
find: [fn@[Ok String | Null] [haystack@String needle@String]
  [if [str-contains? needle haystack] [Ok haystack] []]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 6:1:5
  |
  1 | find: [fn@[Ok String | Null] [haystack@String needle@String]
    |     ^
```

**Missing keys are errors.** A key not being present in a dict is distinct from a key being present with value `[]`. Accessing a missing key errors immediately:

```tinct
[get person "name"]              # → "Alice"
[get person "occupation"]        # → Error: key "occupation" not found

# Safe alternatives
[get-or "timeout" 30 config]    # → 30 if "timeout" missing
[has? "timeout" config]          # → true/false
=== error
type errors:
  undefined variable: person at 1:6-1:12
  undefined variable: person at 2:6-2:12
  undefined variable: config at 5:9-5:15
  undefined variable: config at 6:7-6:13

```

**JSON null mapping:** `from-json` maps JSON `null` to `[]`. After conversion, `null?` on the result returns true, consistent with tinct's null-as-empty-dict model.

## Data Access — Two Modes

Data access has two distinct modes: **key-based** (look up by key) and **position-based** (look up by insertion-order index). For dense lists `[a b c]` = `[0: a 1: b 2: c]`, these coincide. They diverge for sparse or mutated dicts.

**Key-based access** — dot notation and `get` builtin:

```tinct
# Dot notation (string keys and integer dot access)
person.name                     # string key "name"
config.database.host            # chained string key access
data.0                          # integer dot access — looks up Key::Int(0)

# get builtin (key first, collection second)
[get 5 data]                    # Integer key 5
[get "name" data]               # String key "name"
[get $key data]                 # Computed key lookup ($key is a variable reference)
[get 0 config.services].host    # Dynamic key then dot chain
=== error
type errors:
  undefined variable: person at 2:1-2:7
  undefined variable: config at 3:1-3:7
  undefined variable: data at 4:1-4:5
  undefined variable: data at 7:8-7:12
  undefined variable: data at 8:13-8:17
  undefined variable: key at 9:6-9:10
  undefined variable: config at 10:8-10:14

```

**Rules:** Identifiers can start access chains directly — `foo.bar` and `$foo.bar` are both valid. `[get key data]` finds the entry whose key matches `key`, not the nth entry by position.

Use `[get key data]` for integer and dynamic key access.

**Subsequence operations** — stdlib functions:

```tinct
[slice 2 5 data]                # Entries at positions 2, 3, 4 (position-based)
[take 3 data]                   # First 3 entries
[drop 2 data]                   # All entries after the first 2
=== error
type errors:
  undefined variable: data at 1:8-1:12
  undefined variable: data at 2:9-2:13
  undefined variable: data at 3:9-3:13

```

Use `slice`, `take`, and `drop` for subsequences.

**Position-based access** — stdlib functions:

```tinct
[nth 0 data]                    # First entry (position 0)
[nth -1 data]                   # Last entry (negative = from end)
[last data]                     # Last entry (alias)
[slice 2 5 data]                # Entries at positions 2, 3, 4
=== error
type errors:
  undefined variable: data at 1:6-1:10
  undefined variable: data at 2:6-2:10
  undefined variable: data at 3:7-3:11
  undefined variable: data at 4:8-4:12

```

**Why the split:** Position-based access on a dict that has been mutated over time has less-than-useful ordering. Making it a function call (not syntax) signals that it's the unusual operation. For the common case of dense lists, `[get 0 data]` (key 0) and `[nth 0 data]` (position 0) return the same thing — you never need `nth` unless you specifically want insertion-order semantics on sparse data.

## List vs Dict Operations — Renumbering Rule

**List operations require integer keys and always produce dense `[0..n]`.** Error on string keys. Dict operations preserve keys. Universal operations work on both and preserve keys.

```tinct
# List operations — integer keys only, always renumber
[first [alice bob carol]]               # → alice
[rest [alice bob carol]]                # → [bob carol] = [0: bob  1: carol]
[cons z [a b c]]                        # → [z a b c] = [0: z  1: a  2: b  3: c]
[conj [a b c] d]                        # → [a b c d] = [0: a  1: b  2: c  3: d]
[concat [a b] [c d]]                    # → [a b c d] = [0: a  1: b  2: c  3: d]
[reverse [a b c]]                       # → [c b a] = [0: c  1: b  2: a]
[sort [cherry apple banana]]            # → [apple banana cherry] — sorts by value, discards original keys
[reindex [0: a  5: b  10: c]]           # → [a b c] = [0: a  1: b  2: c]
=== error
type errors:
  undefined variable: alice at 2:9-2:14
  undefined variable: alice at 3:8-3:13
  undefined variable: z at 4:7-4:8
  undefined variable: a at 4:10-4:11
  undefined variable: a at 5:8-5:9
  undefined variable: d at 5:15-5:16
  undefined variable: a at 6:10-6:11
  undefined variable: c at 6:16-6:17
  undefined variable: a at 7:11-7:12
  undefined variable: cherry at 8:8-8:14
  undefined variable: a at 9:14-9:15
  undefined variable: b at 9:20-9:21
  undefined variable: c at 9:27-9:28

```

**Why this split:**

- No ambiguity about which operations renumber — it's determined by the category, not the data
- List operations always give you clean, predictable lists
- Dict operations never silently destroy your key structure
- `filter` returns a Seq of matching values (since inclusion requires predicate evaluation, keys are not preserved) — use `collect` to get a dict back
- The type system enforces the boundary: list operations require `[a]` (integer-keyed)

```tinct
# filter returns a Seq of matching values (dual-dispatch)
data: [alice bob carol dave]
[filter [fn [x] [not [= x bob]]] data]
# → Seq(alice, carol, dave)    use collect for a dict

# Pipe through collect for a clean list
[collect [filter [fn [x] [not [= x bob]]] data]]
# → [0: alice  1: carol  2: dave]

# filter on string-keyed dicts also returns Seq of values
[collect [filter [fn [v] [> v 0]] [x: 1  y: -2  z: 3]]]
# → [0: 1  1: 3]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 12:2:5
  |
  2 | data: [alice bob carol dave]
    |     ^
```

**`conj` on sparse data:** `conj` delegates to `append`, which uses the maximum existing integer key + 1 as the new key (or 0 if no integer keys exist). This avoids key collisions even on sparse data:

```tinct
# Dense list — conj works as expected
[conj [a b c] d]                        # → [0: a  1: b  2: c  3: d]

# Sparse data — no collision, key 11 is used (max 10 + 1)
sparse: [0: a  5: b  10: c]
[conj sparse d]                         # → [0: a  5: b  10: c  11: d]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 13:5:7
  |
  5 | sparse: [0: a  5: b  10: c]
    |       ^
```

## Value Types — Overview

Tinct has a fixed set of runtime value types. The following table lists all types in the `Value` enum with a brief description and where to find details. In type annotations, `String` is the user-facing name for the internal type `Str`.

| Type | Description | Details |
|------|-------------|---------|
| `Int` | 64-bit signed integer (`i64`) | §Numeric Types below |
| `Float` | 64-bit IEEE 754 float (`f64`) | §Numeric Types below |
| `Decimal` | Arbitrary-precision decimal (`rust_decimal`) | §Numeric Types below |
| `BigInt` | Arbitrary-precision integer (`num_bigint`) | §Numeric Types below |
| `String` | UTF-8 string | Used throughout; type name `Str` internally |
| `Bool` | Boolean (`true` / `false`) | Literal values; no dedicated section |
| `Dict` | Key-value dictionary (the fundamental structure) | §Dicts Are Fundamental above |
| `Seq` | Lazy sequence (coinductive stream) | §Lazy Sequences below |
| `Function` | User-defined function (closure) | [Functions](04-functions.md) |
| `Builtin` | Rust-native function | [Builtins Reference](11a-builtins.md) |
| `Variant` | Tagged value (`tag` + optional `payload`) | [Builtins Reference](11a-builtins.md) §ADTs |
| `Handle` | Open I/O resource (file, socket, etc.) | §Handles below |
| `Uri` | Parsed URI (scheme + uri string) | §URI Values below |
| `Timestamp` | Nanosecond-precision instant | [Builtins Reference](11a-builtins.md) §Datetime |
| `Duration` | Time span (nanosecond precision) | [Builtins Reference](11a-builtins.md) §Datetime |
| `Bytes` | Raw binary data | [Builtins Reference](11a-builtins.md) §I/O |
| `Proxy` | Virtual field dispatch wrapper | [Builtins Reference](11a-builtins.md) §Proxy |
| `Overlay` | Lazy merge of two dicts | [Evaluation](08-evaluation.md) §Lazy Merge |
| `QuicSession` | QUIC session handle | [Builtins Reference](11a-builtins.md) §Network |
| `Http2Session` | HTTP/2 session handle | [Builtins Reference](11a-builtins.md) §Network |
| `Http3Session` | HTTP/3 session handle | [Builtins Reference](11a-builtins.md) §Network |

## Numeric Types — `Int`, `Float`, `Number`

**Two concrete types: `Int(i64)` and `Float(f64)`.** `Number` is the supertype that accepts either. Integer literals carry their value: `42` has type `IntLiteral(42)`, which is a subtype of `Int`. Float literals do not have a literal type variant because floats cannot be dict keys.

```tinct
port: 8080                      # Int — no decimal point
pi: 3.14                        # Float — has decimal point
x@Int                           # must be an integer
y@Float                         # must be a float
z@Number                        # accepts either
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 14:1:5
  |
  1 | port: 8080                      # Int — no decimal point
    |     ^
```

**Arithmetic auto-promotes.** The compiler handles promotion with a fixed table — no typeclasses needed:

| Left | Op | Right | Result |
|------|-----|-------|--------|
| Int | `+`, `-`, `*` | Int | Int |
| Int | any | Float | Float |
| Float | any | Int | Float |
| Float | any | Float | Float |
| any | `/` | any | Float (always) |
| Int | `quot`, `mod` | Int | Int |

```tinct
[+ 5 3]                         # → 8 (Int)
[+ 5 3.0]                       # → 8.0 (Float)
[/ 10 3]                        # → 3.333... (Float — / always returns Float)
[quot 10 3]                     # → 3 (Int — truncated integer division, prelude function using trunc)
[mod 10 3]                      # → 1 (Int — remainder)
=== error
type errors:
  expected record type, got Int at 1:1-1:8
  expected record type, got Float at 2:1-2:10
  expected record type, got Float at 3:1-3:9
  expected record type, got Int at 4:1-4:12

```

**Precision-safe promotion.** Implicit Int→Float promotion in mixed-type arithmetic operations errors when the integer's magnitude exceeds `2^53`, the largest integer exactly representable in an `f64` mantissa. This prevents silent precision loss:

```tinct
[+ 9007199254740992 1.0]        # → 9007199254740993.0 (2^53, exact)
[+ 9007199254740993 1.0]        # → Error: Int→Float promotion would lose precision
=== error
type errors:
  expected record type, got Float at 1:1-1:25

```

**Explicit float conversion** — `[float n]` builtin performs unconditional Int→Float conversion without precision checks, allowing controlled precision loss when desired. For Float inputs, `float` is a no-op.

```tinct
[float 9007199254740993]        # → 9007199254740992.0 (loss of precision, intentional)
[float 3.14]                    # → 3.14 (no-op on Float inputs)
=== error
type errors:
  undefined variable: float at 1:2-1:7
  undefined variable: float at 2:2-2:7

```

**Integer arithmetic uses checked semantics.** `Int` operations (`+`, `-`, `*`) use Rust's `checked_add`/`checked_sub`/`checked_mul`, so overflow returns an error rather than wrapping or panicking. This prevents silent data corruption on large values. Width-specific types like `Int32` could enforce narrower range constraints via the contracts system.

**Dict key integration:** `Int` values are directly usable as dict keys. `Float` values cannot be used as keys — floating-point equality semantics make them unreliable as hash keys.

**Width-specific types** (`Int32`, `Int64`, `Int128`, etc.) are range constraints expressed through the contracts system, not new runtime representations. `Decimal` is a first-class runtime type created via `[decimal "3.14159"]`, backed by `rust_decimal::Decimal` (96-bit software decimal). `BigInt` is an arbitrary-precision integer created via `[big-int "12345678901234567890"]`, backed by `num_bigint::BigInt`.

## Lazy Sequences

**Lazy sequences (`Value::Seq`) are a runtime-only value type** representing infinite or demand-driven data (from `$range`, `$repeat`, `$cycle`, `$iterate`, etc.). They exist alongside `Dict`, `Int`, `Float`, `String`, `Bool`, `Function`, `Handle`, `HttpConn`, `Uri`, `Decimal`, `BigInt`, `Variant`, `Timestamp`, `Duration`, and `Bytes` in the value representation. Sequences have no literal syntax — they are produced by builtin functions and consumed by sequence operations like `$map`, `$filter`, `$take`, `$collect`.

Sequences are dual-dispatch targets: `$map` on a Seq returns a lazy Seq, `$filter` on a Seq returns a lazy Seq. Use `$collect` to materialize a Seq to a dense dict. Attempting operations that require full materialization (like `$sort` or `$length`) on an infinite Seq will error. See doc/08-evaluation.md §Lazy Sequences for implementation details and laziness semantics.

## Handles

**Handles (`Value::Handle`) are runtime-only values representing open I/O resources** — file descriptors, network streams, and other OS-level channels. A Handle is an unforgeable reference in the capability sense (Dennis & Van Horn 1966): holding it is sufficient authority to perform I/O; no separate capability argument is required at use time.

### Capability Row

Every Handle carries a **capability row** — a `HashMap<String, Value>` mapping capability names to associated data. The row is immutable after construction; each operation that adds a capability produces a new Handle wrapping the old one. The capability row determines which builtins are callable on the Handle:

| Capability | Value | Granted by | Required by |
|-----------|-------|------------|-------------|
| `Readable` | empty dict (`[]`) | `open … Readable`, `connect`, `tls-layer` | `slurp`, `lines`, `read` |
| `Writable` | empty dict (`[]`) | `open … Writable`, `connect`, `tls-layer` | `write`, stream writes |
| `Binary` | empty dict (`[]`) | `connect`, `tls-layer` | `slurp` (binary mode) |
| `Text` | empty dict (`[]`) | `open … Readable` on text files | `lines`, `slurp` (text mode) |
| `Stream` | empty dict (`[]`) | `connect … Tcp`, `tls-layer` | streaming reads/writes |
| `Datagram` | empty dict (`[]`) | `connect … Udp` | datagram I/O |
| `Seekable` | empty dict (`[]`) | regular file `open` | `seek` |
| `Tls` | `Value::Dict` (TLS metadata) | `tls-layer` | `tls-peer-cert` |

Boolean capabilities (Readable, Writable, Binary, Text, Stream, Datagram, Seekable) store an empty dict (`[]`) as their associated data — the presence of the key is the entire capability. There is no `Value::Null` variant; the `null?` predicate tests for an empty dict. Protocol capabilities like `Tls` store structured data: the `Tls` value is a dict containing the leaf certificate metadata and negotiated ALPN protocol string.

**Reading capability data:** Use `cap-data h name` to read the associated `Value` for a capability, and `has-cap? h name` to test whether a capability is present without extracting data.

### Network Handles

`connect` dispatches on the Transport variant to determine the address format and capability routing. Port is absent for transports that have no port concept:

```text
# Stream transports (NetCap)
connect cap Tcp  host port       → Handle{ Binary Readable Writable Stream }
connect cap Udp  host port       → Handle{ Binary Readable Writable Datagram }
connect cap Icmp host            → Handle{ Binary Readable Writable Datagram }

# Local transports (DirCap)
connect cap UnixStream    path   → Handle{ Binary Readable Writable Stream }
connect cap UnixDatagram  path   → Handle{ Binary Readable Writable Datagram }
connect cap NamedPipe     path   → Handle{ Binary Readable Writable }

# TLS Layer (Handle → Handle upgrade)
tls-layer sni opts handle        → Handle{ …existing… Tls→{cert…} }
```

Capability routing: `Tcp`/`Udp`/`Icmp` require a `NetCap` (allowlist checked before syscall); `UnixStream`/`UnixDatagram`/`NamedPipe` require a `DirCap` (cap_std path-based access). User-defined Connectors handle their own capability checks.

The `Tls` capability value is a dict with the same fields as the `PeerCert` type returned by `tls-peer-cert` (see [Builtins](11a-builtins.md) §Network).

### Layers — Handle→Handle Protocol Upgrades

A **Layer** is any function that takes a Handle and returns a Handle with augmented capabilities (`Handle@R → Handle@[R ∪ NewCaps]`). There is no Layer typeclass — the composition is structural. Any pure-tinct function with the right signature is a Layer.

Standard library Layers: `tls-layer` (TLS/STARTTLS upgrade, Rust builtin), `socks5-layer` (SOCKS5 tunnel, pure tinct in `protocols/socks5.llt`), `http-connect-layer` (HTTP CONNECT tunnel, pure tinct in `net.llt`).

Layers compose left-to-right with Connectors:

```tinct
[tcp:  [connect %nc Tcp "proxy.corp" 1080]]
[tun:  [socks5-layer tcp "api.internal" 443]]
[tls:  [tls-layer "api.internal" tls-opts tun]]
=== error
type errors:
  undefined variable: %nc at 1:17-1:20
  undefined variable: socks5-layer at 2:9-2:21
  undefined variable: tun at 3:19-3:22
  undefined variable: tls-opts at 3:38-3:46

```

The original Handle is consumed; subsequent operations on it produce a runtime error. The new Handle wraps the protocol-upgraded connection.

### Sessions — Multiplexed Connections

A **Session** is a multiplexed connection: one physical channel carrying multiple independent logical streams. Sessions are opened from Handles or Connectors; stream Handles are opened from Sessions.

Three Session types exist as runtime-only opaque values:

**`Value::QuicSession`** — QUIC (RFC 9000), implemented via `quinn`. QUIC integrates transport, TLS, and reliable delivery at the UDP level. `quinn` owns the UDP socket internally (managing path migration, congestion control, ACKs):

```tinct
[quic:   [quic-session %nc "api.example.com" 443 quic-opts]]
[stream: [quic-open-stream quic]]    # → Handle{ Binary Readable Writable Stream }
=== error
type errors:
  undefined variable: %nc at 1:24-1:27
  undefined variable: quic-opts at 1:50-1:59
  undefined variable: quic at 2:28-2:32

```

**`Value::Http2Session`** — HTTP/2 (RFC 7540), via reqwest/h2. Created from a `Handle@[Stream Tls]` with h2 ALPN:

```tinct
[h2: [http2-session tls-handle]]
[r:  [http-request h2 "GET" "/api" []]]
=== error
type errors:
  arity mismatch: expected 3 argument(s), got 1 (1 positional, 0 named) at 1:6-1:32
  arity mismatch: expected 5 argument(s), got 4 (4 positional, 0 named) at 2:6-2:39

```

**`Value::Http3Session`** — HTTP/3 (RFC 9114), over a QuicSession:

```tinct
[h3: [http3-session quic-session]]
[r:  [http-request h3 "GET" "/api" []]]
=== error
type errors:
  arity mismatch: expected 2 argument(s), got 1 (1 positional, 0 named) at 1:6-1:34
  arity mismatch: expected 5 argument(s), got 4 (4 positional, 0 named) at 2:6-2:39

```

`http-request` is the uniform application-level call across all HTTP session types, returning `{ok: {status: Int  headers: Dict  body: Bytes}} | {err: String}`.

## URI Values

The `uri`, `url`, and `urn` builtins parse URI strings and return **plain dicts** with the documented fields accessible via dot notation. There are no distinct `Value::Url` or `Value::Urn` enum variants — all three builtins return `Value::Dict`, and `type-of` reports `"Dict"` for all three. (`Value::Uri { scheme, uri }` exists as an enum variant but is reserved for internal use and is not produced by any builtin.)

### uri (RFC 3986 §3)

**`uri` parses a generic RFC 3986 URI** and returns a dict, covering all URI forms including non-hierarchical ones (mailto:, tel:, urn:, news:).

Fields accessible via dot notation:

| Field | Type | Description |
|-------|------|-------------|
| `scheme` | `String` | Lowercase scheme, e.g. `"https"`, `"mailto"`, `"urn"` |
| `username` | `String` or empty dict | Empty dict if absent or URI is non-hierarchical |
| `password` | `String` or empty dict | Empty dict if absent; splitting userinfo on `:` is a practical convention — RFC 3986 §3.2.1 treats the userinfo component as opaque. Password in URIs is deprecated per RFC 7235 §6.5. |
| `host` | `String` or empty dict | Empty dict for non-hierarchical URIs (mailto:, tel:, urn:, news:) |
| `port` | `Int` or empty dict | Empty dict for non-hierarchical or when unspecified; an empty port string (e.g., `"http://host:/path"`) is parsed as empty dict, not an error |
| `path` | `String` | Always present per RFC 3986 §3.3 (though may be empty) |
| `query` | `String` or empty dict | Raw query string without `?`; empty dict if absent |
| `fragment` | `String` or empty dict | Fragment without `#`; empty dict if absent |

```tinct
[u: [uri "https://user:pass@host:8080/path?q=1#frag"]]
u.scheme    # → "https"
u.host      # → "host"
u.port      # → 8080
u.path      # → "/path"
u.query     # → "q=1"
u.fragment  # → "frag"

[m: [uri "mailto:user@example.com"]]
m.scheme    # → "mailto"
m.host      # → [] (empty dict — non-hierarchical)
m.path      # → "user@example.com"
=== error
[E010] document pipeline: expected Dict, got String (defined at 2:1-2:9)
```

### url (RFC 3986 §3.2)

**`url` parses a hierarchical URI with a required authority (host and port)** and returns a dict. The builtin errors if the URI has no authority component. Network functions (`http-get`, `tls-layer`) accept url dicts.

Fields:

| Field | Type | Description |
|-------|------|-------------|
| `scheme` | `String` | Lowercase: `"https"`, `"http"`, `"postgres"`, `"s3"`, `"amqp"`, etc. |
| `username` | `String` or empty dict | Empty dict if absent |
| `password` | `String` or empty dict | Empty dict if absent; splitting userinfo on `:` is a convention not mandated by RFC 3986 §3.2.1; deprecated for HTTP (RFC 7235 §6.5) |
| `host` | `String` | Always present — validated at parse time; IPv6 addresses without brackets |
| `port` | `Int` | Always present — scheme-defaulted if absent (e.g., `443` for https, `80` for http); empty port string treated as absent and then defaulted |
| `path` | `String` | Always present; `"/"` if absent in the input string |
| `query` | `String` or empty dict | Raw query string without `?`; empty dict if absent |
| `fragment` | `String` or empty dict | Fragment without `#`; empty dict if absent |

```tinct
[u: [url "https://api.example.com/v1/users?page=2"]]
u.scheme    # → "https"
u.host      # → "api.example.com"
u.port      # → 443
u.path      # → "/v1/users"
u.query     # → "page=2"

# url errors for non-hierarchical URIs:
[url "mailto:user@example.com"]   # → Error: no authority component
[url "urn:isbn:978-0-306-40615-7"] # → Error: no authority component
=== error
[E010] document pipeline: expected Dict, got String (defined at 2:1-2:9)
```

### urn (RFC 8141)

**`urn` parses a URN per RFC 8141**: `urn:NID:NSS[?+r][?=q][#f]` and returns a dict. The builtin errors if the string is not a `urn:` URI.

Fields:

| Field | Type | Description |
|-------|------|-------------|
| `nid` | `String` | Namespace Identifier: `"isbn"`, `"uuid"`, `"oasis"`, etc. |
| `nss` | `String` | Namespace Specific String |
| `r-component` | `String` or empty dict | RFC 8141 §2.3 resolution parameters (`?+…`); empty dict if absent. RFC 8141 §2.3.1 states this component SHOULD NOT be used (reserved for future use); it is parsed and stored but should be ignored in most contexts. |
| `q-component` | `String` or empty dict | RFC 8141 §2.3 query parameters (`?=…`); empty dict if absent |
| `fragment` | `String` or empty dict | Fragment (`#…`); empty dict if absent |

```tinct
[u: [urn "urn:isbn:978-0-306-40615-7"]]
u.nid    # → "isbn"
u.nss    # → "978-0-306-40615-7"

[u: [urn "urn:uuid:6e8bc430-9c3a-11d9-9669-0800200c9a66"]]
u.nid    # → "uuid"
u.nss    # → "6e8bc430-9c3a-11d9-9669-0800200c9a66"
=== error
[E010] document pipeline: expected Dict, got String (defined at 2:1-2:6)
```

## Access Chain Evaluation — Formal Specification

Formalizes access forms (dot and `get` builtin) as an access algebra with compositional chain semantics. Access chains are the primary data extraction mechanism in tinct — they desugar to nested AST nodes that the evaluator reduces inside-out, materializing the target at each step.

The formal specification below covers dot access and the `get` builtin.

### Part 1: Access Algebra

An **access chain** is a sequence of projections applied left-to-right to a target expression. The parser produces nested AST nodes; the algebra makes the compositional structure explicit.

**Projections.** A projection `π` extracts data from a dict:

```text
π ::= dot(f)              — field access by literal string key f (or integer key n for dot-int access)
```

**Chains.** An access chain `C = π₁ · π₂ · ... · πₙ` applied to target expression `t` evaluates as left-to-right composition:

```text
eval_chain(t, [], ρ, d) = eval(t, ρ, d)                          (empty chain)
eval_chain(t, [π₁, ...πₙ], ρ, d) = eval_chain(apply(π₁, t, ρ, d), [π₂, ...πₙ], ρ, d)
```

**Parser correspondence:** The parser produces nested AST nodes for chains. `$a.b.0.c` parses as:

```text
DotAccess(
  DotAccess(
    DotAccess(VarRef("a"), "b"),
    Int(0)),
  "c")
```

Use `[get 0 $a.b].c` for dynamic key access followed by dot access.

The evaluator reduces inside-out: first `eval(VarRef("a"))`, then `apply(dot("b"), ...)`, then `apply(dot(0), ...)`, then `apply(dot("c"), ...)`. This inside-out reduction is equivalent to the left-to-right chain evaluation defined above.

### Part 2: Projection Rules

Each projection materializes its target to a `Dict`, then extracts by key. All three rules share a common materialization step formalized as `materialize_dict`.

**[MATERIALIZE-DICT]** — Common target materialization

```text
θ_target = eval(target, ρ, d+1)
v = materialize(θ_target, d+1)              (inherent materialization — must know dict structure)
v = Dict(map)                               (target must be Dict; type error otherwise)
────────────────────────────────────────────
materialize_dict(target, ρ, d) ⇒ map
```

If `v` is not a `Dict`, evaluation fails with `type_mismatch("Dict", v.type_name(), span)`. This is inherent materialization (§Selective Materialization) — the dict structure must be known to perform key lookup. MATERIALIZE-DICT is a composite rule combining `eval`, `materialize`, and pattern match — it is not a primitive judgment of the Thunk Lifecycle. ACCESS-DOT returns an alias to an existing thunk in the dict.

**[ACCESS-DOT]** — Dot access: `$target.field`

```text
map = materialize_dict(target, ρ, d)
key = String(field)                          (field is a literal string from the AST)
map[key] = θ                                 (look up key; error if absent)
────────────────────────────────────────────
eval_dot(target, field, ρ, d) ⇒ θ
```

Error case: if `key ∉ dom(map)`, error `key_not_found(field, span)`. No default — missing keys are always errors (§Null and Missing Keys).

### Part 3: Error Taxonomy

Error classes for current access forms:

| Error | Rule | Condition | Message |
|-------|------|-----------|---------|
| Target not a Dict | MATERIALIZE-DICT | `v` is not `Dict` | `type_mismatch("Dict", v.type_name())` |
| Key not found (dot) | ACCESS-DOT | `String(field) ∉ dom(map)` | `key_not_found(field)` |
| Key not found (`get`) | `get` builtin | `key ∉ dom(map)` | `key_not_found(key)` |

Error context is enriched via `push_frame`: dot access adds `"accessing .{field}"`.

### Part 4: Chain Properties

Five properties that hold for all access chains.

#### Property 1: Step-wise Materialization

*Statement:* Each projection in a chain invokes MATERIALIZE-DICT exactly once. In a chain `π₁ · π₂ · ... · πₙ`, MATERIALIZE-DICT is invoked `n` times — once per step. MATERIALIZE-DICT evaluates and materializes the target — if the target thunk is already `Materialized`, materialization is a cache hit (MATERIALIZE-CACHED from §Thunk Lifecycle).

*Proof sketch:* By induction on chain length. Each `apply(πᵢ, ...)` invokes MATERIALIZE-DICT, which calls `materialize(θ, d+1)`. The result of step `i` becomes the target of step `i+1`. No step materializes the target of a different step. ∎

#### Property 2: Result Laziness

*Statement:* ACCESS-DOT returns the thunk stored in the dict without materializing it. The result may be `Unevaluated`, `PendingBuiltin`, `PendingCall`, or `Materialized` — access does not trigger evaluation of the accessed value.

*Proof sketch:* ACCESS-DOT returns `Rc::clone(thunk)` from `map.get(&key)` — a pointer copy, not a `materialize` call. The thunk's state is unchanged by the access. ∎

#### Property 3: Error Short-Circuiting

*Statement:* If projection `πᵢ` in a chain fails, projections `πᵢ₊₁, ..., πₙ` are never evaluated.

*Proof sketch:* By the chain recurrence, `eval_chain(t, [π₁, ...πₙ], ρ, d)` first computes `apply(π₁, t, ρ, d)`. If this returns an error, the recurrence has no value to pass to the next step, so the chain terminates with that error. By induction, no subsequent projection is evaluated. ∎

#### Property 4: Unbounded Chain Length

*Statement:* In the iterative CEK machine, access chain steps push continuation frames onto the heap-allocated `Vec<Cont>`. Chain length is bounded only by available memory, with no hard depth limit. Each step pushes one `Cont::DotAccess` frame, which is popped after the target materializes.

*Proof sketch:* The CEK machine's `eval_step` for dot access pushes `Cont::DotAccess(field, span)` and transitions to `Action::Materialize(target)`. Each step adds one frame to the stack (O(1) space per step). There is no `MAX_EVAL_DEPTH` check in the access path — the old recursive depth budget was eliminated by the CEK machine (see [Evaluation](08-evaluation.md) §Iterative Evaluator). ∎

#### Property 5: Sharing Preservation

*Statement:* ACCESS-DOT returns an `Rc::clone` of the thunk stored in the dict — an alias, not a copy. If the same field is accessed twice, both accesses obtain pointers to the same `Rc<Thunk>`. Once the first access materializes it, the second access gets MATERIALIZE-CACHED (§Thunk Lifecycle).

*Proof sketch:* ACCESS-DOT returns `Rc::clone(thunk)` from `map.get(&key)`. The `Rc` reference count increases, but both the dict entry and the accessor hold pointers to the same `Thunk`. When either materializes it, the thunk transitions to `Materialized` (or `Failed`), and subsequent accesses via any alias see the cached state. This is the Launchbury (1993) sharing guarantee applied to record projection — access is observation, not duplication. ∎

### Part 5: Type System Correspondence

Under Boolean Algebraic Subtyping (BAS), all records are closed. Dot access on a closed record returns the declared field type if the field exists, or produces a type error if the field is missing. There is no "open record" fallback to `Any` — records are precise.

The type checker mirrors the access algebra with type-level projections:

| Runtime rule | Type rule | Type-level behavior |
|-------------|-----------|-------------------|
| ACCESS-DOT | `check_dot_access` (DotKey::Str arm) | `Record(fields) → fields[f]`; closed + missing → error |
| ACCESS-DOT (Int) | `check_dot_access` (DotKey::Int arm) | Integer dot access `.N`; looks up `Key::Int(N)` |
| `get` builtin | regular builtin call | Key access via `[get key data]` |

**Type variable access:** Accessing a field on a type variable (`TypeVar(α)`) is a type error (`typecheck.rs:313` falls through to `not_a_record`). Under BAS, all records are closed — openness is expressed via width subtyping rather than row variables.

**Closed records:** Under BAS, all records are closed. Dot access on a closed record returns the declared field type if present, or produces a type error if missing. There is no runtime fallback — the type checker enforces exact field presence.

**`get` builtin precision:** When the key passed to `[get key data]` is a literal (`Expr::Str` or `Expr::Int`), the type checker performs exact field lookup. When the key is a variable with type `Str`, `Int`, or `Any`, the result type is `Any` — since the key value is not known until runtime, the type checker cannot determine which field will be accessed. The `get` builtin is checked as a regular call.

### Part 6: Implementation Correspondence

| Formal rule | Implementation | Source |
|------------|----------------|--------|
| MATERIALIZE-DICT | Inlined in each access function (`eval` + `materialize` on target) | `eval_materialize.rs` |
| ACCESS-DOT | `eval()` returns `Unevaluated` thunk; `force_step()` via `DotAccessForce` continuation | `eval_materialize.rs` |
| `Key::PartialOrd` | `impl PartialOrd for Key` | `value.rs` |
| Chain nesting | Parser produces nested `DotAccess` AST nodes | `ast.rs` |
| Type-level dot | `check_dot_access()` | `typecheck.rs` |

### Part 7: Worked Examples

#### Example 1: Chained dot access

```tinct
[config: [database: [host: "localhost"  port: 5432]]]

[str config.database.host]
=== out
"localhost"
```

Chain: `dot("database") · dot("host")` applied to `config`.

1. `eval(VarRef("config"), ρ)` → `θ_config`
2. `materialize_dict(θ_config)` → `{database: θ_db}`. `map[String("database")]` → `θ_db`. Result: `θ_db` (lazy).
3. `materialize_dict(θ_db)` → `{host: θ_host, port: θ_port}`. `map[String("host")]` → `θ_host`. Result: `θ_host` (lazy).
4. `str` materializes `θ_host` → `"localhost"`.

Note: `θ_port` is never materialized — Property 2 (result laziness) means accessing `.host` does not evaluate `.port`.

**Example 2: Dynamic key access with `get` builtin**

```tinct
[get 0 services].host
=== error
type errors:
  undefined variable: services at 1:8-1:16

```

`[get 0 services]` calls the `get` builtin with key `Int(0)` and dict `services`. The builtin materializes `services`, looks up `Key::Int(0)` → `θ_svc0`. Then `.host` dot-accesses `θ_svc0`.

**Example 3: Subsequence with `slice`**

```tinct
data: [a: 1  b: 2  c: 3  d: 4]
[slice 1 3 data]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 27:1:5
  |
  1 | data: [a: 1  b: 2  c: 3  d: 4]
    |     ^
```

`[slice 1 3 data]` returns entries at positions 1 and 2 (half-open interval `[1, 3)` by insertion order), yielding `[0: 2  1: 3]` (renumbered). Use `slice`, `take`, and `drop` for subsequences.
