# Data Model

## Dicts Are Fundamental

The lowest-level unit is the dictionary (key-value pairs), not the list. First-class key-value pair syntax is core to the language.

A list is equivalent to a dict with integer keys:

```tinct
[a b c]  ≡  [0: a  1: b  2: c]
```

**Why this design:**
- **Unification** — One fundamental data structure. Functions like `map`, `filter`, `get` work uniformly on all data.
- **Flexibility** — Mixed integer and string keys naturally supported. Natural extension to keyword arguments.
- **First-class key-value pairs** — Matches the configuration language use case. Keys are names, not duplicated strings.

**Implementation:** May use different internal representations (dense vector for list-like data, HashMap for sparse/string keys) as a transparent performance optimization. Users never see the difference.

**Type-theoretic implication:** The static `Record` type tracks only string-keyed fields; integer-keyed (positional) entries are not part of the record type. A dict `[a b c  name: Alice]` has record type `[name: String]` — the positional entries `a`, `b`, `c` are invisible to the type checker. This is a deliberate consequence of unifying lists and records: positional entries are list-like data without static field names, while named entries form the record structure that type inference reasons about.

## One Bracket, One Structure

**`[]` is the only bracket type.** There is one syntax for the one fundamental data structure. Entries with `key:` are keyed; entries without get auto-incrementing integer keys. Both can appear in the same `[]`.

```tinct
[name: "Alice"  age: 30]        # All keyed — a "dict"
[a b c]                         # All auto-indexed — a "list" = [0: a  1: b  2: c]
[f x timeout: 60]               # Mixed — positional + named (implied call)
[]                              # Empty — list and dict are identical
```

**Parsing rule:** After parsing an entry, look ahead for `:`. If found, the entry is a key and the next thing is its value. If not, the entry is auto-indexed. The integer counter only increments for unkeyed entries — keyed entries don't consume an index.

**Positional and named entries may appear in any order.** Auto-indices are assigned sequentially to positional entries regardless of where named entries appear. For function calls, the binding priority chain (§Call Convention, C-PRIORITY) resolves positional arguments by index, then named arguments fill remaining parameters, then defaults apply.

## Heterogeneous Keys

**Allowed by default.** Integer and string keys can coexist in the same dict. Quoted strings are valid as keys, allowing keys with spaces or special characters: `["my key": value  "another:key": 42]`.

**Computed keys and the type checker:** Dict keys can be variable references (`[$k: value]`). The evaluator resolves computed keys at runtime. The type checker resolves them at compile time via literal types: if `$k` has type `StringLiteral("name")`, the field name is `"name"`. If the type is not a literal (e.g., plain `String`), the field is excluded from the Record type. See "Literal types enable computed key resolution" in the Type System section.

## Insertion Order

**Dicts preserve insertion order for iteration and display.** Semantically, entry order doesn't matter (letrec scoping). But iteration via `$keys`, `$values`, `$map` etc. follows the order entries appear in source. `$merge` preserves left order, appends new keys from right.

## Duplicate Keys Are Errors

**Duplicate keys in dict literals are an error.** Use `merge` for intentional overrides.

```tinct
[name: "Alice"  name: "Bob"]              # → Error: duplicate key "name"
[merge [name: "Alice"] [name: "Bob"]]     # → [name: "Bob"]  (right-biased, intentional)
```

**Why:** Duplicate keys + lazy evaluation creates confusing semantics — depending on the scoping model, derived values may see different bindings of the same key. Prohibiting duplicates eliminates the ambiguity entirely and catches copy-paste errors.

## Equality

**Dict equality is order-insensitive and structural.** Two dicts are equal if they have the same key set and equal values at each key, regardless of insertion order. This follows from the extensional (finite-map) semantics of Dict: a dict is a partial function from keys to values, and two functions are equal when they agree on every point in their domain.

```tinct
[= [a: 1  b: 2] [b: 2  a: 1]]   # → true  (same keys and values, different order)
[= [a: 1] [a: 2]]                 # → false (value at "a" differs)
[= [a: 1  b: 2] [a: 1]]          # → false (different key sets)
[= [] []]                          # → true  (empty dicts are equal)
```

