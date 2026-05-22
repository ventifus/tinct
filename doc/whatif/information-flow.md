# What If: Information Flow Control

**State:** Proposal — 2026-05-22

**Depends on:** [`lib-net-v3.md`](lib-net-v3.md) — network boundaries are the primary taint introduction points; the `LabeledBytes t` and `ByteLabel` typeclass designs motivate this proposal.

---

## Problem

Every network boundary is an untrusted data entry point. Every cryptographic key is data that must not leak. Tinct has no way to express these constraints in the type system — nothing prevents:

```tinct
# Reads user-controlled HTTP request body
body: [read-bytes conn 4096]

# Directly used as a SQL query — SQL injection; type checker sees only Bytes
[db-query db-cap body]
```

```tinct
# API key loaded from config
api-key@[Bytes 32]: [slurp key-cap "api-key.bin"]

# Accidentally logged — type checker sees only [Bytes 32]
[log [str "Connecting with key: " api-key]]
```

Without IFC, the type system cannot distinguish data that must be validated before use from data that is safe, or data that must be protected from data that can be observed.

---

## The Two Labels

### `Tainted` — externally-controlled, unvalidated

Data originating from outside the trust boundary: network input, environment variables, user-supplied arguments, DNS responses. `Tainted t` cannot flow into any security-sensitive operation (database queries, shell execution, path construction, credential comparison) without explicit sanitization that strips the taint.

```tinct
Tainted: [type [t] [Tainted t]]
```

### `Secret` — sensitive, must not leak

Data that must not appear in logs, error messages, network responses, or any observable output without explicit declassification. Cryptographic keys, passwords, tokens, PII.

```tinct
Secret: [type [t] [Secret t]]
```

Both labels are nominal wrapper types. Operations on labeled values propagate the label through the type system.

---

## Label Propagation

The type checker enforces propagation: any expression that depends on a `Tainted t` value produces a `Tainted result`. Any expression that depends on a `Secret t` value produces a `Secret result`. This is equivalent to tracking data dependencies through the computation.

```tinct
# Taint propagates
body: [Tainted Bytes]    # from read-bytes on a network Handle
len:  [length body]      # → Tainted Int (depends on tainted input)
msg:  [str "Length: " len]  # → Tainted String

# Secret propagates
key:  [Secret [Bytes 32]]
hash: [sha256 key]       # → Secret [Bytes 32] (sha256 output reveals info about key)
```

Labels combine when a computation depends on multiple labeled values:

```tinct
# A value that depends on both tainted input AND a secret is both
response: [encrypt key user-data]   # user-data: Tainted, key: Secret → Secret Tainted Bytes
```

---

## Entry Points

Labels are introduced at system boundaries — places where data crosses from the outside world into tinct:

```tinct
# Network input (lib-net-v3)
read-bytes:    [Fn [h@Handle n@Int] [Tainted Bytes]]
recv-datagram: [Fn [sock@UdpSocket] [Tainted UdpDatagram]]

# Cryptographic key material
crypto-random: [Fn [len@Int] [Secret Bytes]]    # random bytes are sensitive
x25519-keypair: [Fn [] [private: [Secret [Bytes 32]]  public: [Bytes 32]]]
                #           ^^^^ private key is Secret; public key is not

# Environment / config (intentionally loaded secrets)
slurp-secret: [Fn [cap@DirCap path@String] [Secret Bytes]]
              # caller asserts the file contains sensitive data

# User input, env vars
env-var: [Fn [name@String] [Tainted [or String Null]]]
```

---

## Exit Points — Sanitization and Declassification

### Sanitizing `Tainted` values

Sanitizers are explicit functions that validate tainted data and, on success, return a clean value:

```tinct
# Pattern validation strips taint when the input matches
validate-pattern: [Fn [Tainted String  pat@Regex] [Result String]]
parse-int:        [Fn [Tainted String] [Result Int]]
parse-json:       [Fn [Tainted Bytes] [Result Dict]]

# Parameterized queries — take tainted strings but prevent injection structurally
db-query: [Fn [cap@DbCap sql@String params@[Seq Tainted]] [Result Seq]]
#                                          ^^^^^ tainted params are safe here —
#                                          the db driver escapes them structurally
```

### Declassifying `Secret` values

Declassification is the explicit, deliberate decision to allow a secret to be used in an observable way:

