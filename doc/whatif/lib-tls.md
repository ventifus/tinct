# What If: TLS, PKI, and HTTP Protocol Support for tinct (lib-tls)

What would it take to give tinct a complete HTTP and TLS story — CA roots,
client certificates, certificate pinning, and protocol negotiation — while
keeping the capability model clean and the design appropriate for a
configuration language?

## Current State

`doc/whatif/io.md` defines `tls-connect net-cap host port`, which opens a TLS
connection via `rustls` with full chain and hostname verification always
enabled. It returns a `Value::Handle` — the same opaque byte-stream type
as a file or TCP socket. The Handle IS the authenticated channel in the
capability model: the TLS handshake completed at call time, so holding
the Handle proves the connection was established against a trusted server.

What is not yet specified or implemented:

- **CA configuration** — no mechanism for custom CA bundles (corporate
  internal CA, Vault PKI, self-signed dev certs)
- **Mutual TLS** — no way to present a client certificate to a server
  that requires it
- **Certificate pinning** — no defense against CA compromise or rogue
  certs for internal services
- **HTTP/2 multiplexing** — the Handle byte-stream model is insufficient
  for HTTP/2 streams
- **HTTP/3/QUIC** — UDP-based QUIC connections cannot be modeled as a
  Handle at all
- **TLS identity introspection** — no way to inspect the server's
  certificate from tinct code

## Why TLS Configuration Matters for tinct

tinct runs at system boundaries: it reads secrets from Vault, validates
Kubernetes schemas from cluster APIs, posts webhook payloads to internal
services. These scenarios have heterogeneous PKI:

- **Corporate environments** have private CA hierarchies. The browser's
  Mozilla roots reject them.
- **Container environments** (distroless, Alpine without `ca-certificates`,
  scratch images) have no system CA store at all.
- **Service-mesh environments** use mTLS everywhere — every service
  presents a client certificate to prove its identity.
- **CI/CD pipelines** need certificate pinning to resist supply-chain
  attacks on internal CA infrastructure.

A TLS story that only covers the public web is insufficient. The
configuration language that generates infrastructure is exactly where
these edge cases matter most.

## Design

### Handle Types for Network Connections

Following the capability-typed handle model from
`doc/whatif/lib-supplemental.md` §Streaming File I/O:

```
connect cap host port        → Handle[Binary Readable Writable]
tls     cap host port ...    → Handle[Binary Readable Writable Tls]
```

Network handles carry `Readable` and `Writable` — TCP is bidirectional.
They do not carry `Seekable` — streams are sequential. `connect` handles
are plain TCP; `tls-connect` handles carry an additional `Tls` capability that
gates `tls-peer-cert`.

With `Handle[Binary Readable Writable]`, HTTP can be implemented in
pure-tinct `stdlib/net.llt` without any Rust-level `http-get` builtin:

```tinct
# stdlib/net.llt — pure-tinct HTTP/1.0 over a Readable Writable handle
http-get: [fn [cap@NetCap host@String port@Int path@String headers@Dict]
  [conn: [connect cap host port]]
  [req:  [build-http-request "GET" path headers]]
  [write conn [str-bytes req]]
  [parse-http-response [slurp conn]]]
```

`https-get` is identical but uses `tls-connect` instead of `connect`:

```tinct
https-get: [fn [cap@NetCap host@String port@Int path@String headers@Dict tls-opts@Dict]
  [conn: [tls-connect cap host port tls-opts]]
  [req:  [build-http-request "GET" path headers]]
  [write conn [str-bytes req]]
  [parse-http-response [slurp conn]]]
```

`fetch` in `stdlib/net.llt` wraps both with URL parsing:

```tinct
fetch: [fn [cap@NetCap url@String]
  [parsed: [parse-url url]]
  [if [starts-with? "https://" url]
    [https-get cap parsed.host parsed.port parsed.path [] []]
    [http-get  cap parsed.host parsed.port parsed.path []]]]
```

For HTTP/2 and HTTP/3, the Handle byte-stream model is insufficient
(see §HTTP/2, HTTP/3, and the Handle Boundary). Those protocols require
a Rust-level `fetch` builtin using `reqwest`.

### `tls-connect` Options Dict