Both Record and Map forms use the same order-insensitive comparison — the runtime representation is the same `Value::Dict`, so `=` treats them identically. Cycle detection via a visited-pair set prevents infinite loops on self-referential structures.

Functions and builtins always compare as unequal to each other (no meaningful closure equality).

## Numeric Types — `Int`, `Float`, `Number`

**Two concrete types: `Int(i64)` and `Float(f64)`.** `Number` is the supertype that accepts either. Integer literals carry their value: `42` has type `IntLiteral(42)`, which is a subtype of `Int`. Float literals do not have a literal type variant because floats cannot be dict keys.

```tinct
port: 8080                      # Int — no decimal point
pi: 3.14                        # Float — has decimal point
x@Int                           # must be an integer
y@Float                         # must be a float
z@Number                        # accepts either
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
```

**Precision-safe promotion.** Implicit Int→Float promotion in mixed-type arithmetic operations errors when the integer's magnitude exceeds `2^53`, the largest integer exactly representable in an `f64` mantissa. This prevents silent precision loss:

```tinct
[+ 9007199254740992 1.0]        # → 9007199254740993.0 (2^53, exact)
[+ 9007199254740993 1.0]        # → Error: Int→Float promotion would lose precision
```

**Explicit float conversion** — `[float n]` builtin performs unconditional Int→Float conversion without precision checks, allowing controlled precision loss when desired. For Float inputs, `float` is a no-op.

```tinct
[float 9007199254740993]        # → 9007199254740992.0 (loss of precision, intentional)
[float 3.14]                    # → 3.14 (no-op on Float inputs)
```

**Integer arithmetic uses checked semantics.** `Int` operations (`+`, `-`, `*`) use Rust's `checked_add`/`checked_sub`/`checked_mul`, so overflow returns an error rather than wrapping or panicking. This prevents silent data corruption on large values. Width-specific types like `Int32` could enforce narrower range constraints via the contracts system.

**Dict key integration:** `Int` values are directly usable as dict keys. `Float` values cannot be used as keys — floating-point equality semantics make them unreliable as hash keys.

**Width-specific types** (`Int32`, `Int64`, `Int128`, `Decimal`, etc.) are range constraints expressed through the contracts system, not new runtime representations. `Decimal` (if ever needed) would require a new Value variant.

The promotion table is built into the evaluator. User-defined numeric types participating in arithmetic would require type classes — see `doc/whatif/typeclasses.md` for the accepted design.

## No Null — Missing Keys Are Errors

**No `null` value in the language.** Accessing a nonexistent key is an error.

```tinct
[get person "name"]              # → "Alice"
[get person "occupation"]        # → Error: key "occupation" not found

# Safe alternative with default
[get-or config "timeout" 30]    # → 30 if "timeout" is missing

# Check existence
[has? config "timeout"]          # → true/false
```

**Why no null:**
- **Row polymorphism catches it at compile time.** A function taking `[name: String ...]` guarantees `name` exists. Most missing-key bugs never reach runtime.
- **Lazy eval provides a safety net.** `[x: [get dict "maybe-missing"]]` doesn't error until `x` is materialized. If you never use `x`, no error.
- **No null confusion.** Can't confuse "key exists with null" vs "key is missing." Every key that exists has a real value.
- **Clean data representation.** Config files have no `null` noise — every key is meaningful.

**JSON null mapping:** Since Tinct has no null value, `from-json` (and CLI stdin JSON injection) maps JSON `null` to `[]` (empty dict). This means it is impossible to distinguish "was null" from "was empty object" after conversion. This is an intentional trade-off -- Tinct's "no null" design prioritizes simplicity over round-trip fidelity with JSON.

## Data Access — Two Modes

Data access has two distinct modes: **key-based** (look up by key) and **position-based** (look up by insertion-order index). For dense lists `[a b c]` = `[0: a 1: b 2: c]`, these coincide. They diverge for sparse or mutated dicts.

**Key-based access** — dot notation and `get` builtin:

```tinct
# Dot notation (string keys and integer dot access)
person.name                     # string key "name"
config.database.host            # chained string key access
data.0                          # integer dot access — looks up Key::Int(0)

# get builtin (dynamic key access, replaces bracket access)
[get 5 data]                    # Integer key 5
[get "name" data]               # String key "name"
[get $key data]                 # Computed key lookup
[get 0 config.services].host    # Dynamic key then dot chain
```