```tinct
# Comparison without revealing — timing-safe, returns Bool not the secret
constant-time-eq: [Fn [Secret Bytes  other@Bytes] Bool]

# Logging redaction — writes "[REDACTED]" to the log, not the value
log-redacted: [Fn [Secret t] Null]

# Intentional export (for backup, transmission) — requires explicit DirCap/NetCap
export-secret: [Fn [cap@NetCap  s@Secret Bytes] [Task Null]]

# Within TLS/crypto stack — the crypto primitives accept Secret key material
chacha20-poly1305-seal: [Fn [key@[Secret [Bytes 32]]  nonce@[Bytes 12]  pt@Bytes  aad@Bytes] Bytes]
#                              ^^^^ crypto primitives are the legitimate consumers of secrets
```

---

## Network Boundary Integration (lib-net-v3)

Every network receive operation returns `Tainted` data. Protocol layers strip taint only after structural validation:

```tinct
# HTTP/1.1 parse-request: validates the wire format structurally
parse-request: [Fn [h@Handle] [Result HttpRequest]]
# HttpRequest fields are clean (not Tainted) because the parser validated structure.
# The BODY is still Tainted (content is user-controlled):
HttpRequest: [type [method: String  path: String  headers: [Map String String]
                    body: [Tainted Bytes]]]

# DNS responses — IP addresses from DNS are tainted (attacker may control DNS)
dns-resolve: [Fn [cap@NetCap host@String type@Symbol] [Seq [Tainted IpAddress]]]
# Caller must sanitize: validate the IP is in the expected range before tcp-connect
```

---

## `Secret` and Crypto Primitives

All crypto primitives are declared to accept `Secret` key material and return `Secret` output where appropriate:

```tinct
sha256:    [Fn [data@Bytes] Bytes]            # input not assumed secret; output not secret
sha256:    [Fn [data@[Secret Bytes]] [Secret [Bytes 32]]]  # secret input → secret output (overloaded)
x25519-dh: [Fn [private@[Secret [Bytes 32]]  peer-public@[Bytes 32]] [Secret [Bytes 32]]]
```

---

## What Would Change

### Type checker

**Proposed:** Track `Tainted` and `Secret` wrappers through type inference. Any expression whose type is derived from a `Tainted t` or `Secret t` value carries the label in its inferred type. Operations that cross security boundaries (db-query, log, write-bytes on a non-encrypted Handle) reject labeled inputs without sanitization/declassification. **Impact:** Major.

### Stdlib

**Proposed:** Annotate all stdlib I/O entry points with `Tainted` return types; annotate crypto primitives with `Secret` parameter/return types; add sanitizer and declassifier functions. **Impact:** Moderate — annotation-only for most functions.

### Evaluator

**Proposed:** No change to the evaluator — IFC is purely a compile-time type system feature. `Tainted` and `Secret` are erased at runtime; the wrapper types have zero overhead. **Impact:** None.

---

## Open Questions

1. **Label combination**: when a value depends on both `Tainted` and `Secret` sources, does it get both labels? A union label `TaintedSecret t` would require intersection types. Simpler: any computation touching a `Secret` becomes `Secret`; touching a `Tainted` becomes `Tainted`; touching both becomes `Tainted Secret` via a parameterized label.

2. **Granularity**: label the whole value (`Tainted [Map String String]`) or label individual fields (`[Map String [Tainted String]]`)? Field-level is more precise but more verbose. Start with value-level.

3. **Implicit vs explicit propagation**: should propagation be automatic (the type checker does it) or explicit (the programmer wraps)? Automatic is safer; explicit gives more control. Automatic through typeclass instances on all stdlib types.

4. **Legacy code and gradual adoption**: all existing code treats everything as unlabeled. Introduce IFC behind a `--strict-ifc` flag initially, make it the default once stdlib annotations are complete.

---

## Prerequisites

- Type system stability (HM + CHR constraints complete) — IFC adds another layer of type annotations
- `LabeledBytes t` and `ByteLabel` typeclass (lib-net-v3) — labels on bytes are the primary use case

---

## References

- Denning, D.E. & Denning, P.J. (1977). "Certification of Programs for Secure Information Flow." *Communications of the ACM* 20(7). — foundational IFC lattice model.
- Myers, A.C. & Liskov, B. (1997). "A Decentralized Model for Information Flow Control." *SOSP '97*. — JFlow/Jif; label polymorphism and principal hierarchies.
- Stefan, D. et al. (2011). "Flexible Dynamic Information Flow Control in Haskell." *Haskell '11*. — LIO monad; runtime IFC in a functional language.
- Russo, A. & Sabelfeld, A. (2010). "Dynamic vs. Static Flow-Sensitive Security Analysis." *CSF '10*. — comparison of approaches; argues for static where possible.
- Duong, T. & Rizzo, J. (2012). "The CRIME Attack." — compression + encryption side-channel; directly motivates `Secret` on HTTP response bodies with compression.