`tls-connect net-cap host port` accepts an optional fifth argument — a dict of
TLS configuration options:

```tinct
# Common case: compiled-in Mozilla roots, no client cert, default ALPN
[h: [tls-connect net "api.example.com" 443]]

# Custom CA bundle for internal PKI
[ca-pem: [open fs "certs/internal-ca.pem" Readable]]
[h: [tls-connect net "api.internal" 443 [ca-bundle: ca-pem]]]

# Mutual TLS: server authenticates us via our client certificate
[cert: [open fs "certs/client.pem" Readable]]
[key:  [open fs "certs/client-key.pem" Readable]]
[h: [tls-connect net "api.internal" 443 [client-cert: cert  client-key: key]]]

# Certificate pinning
[h: [tls-connect net "api.internal" 443 [
  pin-sha256: ["sha256//AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]
]]]

# Combined: custom CA + mutual TLS + pinning
[h: [tls-connect net "vault.internal" 8200 [
  ca-bundle:    ca-pem
  client-cert:  cert
  client-key:   key
  pin-sha256:   ["sha256//FINGERPRINT1=" "sha256//FINGERPRINT2="]
]]]
```

The options dict keys:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `ca-bundle` | `Handle[Text Readable ...]` | — | PEM file via `[open cap path]`; extends or replaces roots |
| `system-roots` | `Bool` | `false` | Also trust the OS CA store |
| `client-cert` | `Handle[Text Readable ...]` | — | PEM client certificate (mTLS) |
| `client-key` | `Handle[Text Readable ...]` | — | PEM private key for client cert |
| `pin-sha256` | `Seq[String]` | — | SPKI SHA-256 fingerprints (base64); leaf cert must match one |
| `alpn` | `Seq[String]` | `["http/1.1"]` | ALPN protocol list for negotiation |

### CA Root Selection

Three trust sources, combinable:

**Compiled-in Mozilla roots (`webpki-roots`)** — the default.
Deterministic and container-safe: the same binary always trusts the
same set of public CAs, regardless of whether the container has a CA
store installed. The correct default for a config tool run in CI and
containers.

**Custom CA bundle** — the `ca-bundle` Handle points to a PEM file
opened via `open`. Cert access flows through `DirCap` and is
auditable. The PEM is consumed by `tls-connect` at connection time; the Handle
is drained and closed.

**System roots** — `system-roots: true` also includes the OS CA store.
Container deployments should avoid this: an empty or missing system
store silently produces zero certs, causing every connection to fail.

When `ca-bundle` and `system-roots: true` are both present, both trust
sets are unioned. When only `ca-bundle` is present, only the custom CA
is trusted — useful for fully private PKI where public CAs must be
excluded.

### Client Certificates and Mutual TLS

In mTLS, the server requires a client certificate during the TLS
handshake. In tinct's capability model, a client certificate is an
**identity capability**: it asserts who you are to a remote server.
Cert and key are loaded via `open` (gated by `DirCap`) and passed
as Handles to `tls-connect`.

- **Auditable**: loading a client cert requires a `DirCap`. Grep
  `open` to find all cert loads.
- **Key protection**: the private key flows through the DirCap path and
  is consumed by `tls-connect` at connection time. Raw key bytes never appear
  in tinct's value space after that.

```tinct
[include "stdlib/io.llt"]

# tinct run --cap-fs fs=/etc/service-certs --cap-net net=api.internal script.llt

[cert: [open fs "client.pem" Readable]]
[key:  [open fs "client.key" Readable]]
[ca:   [open fs "ca.pem" Readable]]

[conn: [tls-connect net "api.internal" 443 [
  ca-bundle:   ca
  client-cert: cert
  client-key:  key
]]]
# conn : Handle[Binary Readable Writable Tls]
[write conn [str-bytes "GET /config HTTP/1.0\r\nHost: api.internal\r\n\r\n"]]
[response: [slurp conn]]
```

### Certificate Pinning

tinct implements **SPKI hash pinning**: the SHA-256 hash of the leaf
certificate's Subject Public Key Info. SPKI pinning survives certificate
rotation (as long as the key is reused) and is the form used by HTTP
Public Key Pinning (HPKP itself is deprecated for browsers, but the
underlying SPKI hash technique remains valid for non-browser contexts).