**Rules:** Identifiers can start access chains directly — `foo.bar` and `$foo.bar` are both valid. `[get key data]` finds the entry whose key matches `key`, not the nth entry by position.

**Note:** Bracket access (`data[5]`, `data[$key]`) was removed in access-pipeline-phase2. Use `[get key data]` for integer and dynamic key access.

**Subsequence operations** — stdlib functions:

```tinct
[slice data 2 5]                # Entries at positions 2, 3, 4 (position-based)
[take 3 data]                   # First 3 entries
[drop 2 data]                   # All entries after the first 2
```

**Note:** Range access (`data[2..5]`, `data[2..]`, `data[..3]`) was removed in access-pipeline-phase2. Use `slice`, `take`, and `drop` for subsequences.

**Position-based access** — stdlib functions:

```tinct
[nth data 0]                    # First entry (position 0)
[nth data -1]                   # Last entry (negative = from end)
[last data]                     # Last entry (alias)
[slice data 2 5]                # Entries at positions 2, 3, 4
```

**Why the split:** Position-based access on a dict that has been mutated over time has less-than-useful ordering. Making it a function call (not syntax) signals that it's the unusual operation. For the common case of dense lists, `[get 0 data]` (key 0) and `[nth data 0]` (position 0) return the same thing — you never need `nth` unless you specifically want insertion-order semantics on sparse data.

### Lazy Sequences — Value::Seq

**Lazy sequences (`Value::Seq`) are a runtime-only value type** representing infinite or demand-driven data (from `$range`, `$repeat`, `$cycle`, `$iterate`, etc.). They exist alongside `Dict`, `Int`, `Float`, `String`, `Bool`, `Function`, `Handle`, `HttpConn`, `Uri`, `Url`, and `Urn` in the value representation. Sequences have no literal syntax — they are produced by builtin functions and consumed by sequence operations like `$map`, `$filter`, `$take`, `$collect`.

Sequences are dual-dispatch targets: `$map` on a Seq returns a lazy Seq, `$filter` on a Seq returns a lazy Seq. Use `$collect` to materialize a Seq to a dense dict. Attempting operations that require full materialization (like `$sort` or `$length`) on an infinite Seq will error. See doc/08-evaluation.md §Lazy Sequences for implementation details and laziness semantics.

### Handles — Value::Handle

**Handles (`Value::Handle`) are runtime-only values representing open I/O resources** — file descriptors, network streams, and other OS-level channels. A Handle is an unforgeable reference in the capability sense (Dennis & Van Horn 1966): holding it is sufficient authority to perform I/O; no separate capability argument is required at use time.

#### Capability Row

Every Handle carries a **capability row** — a `HashMap<String, Value>` mapping capability names to associated data. The row is immutable after construction; each operation that adds a capability produces a new Handle wrapping the old one. The capability row determines which builtins are callable on the Handle:

| Capability | Value | Granted by | Required by |
|-----------|-------|------------|-------------|
| `Readable` | `Value::Null` | `open … Readable`, `connect`, `tls-connect` | `slurp`, `lines`, `read` |
| `Writable` | `Value::Null` | `open … Writable`, `connect`, `tls-connect` | `write`, stream writes |
| `Binary` | `Value::Null` | `connect`, `tls-connect` | `slurp` (binary mode) |
| `Text` | `Value::Null` | `open … Readable` on text files | `lines`, `slurp` (text mode) |
| `Stream` | `Value::Null` | `connect … Tcp`, `tls-connect` | streaming reads/writes |
| `Datagram` | `Value::Null` | `connect … Udp` | datagram I/O |
| `Seekable` | `Value::Null` | regular file `open` | `seek` |
| `Tls` | `Value::Dict` (TLS metadata) | `tls-connect` | `tls-peer-cert` |

Boolean capabilities (Readable, Writable, Binary, Text, Stream, Datagram, Seekable) store `Value::Null` as their associated data — the presence of the key is the entire capability. Protocol capabilities like `Tls` store structured data: the `Tls` value is a dict containing the leaf certificate metadata and negotiated ALPN protocol string.

**Reading capability data:** Use `cap-data h name` to read the associated `Value` for a capability, and `has-cap? h name` to test whether a capability is present without extracting data.