`pin-sha256` is a list of acceptable SPKI hashes in `sha256//base64=`
format. The connection proceeds if the leaf's SPKI hash matches any
entry; `tls-connect` fails with an error otherwise.

```tinct
[h: [tls-connect net "vault.internal" 8200 [
  ca-bundle:  ca-pem
  pin-sha256: [
    "sha256//AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
    "sha256//BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB="   # backup pin
  ]
]]]
```

Maintain two pins (current + next-rotation backup) to allow key rotation
without a service outage.

### TLS Identity Introspection: `tls-peer-cert`

`tls-peer-cert` requires `Handle[... Tls ...]` — the `Tls` capability
is only present on handles created by `tls-connect`, not by `connect`. The type
system prevents calling `tls-peer-cert` on a plain TCP handle.

```tinct
[h:    [tls-connect net "api.internal" 443]]
[cert: [tls-peer-cert h]]

# cert: [
#   subject:     "CN=api.internal,O=Internal Corp"
#   issuer:      "CN=Internal CA,O=Internal Corp"
#   sans:        ["api.internal" "api.internal.corp.net"]
#   not-before:  "2025-01-01T00:00:00Z"
#   not-after:   "2026-01-01T00:00:00Z"
#   spki-sha256: "sha256//AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
# ]

[days-left: [days-between [parse-timestamp cert.not-after] [now clock]]]
[if [< days-left 30]
  [emit [str "WARNING: cert expires in " days-left " days"]]
  null]
```

`tls-peer-cert` is read-only introspection — derived from information
already verified at handshake time. It does not grant new authority.

### ALPN and Protocol Negotiation

```tinct
# Prefer HTTP/2, fall back to HTTP/1.1
[h: [tls-connect net "api.example.com" 443 [alpn: ["h2" "http/1.1"]]]]
```

Default: `["http/1.1"]`. Correct for stdlib `fetch` (HTTP/1.0 over
Handle). Negotiating `h2` with an HTTP/1.0 client produces a protocol
error — set `alpn` explicitly when using the high-level Rust `fetch`.

### HTTP/2, HTTP/3, and the Handle Boundary

`Handle[Binary Readable Writable]` models one bidirectional byte stream.
This covers HTTP/1.x. It cannot cover HTTP/2 or HTTP/3:

**HTTP/2 (RFC 7540):** Multiplexes request/response streams over one
TLS connection using stream IDs, HPACK header compression, and
per-connection flow control. The framing layer requires stateful parsing
in Rust.

**HTTP/3 (RFC 9114):** Runs over QUIC (RFC 9000), which is UDP-based.
QUIC is not a byte stream — `Handle` cannot model a QUIC connection.

For HTTP/2 and HTTP/3, the right answer is a Rust-level `fetch` builtin
returning a response dict:

```tinct
# High-level Rust fetch — HTTP/1.1+/HTTP/2/HTTP/3 via reqwest
[resp: [fetch net "https://api.example.com/config"]]
# resp: [status: 200  headers: [...]  body: "..."]
```

### The `fetch` Boundary: stdlib vs Rust

| Tier | Function | Protocol | Implementation |
|------|----------|----------|----------------|
| Low-level | `connect` | TCP | Rust builtin → `Handle[Binary Readable Writable]` |
| Low-level | `tls-connect` | TLS | Rust builtin → `Handle[Binary Readable Writable Tls]` |
| Mid-level | `http-get` | HTTP/1.0 plain | **pure-tinct** (stdlib/net.llt) |
| Mid-level | `https-get` | HTTP/1.0 over TLS | **pure-tinct** (stdlib/net.llt) |
| Mid-level | `fetch` (stdlib) | HTTP/1.0, scheme dispatch | pure-tinct (stdlib/net.llt) |
| High-level | `fetch` (Rust) | HTTP/1.1+/HTTP/2/HTTP/3 | Rust builtin (reqwest), replaces stdlib version |

`http-get` and `https-get` are pure-tinct because the capability-typed
`Handle[Binary Readable Writable]` from `connect`/`tls-connect` supports both
reads and writes. No Rust builtin needed for HTTP/1.0.

## What Would Change

### Rust Builtins (`src/builtins.rs`)

**`connect cap host port`** — returns `Handle[Binary Readable Writable]`
instead of the current read-only `Handle`. No other API change.

**`tls-connect net-cap host port`** — extended to accept optional fifth argument
(TLS opts dict). Returns `Handle[Binary Readable Writable Tls]`. The
`Tls` capability tag carries leaf cert metadata and negotiated ALPN
protocol for `tls-peer-cert`.

**`tls-peer-cert handle`** — new builtin. Requires
`Handle[... Tls ...]`. Returns dict with `subject`, `issuer`, `sans`,
`not-before`, `not-after`, `spki-sha256`. Type error on non-Tls handles.

**`fetch net-cap url`** — Rust-level HTTP client using `reqwest`.
Replaces `fetch` in `stdlib/net.llt` for HTTP/2+ support. Returns
`[status: Int  headers: Dict  body: String]`. Accepts optional opts
dict: `[tls: TLS-opts  follow-redirects: Bool  headers: Dict  method: String  body: String]`.

### Handle Type (`src/value.rs`)

`Value::Handle` gains a capability row per
`doc/whatif/lib-supplemental.md` §Streaming File I/O. `connect` sets
`{Binary Readable Writable}`; `tls-connect` sets `{Binary Readable Writable Tls}`.
The `Tls` tag carries `Option<TlsInfo>` (leaf cert + negotiated ALPN).

### stdlib (`stdlib/net.llt`)

`http-get`, `https-get`, `fetch`, `fetch-opts` are pure-tinct
implementations over the capability-typed handles. When the Rust `fetch`
builtin lands, `stdlib/net.llt` becomes a thin compatibility wrapper.

### Dependencies (`Cargo.toml`)

| Crate | Purpose |
|-------|---------|
| `rustls = "0.23"` | TLS engine |
| `rustls-native-certs = "0.7"` | System root loading (`system-roots: true`) |
| `webpki-roots = "0.26"` | Compiled-in Mozilla roots (default) |
| `reqwest = { version = "0.12", features = ["http2", "brotli"] }` | High-level HTTP/2 |
| `reqwest = { ..., features = ["http3"] }` | HTTP/3 via QUIC |

`rustls` and `webpki-roots` are already required by io.md.

### Type Checker (`src/typecheck.rs`)

`Handle` gains a capability row parameter. `tls-peer-cert` is typed as
`Handle[Tls | r] → Dict` — a type error on plain TCP handles. TLS opts
dict infers as `Any` for now; future work can add field-level validation.

## Dependencies

- Capability-typed Handle (§Streaming File I/O, lib-supplemental.md)
  is a prerequisite — `connect` and `tls-connect` must return typed handles
  before `http-get`/`https-get` can be pure-tinct.
- `starts-with?` from lib-supplemental.md §Extended String Utilities
  (used in `fetch` scheme dispatch).
- `tls-connect`, `tls-peer-cert` opts dict require `rustls`, `webpki-roots`,
  `rustls-native-certs` in `Cargo.toml`.
- Rust `fetch` requires `reqwest = "0.12"` with `http2`; HTTP/3
  requires `http3` feature.

## References

- RFC 8446. "The Transport Layer Security (TLS) Protocol Version 1.3." IETF, 2018.
- RFC 7301. Friedl et al. (2014). "TLS Application-Layer Protocol Negotiation Extension." IETF.
- RFC 9000. Iyengar & Thomson (2021). "QUIC: A UDP-Based Multiplexed and Secure Transport." IETF.
- RFC 9114. Bishop (2022). "HTTP/3." IETF.
- RFC 7469. Evans et al. (2015). "Public Key Pinning Extension for HTTP." IETF.
- Thomson & Stradling. "rustls" crate documentation.
- Cooper et al. (2008). RFC 5280. "Internet X.509 PKI Certificate and CRL Profile." IETF.
- Rescorla (2018). "TLS 1.3 and the Decline of Centralized PKI."
- Miller (2006). *Robust Composition*. — mTLS client certs as identity capabilities.
- Dennis & Van Horn (1966). "Programming semantics for multiprogrammed computations." *CACM*.