#### Network Handles

`connect` dispatches on the Transport variant to determine the address format and capability routing. Port is absent for transports that have no port concept:

```
# Stream transports (NetCap)
connect cap Tcp  host port       → Handle{ Binary Readable Writable Stream }
connect cap Udp  host port       → Handle{ Binary Readable Writable Datagram }
connect cap Icmp host            → Handle{ Binary Readable Writable Datagram }

# Local transports (DirCap)
connect cap UnixStream    path   → Handle{ Binary Readable Writable Stream }
connect cap UnixDatagram  path   → Handle{ Binary Readable Writable Datagram }
connect cap NamedPipe     path   → Handle{ Binary Readable Writable }

# TLS Layer (Handle → Handle upgrade)
tls-layer handle sni opts        → Handle{ …existing… Tls→{cert…} }
```

Capability routing: `Tcp`/`Udp`/`Icmp` require a `NetCap` (allowlist checked before syscall); `UnixStream`/`UnixDatagram`/`NamedPipe` require a `DirCap` (cap_std path-based access). User-defined Connectors handle their own capability checks.

The `Tls` capability value is a dict with the same fields as the `PeerCert` type returned by `tls-peer-cert` (see [Builtins](11a-builtins.md) §Network).

#### Layers — Handle→Handle Protocol Upgrades

A **Layer** is any function that takes a Handle and returns a Handle with augmented capabilities (`Handle[R] → Handle[R ∪ NewCaps]`). There is no Layer typeclass — the composition is structural. Any pure-tinct function with the right signature is a Layer.

Standard library Layers: `tls-layer` (TLS/STARTTLS upgrade, Rust builtin), `socks5-layer` (SOCKS5 tunnel, pure tinct in `protocols/socks5.llt`), `http-connect-layer` (HTTP CONNECT tunnel, pure tinct in `net.llt`).

Layers compose left-to-right with Connectors:

```tinct
[tcp:  [connect %nc Tcp "proxy.corp" 1080]]
[tun:  [socks5-layer tcp "api.internal" 443]]
[tls:  [tls-layer tun "api.internal" tls-opts]]
```

The original Handle is consumed; subsequent operations on it produce a runtime error. The new Handle wraps the protocol-upgraded connection.

#### Sessions — Multiplexed Connections

A **Session** is a multiplexed connection: one physical channel carrying multiple independent logical streams. Sessions are opened from Handles or Connectors; stream Handles are opened from Sessions.

Three Session types exist as runtime-only opaque values:

**`Value::QuicSession`** — QUIC (RFC 9000), implemented via `quinn`. QUIC integrates transport, TLS, and reliable delivery at the UDP level. `quinn` owns the UDP socket internally (managing path migration, congestion control, ACKs):

```tinct
[quic:   [quic-session %nc "api.example.com" 443 quic-opts]]
[stream: [quic-open-stream quic]]    # → Handle{ Binary Readable Writable Stream }
```

**`Value::Http2Session`** — HTTP/2 (RFC 7540), via reqwest/h2. Created from a `Handle[Stream Tls]` with h2 ALPN:

```tinct
[h2: [http2-session tls-handle]]
[r:  [http-request h2 "GET" "/api" []]]
```

**`Value::Http3Session`** — HTTP/3 (RFC 9114), over a QuicSession:

```tinct
[h3: [http3-session quic-session]]
[r:  [http-request h3 "GET" "/api" []]]
```

`http-request` is the uniform application-level call across all HTTP session types, returning `{ok: {status: Int  headers: Dict  body: Bytes}} | {err: String}`.

### URI Values — Uri, Url, Urn

Three RFC 3986 value types represent parsed URIs at the tinct value level. These are produced by the `uri`, `url`, and `urn` builtins and are distinct from raw strings — their fields are accessible via dot notation.

#### Uri — Value::Uri (RFC 3986 §3)

**`Value::Uri` represents a generic RFC 3986 URI**, covering all URI forms including non-hierarchical ones (mailto:, tel:, urn:, news:). The `uri` builtin parses any URI string and returns a Uri.

Fields accessible via dot notation:

| Field | Type | Description |
|-------|------|-------------|
| `scheme` | `String` | Lowercase scheme, e.g. `"https"`, `"mailto"`, `"urn"` |
| `username` | `String` or `Null` | Null if absent or URI is non-hierarchical |
| `password` | `String` or `Null` | Null if absent; splitting userinfo on `:` is a practical convention — RFC 3986 §3.2.1 treats the userinfo component as opaque. Password in URIs is deprecated per RFC 7235 §6.5. |
| `host` | `String` or `Null` | Null for non-hierarchical URIs (mailto:, tel:, urn:, news:) |
| `port` | `Int` or `Null` | Null for non-hierarchical or when unspecified; an empty port string (e.g., `"http://host:/path"`) is parsed as null, not an error |
| `path` | `String` | Always present per RFC 3986 §3.3 (though may be empty) |
| `query` | `String` or `Null` | Raw query string without `?`; null if absent |
| `fragment` | `String` or `Null` | Fragment without `#`; null if absent |

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
m.host      # → null (non-hierarchical)
m.path      # → "user@example.com"
```

#### Url — Value::Url (RFC 3986 §3.2)

**`Value::Url` represents a hierarchical URI with a required authority (host and port)**. The `url` builtin parses the string and errors if the URI has no authority component. All network functions (`http-get`, `http-connect`, `tls-connect`) accept `Url`, not the generic `Uri`.

Fields:

| Field | Type | Description |
|-------|------|-------------|
| `scheme` | `String` | Lowercase: `"https"`, `"http"`, `"postgres"`, `"s3"`, `"amqp"`, etc. |
| `username` | `String` or `Null` | Null if absent |
| `password` | `String` or `Null` | Null if absent; splitting userinfo on `:` is a convention not mandated by RFC 3986 §3.2.1; deprecated for HTTP (RFC 7235 §6.5) |
| `host` | `String` | Always present — validated at parse time; IPv6 addresses without brackets |
| `port` | `Int` | Always present — scheme-defaulted if absent (e.g., `443` for https, `80` for http); empty port string treated as absent and then defaulted |
| `path` | `String` | Always present; `"/"` if absent in the input string |
| `query` | `String` or `Null` | Raw query string without `?`; null if absent |
| `fragment` | `String` or `Null` | Fragment without `#`; null if absent |

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
```

#### Urn — Value::Urn (RFC 8141)

**`Value::Urn` represents a URN per RFC 8141**: `urn:NID:NSS[?+r][?=q][#f]`. The `urn` builtin parses the string and errors if it is not a `urn:` URI.

Fields:

| Field | Type | Description |
|-------|------|-------------|
| `nid` | `String` | Namespace Identifier: `"isbn"`, `"uuid"`, `"oasis"`, etc. |
| `nss` | `String` | Namespace Specific String |
| `r-component` | `String` or `Null` | RFC 8141 §2.3 resolution parameters (`?+…`); null if absent. RFC 8141 §2.3.1 states this component SHOULD NOT be used (reserved for future use); it is parsed and stored but should be ignored in most contexts. |
| `q-component` | `String` or `Null` | RFC 8141 §2.3 query parameters (`?=…`); null if absent |
| `fragment` | `String` or `Null` | Fragment (`#…`); null if absent |

```tinct
[u: [urn "urn:isbn:978-0-306-40615-7"]]
u.nid    # → "isbn"
u.nss    # → "978-0-306-40615-7"

[u: [urn "urn:uuid:6e8bc430-9c3a-11d9-9669-0800200c9a66"]]
u.nid    # → "uuid"
u.nss    # → "6e8bc430-9c3a-11d9-9669-0800200c9a66"
```

### List vs Dict Operations — Renumbering Rule

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
```

**`conj` on sparse data:** `conj` delegates to `append`, which uses the maximum existing integer key + 1 as the new key (or 0 if no integer keys exist). This avoids key collisions even on sparse data:

```tinct
# Dense list — conj works as expected
[conj [a b c] d]                        # → [0: a  1: b  2: c  3: d]

# Sparse data — no collision, key 11 is used (max 10 + 1)
sparse: [0: a  5: b  10: c]
[conj sparse d]                         # → [0: a  5: b  10: c  11: d]
```

### Access Chain Evaluation — Formal Specification

Formalizes access forms (dot and `get` builtin) as an access algebra with compositional chain semantics. Access chains are the primary data extraction mechanism in tinct — they desugar to nested AST nodes that the evaluator reduces inside-out, forcing the target at each step.

**Note:** Bracket access (`data[key]`) and range access (`data[2..5]`) were removed in access-pipeline-phase2. The formal specification below covers the current implementation: dot access and the `get` builtin. The ACCESS-BRACKET and ACCESS-RANGE rules below are retained as historical reference (they document the removed evaluation rules).

#### Part 1: Access Algebra

An **access chain** is a sequence of projections applied left-to-right to a target expression. The parser produces nested AST nodes; the algebra makes the compositional structure explicit.

**Projections.** A projection `π` extracts data from a dict:

```
π ::= dot(f)              — field access by literal string key f (or integer key n for dot-int access)
```

(Historical: `bracket(e)` and `range(s?, e?)` projections were removed in access-pipeline-phase2. Use `[get key data]` for dynamic key access and `[slice data start end]` for subsequences.)

**Chains.** An access chain `C = π₁ · π₂ · ... · πₙ` applied to target expression `t` evaluates as left-to-right composition:

```
eval_chain(t, [], ρ, d) = eval(t, ρ, d)                          (empty chain)
eval_chain(t, [π₁, ...πₙ], ρ, d) = eval_chain(apply(π₁, t, ρ, d), [π₂, ...πₙ], ρ, d)
```

**Parser correspondence:** The parser produces nested AST nodes for chains. `$a.b.0.c` parses as:

```
DotAccess(
  DotAccess(
    DotAccess(VarRef("a"), "b"),
    Int(0)),
  "c")
```

(Bracket access was removed in access-pipeline-phase2. Use `[get 0 $a.b].c` to look up integer key 0 then dot-access "c".)

The evaluator reduces inside-out: first `eval(VarRef("a"))`, then `apply(dot("b"), ...)`, then `apply(dot(0), ...)`, then `apply(dot("c"), ...)`. This inside-out reduction is equivalent to the left-to-right chain evaluation defined above.

#### Part 2: Projection Rules

Each projection forces its target to a `Dict`, then extracts by key. All three rules share a common forcing step formalized as `force_dict`.

**[FORCE-DICT]** — Common target forcing

```
θ_target = eval(target, ρ, d+1)
v = force(θ_target, d+1)                    (inherent materialization — must know dict structure)
v = Dict(map)                               (target must be Dict; type error otherwise)
────────────────────────────────────────────
force_dict(target, ρ, d) ⇒ map
```

If `v` is not a `Dict`, evaluation fails with `type_mismatch("Dict", v.type_name(), span)`. This is inherent materialization (§Selective Materialization) — the dict structure must be known to perform key lookup. FORCE-DICT is a composite rule combining `eval`, `force`, and pattern match — it is not a primitive judgment of the Thunk Lifecycle. ACCESS-DOT returns an alias to an existing thunk in the dict.

**[ACCESS-DOT]** — Dot access: `$target.field`

```
map = force_dict(target, ρ, d)
key = String(field)                          (field is a literal string from the AST)
map[key] = θ                                 (look up key; error if absent)
────────────────────────────────────────────
eval_dot(target, field, ρ, d) ⇒ θ
```

Error case: if `key ∉ dom(map)`, error `key_not_found(field, span)`. No default — missing keys are always errors (§No Null — Missing Keys Are Errors).

**[ACCESS-BRACKET]** — Bracket access (historical — removed in access-pipeline-phase2)

Bracket access (`$target[key_expr]`) was removed. Use `[get key_expr target]` (the `get` builtin) for dynamic key access. The `get` builtin evaluates its key argument and materializes it to a concrete `String` or `Int` key, then performs the lookup. Error if key not found.

**[ACCESS-RANGE]** — Range access (historical — removed in access-pipeline-phase2)

Range access (`$target[start..end]`) was removed. Use `[slice target start end]`, `[take n target]`, or `[drop n target]` for subsequences. These builtins work on position (insertion order), not on key values.

#### Part 3: Error Taxonomy

Error classes for current access forms:

| Error | Rule | Condition | Message |
|-------|------|-----------|---------|
| Target not a Dict | FORCE-DICT | `v` is not `Dict` | `type_mismatch("Dict", v.type_name())` |
| Key not found (dot) | ACCESS-DOT | `String(field) ∉ dom(map)` | `key_not_found(field)` |
| Key not found (`get`) | `get` builtin | `key ∉ dom(map)` | `key_not_found(key)` |

Error context is enriched via `push_frame`: dot access adds `"accessing .{field}"`. (Bracket and range push_frame entries were removed with ACCESS-BRACKET and ACCESS-RANGE in access-pipeline-phase2.)

#### Part 4: Chain Properties

Five properties that hold for all access chains.

**Property 1: Step-wise Forcing**

*Statement:* Each projection in a chain invokes FORCE-DICT exactly once. In a chain `π₁ · π₂ · ... · πₙ`, FORCE-DICT is invoked `n` times — once per step. FORCE-DICT evaluates and forces the target — if the target thunk is already `Materialized`, forcing is a cache hit (FORCE-CACHED from §Thunk Lifecycle).

*Proof sketch:* By induction on chain length. Each `apply(πᵢ, ...)` invokes FORCE-DICT, which calls `force(θ, d+1)`. The result of step `i` becomes the target of step `i+1`. No step forces the target of a different step. ∎

**Property 2: Result Laziness**

*Statement:* ACCESS-DOT returns the thunk stored in the dict without forcing it. The result may be `Unevaluated`, `PendingBuiltin`, `PendingCall`, or `Materialized` — access does not trigger evaluation of the accessed value.

*Proof sketch:* ACCESS-DOT returns `Rc::clone(thunk)` from `map.get(&key)` — a pointer copy, not a `force` call. The thunk's state is unchanged by the access. ∎

**Property 3: Error Short-Circuiting**

*Statement:* If projection `πᵢ` in a chain fails, projections `πᵢ₊₁, ..., πₙ` are never evaluated.

*Proof sketch:* By the chain recurrence, `eval_chain(t, [π₁, ...πₙ], ρ, d)` first computes `apply(π₁, t, ρ, d)`. If this returns an error, the recurrence has no value to pass to the next step, so the chain terminates with that error. By induction, no subsequent projection is evaluated. ∎

**Property 4: Depth Consumption**

*Statement:* A chain of length `n` consumes `n` depth levels — each FORCE-DICT invocation increments depth by 1 (via `eval(target, ρ, d+1)` and `materialize(θ, d+1)` in each access function).

*Proof sketch:* By inspection of FORCE-DICT, which passes `d+1` to both `eval` and `materialize`. Each chain step invokes FORCE-DICT once (Property 1), so `n` steps consume `n` depth levels. For `MAX_EVAL_DEPTH = 256` and typical chain lengths (1–5), this is negligible. The CEK machine removes MAX_EVAL_DEPTH, making this property moot. ∎

**Property 5: Sharing Preservation**

*Statement:* ACCESS-DOT returns an `Rc::clone` of the thunk stored in the dict — an alias, not a copy. If the same field is accessed twice, both accesses obtain pointers to the same `Rc<Thunk>`. Once the first access forces it, the second access gets FORCE-CACHED (§Thunk Lifecycle).

*Proof sketch:* ACCESS-DOT returns `Rc::clone(thunk)` from `map.get(&key)`. The `Rc` reference count increases, but both the dict entry and the accessor hold pointers to the same `Thunk`. When either forces it, the thunk transitions to `Materialized` (or `Failed`), and subsequent accesses via any alias see the cached state. This is the Launchbury (1993) sharing guarantee applied to record projection — access is observation, not duplication. ∎

#### Part 5: Type System Correspondence

Access chain type checking generates row constraints via Remy-style row-variable unification (see §Row-Variable Unification in [Type System Extensions](07-type-extensions.md) Part 5). The target type is inferred first, then field access generates constraints of the form `unify(typeof(x), Record([field: α], ρ))`, binding `α` and `ρ` via row unification — enabling the type checker to infer field requirements from usage without annotations.

The type checker mirrors the access algebra with type-level projections:

| Runtime rule | Type rule | Type-level behavior |
|-------------|-----------|-------------------|
| ACCESS-DOT | `check_dot_access` | `Record(fields) → fields[f]`; open record → `Any`; closed + missing → error |
| ACCESS-DOT (Int) | `check_dot_access_int` | Integer dot access `.N`; looks up `Key::Int(N)`; open record → `Any` |
| `get` builtin | `check_bracket_access` (historical) | Now handled as a regular builtin call; key access via `[get key data]` |

**Type variable access:** Accessing a field on a type variable (`TypeVar(α)`) is a type error (`typecheck.rs:313` falls through to `not_a_record`). Constraint-based row unification would bind `α` to `Record([field: β], ρ)` — see §Row-Variable Unification in [Type System Extensions](07-type-extensions.md). Row variables (`RowVar(r)`) appearing in record types are treated as markers for openness during access type checking; they are not bound to remainder types during access operations (consistent with U-REC in §Type Inference Algorithm).

**Open records and Any:** When a dot access targets an open record (`Record(fields, Open)` or `Record(fields, RowVar(_))`) and the field is not in `fields`, the type checker returns `Any` rather than an error. This reflects Tinct's gradual typing design: open records may contain fields not visible to the type checker. Rather than reject valid programs, the type checker admits the access but types the result as `Any`, deferring validation to runtime. This is sound because `Any` serves as both top and bottom type (S-ANY-TOP, S-ANY-BOT in §Type Inference Algorithm) — values of any type flow through `Any` positions. For closed records, a missing field is a static error.

**`get` builtin precision:** When the key passed to `[get key data]` is a literal (`Expr::Str` or `Expr::Int`), the type checker performs exact field lookup. When the key is a variable with type `Str`, `Int`, or `Any`, the result type is `Any` — since the key value is not known until runtime, the type checker cannot determine which field will be accessed. The `get` builtin is now checked as a regular call rather than via the historical `check_bracket_access` function (removed in access-pipeline-phase2).

#### Part 6: Implementation Correspondence

| Formal rule | Implementation | Source |
|------------|----------------|--------|
| FORCE-DICT | Inlined in each access function (`eval` + `materialize` on target) | `eval_materialize.rs` |
| ACCESS-DOT | `eval()` returns `Unevaluated` thunk; `force_step()` via `DotAccessForce` continuation | `eval_materialize.rs` |
| `Key::PartialOrd` | `impl PartialOrd for Key` | `value.rs` |
| Chain nesting | Parser produces nested `DotAccess` AST nodes | `ast.rs` |
| Type-level dot | `check_dot_access()` | `typecheck.rs` |
| Note: `BracketForceTarget`, `eval_range_access`, `key_in_range`, `check_bracket_access`, `check_range_access` | All removed in access-pipeline-phase2 | — |

#### Part 7: Worked Examples

**Example 1: Chained dot access**

```tinct
[config: [database: [host: "localhost"  port: 5432]]]

[str config.database.host]
```

Chain: `dot("database") · dot("host")` applied to `config`.
1. `eval(VarRef("config"), ρ)` → `θ_config`
2. `force_dict(θ_config)` → `{database: θ_db}`. `map[String("database")]` → `θ_db`. Result: `θ_db` (lazy).
3. `force_dict(θ_db)` → `{host: θ_host, port: θ_port}`. `map[String("host")]` → `θ_host`. Result: `θ_host` (lazy).
4. `str` forces `θ_host` → `"localhost"`.

Note: `θ_port` is never forced — Property 2 (result laziness) means accessing `.host` does not evaluate `.port`.

**Example 2: Dynamic key access with `get` builtin**

```tinct
[get 0 services].host
```

`[get 0 services]` calls the `get` builtin with key `Int(0)` and dict `services`. The builtin materializes `services`, looks up `Key::Int(0)` → `θ_svc0`. Then `.host` dot-accesses `θ_svc0`.

(Historical: The old `services[0].host` — bracket access followed by dot — was removed in access-pipeline-phase2.)

**Example 3: Subsequence with `slice` (replaces range access)**

```tinct
data: [a: 1  b: 2  c: 3  d: 4]
[slice data 1 3]
```

`[slice data 1 3]` returns entries at positions 1 and 2 (half-open interval `[1, 3)` by insertion order), yielding `[0: 2  1: 3]` (renumbered). Use `slice`, `take`, and `drop` for subsequences.

(Historical: The old `data["b".."d"]` — range access by key value — was removed in access-pipeline-phase2.)
