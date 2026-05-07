# What If: TLS, PKI, and HTTP Protocol Support for tinct (lib-tls)

What would it take to give tinct a complete HTTP and TLS story — plain HTTP GET, CA roots, client certificates, certificate pinning, and protocol negotiation — while keeping the capability model clean and the design appropriate for a configuration language?

## Current State

`doc/whatif/io.md` defines `tls net-cap host port`, which opens a TLS connection via `rustls` with full chain and hostname verification always enabled. It returns a `Value::Handle` — the same opaque byte-stream type as a file or TCP socket. The Handle IS the authenticated channel in the capability model: the TLS handshake completed at call time, so holding the Handle proves the connection was established against a trusted server.

What is not yet specified or implemented:

- **Plain HTTP GET** — `connect` returns a read-only `Handle` (`Box<dyn BufRead>`);
  the write half of the TCP stream is discarded, so there is no way to send an HTTP
  request over the connection. `builtin_write` writes files (DirCap + path), not sockets.
  The `fetch` stub in `stdlib/net.llt` has always been a TODO blocked on this.
- **CA configuration** — no mechanism for custom CA bundles (corporate internal CA, Vault PKI, self-signed dev certs)
- **Mutual TLS** — no way to present a client certificate to a server that requires it
- **Certificate pinning** — no defense against CA compromise or rogue certs for internal services
- **HTTP/2 multiplexing** — the Handle byte-stream model is insufficient for HTTP/2 streams
- **HTTP/3/QUIC** — UDP-based QUIC connections cannot be modeled as a Handle at all
- **TLS identity introspection** — no way to inspect the server's certificate from tinct code

## Why TLS Configuration Matters for tinct

tinct runs at system boundaries: it reads secrets from Vault, validates Kubernetes schemas from cluster APIs, posts webhook payloads to internal services. These scenarios have heterogeneous PKI:

- **Corporate environments** have private CA hierarchies. The browser's Mozilla roots reject them.
- **Container environments** (distroless, Alpine without `ca-certificates`, scratch images) have no system CA store at all.
- **Service-mesh environments** use mTLS everywhere — every service presents a client certificate to prove its identity.
- **CI/CD pipelines** need certificate pinning to resist supply-chain attacks on internal CA infrastructure.

A TLS story that only covers the public web is insufficient. The configuration language that generates infrastructure is exactly where these edge cases matter most.

## Design

### HTTP GET: `http-get` and `https-get`

**The Handle problem.** `connect` wraps the `TcpStream` in a `BufReader`,
discarding the write half. `Value::Handle` is `Box<dyn BufRead>` — read-only.
There is no socket write operation. Redesigning Handle to be bidirectional
touches every consumer (`slurp`, `lines`, `emit`). The correct fix for the HTTP
use case is an **atomic Rust builtin** that owns both halves of the socket
internally and never exposes them as a Handle.

**`http-get`** — plain HTTP/1.0 GET over TCP:

```
http-get : NetCap → String → Int → String → Dict → Int → String
           cap      host     port  path      headers timeout-ms
```

Opens a TCP connection via `TcpStream::connect`, sends an HTTP/1.0 GET request
with the given headers, reads the full response, strips HTTP response headers,
and returns the body as a string. The socket is closed before the builtin
returns. Errors (connection refused, timeout, non-2xx status) are tinct errors.
No new crates — uses `std::net::TcpStream` only.

**`https-get`** — HTTP/1.0 GET over TLS. Mirrors `http-get` with an added
`tls-opts` dict:

```
https-get : NetCap → String → Int → String → Dict → Dict → Int → String
            cap      host     port  path      headers tls-opts timeout-ms
```

`tls-opts` accepts the same keys as the `tls` opts dict (see §`tls` Options Dict):
`ca-bundle`, `system-roots`, `client-cert`, `client-key`, `pin-sha256`, `alpn`.

**`fetch` in `stdlib/net.llt`** wraps both builtins with URL parsing, dispatching
on scheme:

```tinct
fetch: [fn [cap@NetCap url@Str]
  [parsed: [parse-url url]]
  [if [starts-with? "https://" url]
    [https-get cap parsed.host parsed.port parsed.path [] [] 5000]
    [http-get  cap parsed.host parsed.port parsed.path [] 5000]]]
```

`fetch-opts` passes a TLS configuration through for HTTPS:

```tinct
fetch-opts: [fn [cap@NetCap url@Str opts@Dict]
  [parsed:  [parse-url url]]
  [tls-cfg: [dict-select opts ["ca-bundle" "client-cert" "client-key" "pin-sha256"]]]
  [if [starts-with? "https://" url]
    [https-get cap parsed.host parsed.port parsed.path [] tls-cfg 5000]
    [http-get  cap parsed.host parsed.port parsed.path [] 5000]]]
```

**Depends on:** `starts-with?` from `doc/whatif/lib-supplemental.md`
§Extended String Utilities.

### `tls` Options Dict

`tls net-cap host port` keeps its current four-argument form for the common case (connecting to a public HTTPS API with Mozilla-standard roots). An optional fifth argument is a dict of TLS configuration options:

```tinct
# Common case: compiled-in Mozilla roots, no client cert, default ALPN
[h: [tls net "api.example.com" 443]]

# Custom CA bundle for internal PKI
[ca-pem:  [open fs "certs/internal-ca.pem" "r"]]
[h: [tls net "api.internal" 443 [ca-bundle: ca-pem]]]

# Mutual TLS: server authenticates us via our client certificate
[cert: [open fs "certs/client.pem" "r"]]
[key:  [open fs "certs/client-key.pem" "r"]]
[h: [tls net "api.internal" 443 [client-cert: cert  client-key: key]]]

# Certificate pinning
[h: [tls net "api.internal" 443 [
  pin-sha256: ["sha256//AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]
]]]

# Combined: custom CA + mutual TLS + pinning
[h: [tls net "vault.internal" 8200 [
  ca-bundle:    ca-pem
  client-cert:  cert
  client-key:   key
  pin-sha256:   ["sha256//FINGERPRINT1=" "sha256//FINGERPRINT2="]
]]]
```

The options dict keys:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `ca-bundle` | `Handle` | — | PEM file opened via `open`; extends or replaces roots (see below) |
| `system-roots` | `Bool` | `false` | Also trust the OS CA store |
| `client-cert` | `Handle` | — | PEM client certificate (mTLS) |
| `client-key` | `Handle` | — | PEM private key for client cert |
| `pin-sha256` | `Seq[Str]` | — | SPKI SHA-256 fingerprints (base64); leaf cert must match one |
| `alpn` | `Seq[Str]` | `["http/1.1"]` | ALPN protocol list for negotiation |

### CA Root Selection

Three trust sources, combinable:

**Compiled-in Mozilla roots (`webpki-roots`)** — the default. Deterministic and container-safe: the same binary always trusts the same set of public CAs, regardless of whether the container has a CA store installed. This is the correct default for a config tool run in CI and containers.

**Custom CA bundle** — the `ca-bundle` Handle points to a PEM file that has been opened via `open fs path "r"`. By flowing through the `DirCap`, the cert access is auditable: grep `open` to find every place a CA cert is loaded, just as you grep `open` to find every file read. The PEM is consumed by `tls` at connection time; the Handle is drained and closed.

**System roots** — set `system-roots: true` to also include the OS CA store. Useful when deploying on VMs where the system admin manages the CA list, or when the system store includes a corporate root. Container deployments should avoid this: an empty or missing system store is a silent failure mode (rustls-native-certs silently returns zero certs, causing every connection to fail with "no trusted roots").

When `ca-bundle` and `system-roots: true` are both present, both sets of roots are trusted (union). When only `ca-bundle` is present and `system-roots` is omitted (or `false`), only the custom CA is trusted — useful for fully private PKI where public CAs must be excluded.

Default (no options): compiled-in Mozilla roots only.

### Client Certificates and Mutual TLS

In mTLS, the server requires the client to present a certificate during the TLS handshake. This proves the client's identity to the server, complementing the server's certificate that proves the server's identity to the client.

In tinct's capability model, a client certificate is an **identity capability**: it asserts who you are to a remote server. The cert and key are loaded via `open` (gated by `DirCap`) and passed as Handles to `tls`. Two properties follow:

- **Auditable**: loading a client cert requires a `DirCap` for the cert directory. Grep `open` to find all cert loads.
- **Key protection**: the private key flows through the DirCap path and is consumed by `tls` at connection time. The raw key bytes never appear in tinct's value space after that.

`tls` reads both Handles to EOF at call time, configures rustls with the PEM bytes, then closes both Handles. The resulting `Value::Handle` embodies both directions of authentication: server is verified against CA roots; client identity is presented via the cert.

```tinct
[include "stdlib/io.llt"]

# tinct run --cap-fs fs=/etc/service-certs --cap-net net=api.internal script.llt

[cert: [open fs "client.pem" "r"]]
[key:  [open fs "client.key" "r"]]
[ca:   [open fs "ca.pem"     "r"]]

# After this call: conn is an mTLS channel.
# The Handle proves: server verified, our identity presented.
[conn: [tls net "api.internal" 443 [
  ca-bundle:   ca
  client-cert: cert
  client-key:  key
]]]
[slurp [write conn "GET /config HTTP/1.0\r\nHost: api.internal\r\n\r\n"]]
```

### Certificate Pinning

Certificate pinning locks a connection to a specific key or certificate, providing defense against CA compromise (a rogue CA issuing a fraudulent cert for your internal domain) and MITM attacks on internal PKI.

tinct implements **SPKI hash pinning**: the SHA-256 hash of the leaf certificate's Subject Public Key Info (the key material, not the certificate). SPKI pinning survives certificate rotation (as long as the key is reused) and is the form used by HTTP Public Key Pinning (though HPKP itself is formally deprecated in browsers due to site-bricking risk; the underlying SPKI hash technique remains valid for non-browser contexts).

`pin-sha256` is a list of acceptable SPKI hashes in `sha256//base64=` format. The connection proceeds if the server's leaf certificate's SPKI hash matches any entry. `tls` fails with an error if no entry matches.

```tinct
[h: [tls net "vault.internal" 8200 [
  ca-bundle:  ca-pem
  pin-sha256: [
    "sha256//AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
    "sha256//BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB="   # backup pin
  ]
]]]
```

For internal services with a controlled key rotation schedule, maintain two pins (current + next-rotation backup) to allow rotation without a service outage.

### TLS Identity Introspection: `tls-peer-cert`

When a Handle is created by `tls`, the server's identity is verified. `tls-peer-cert handle` exposes the server's leaf certificate as a tinct dict for policy decisions in tinct code:

```tinct
[h:    [tls net "api.internal" 443]]
[cert: [tls-peer-cert h]]

# cert: [
#   subject:    "CN=api.internal,O=Internal Corp"
#   issuer:     "CN=Internal CA,O=Internal Corp"
#   sans:       ["api.internal" "api.internal.corp.net"]
#   not-before: "2025-01-01T00:00:00Z"
#   not-after:  "2026-01-01T00:00:00Z"
#   spki-sha256: "sha256//AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
# ]

# Check cert expiry in config logic
[days-left: [cert-days-remaining cert.not-after]]
[if [< days-left 30]
  [emit "WARNING: cert expires soon"]
  null]
```

`tls-peer-cert` is read-only introspection: it returns metadata from the already-authenticated connection. It does not grant new authority. The Handle remains the capability; the cert dict is derived from information already verified at handshake time.

### ALPN and Protocol Negotiation

ALPN (Application-Layer Protocol Negotiation, RFC 7301) is a TLS extension that lets the client and server agree on the application protocol during the TLS handshake. The `alpn` option is a list of protocol identifiers in preference order:

```tinct
# Prefer HTTP/2 if server supports it; fall back to HTTP/1.1
[h: [tls net "api.example.com" 443 [alpn: ["h2" "http/1.1"]]]]
```

Default (no `alpn` option): `["http/1.1"]`. This is correct for tinct's stdlib `fetch`, which implements HTTP/1.0 over a raw Handle — HTTP/1.1 negotiated with an HTTP/2-only server would produce a protocol error.

### HTTP/2, HTTP/3, and the Handle Boundary

The `Value::Handle` models a single bidirectional byte stream. This matches HTTP/1.x (send a request, read a response, optionally close). It does not match HTTP/2 or HTTP/3:

**HTTP/2 (RFC 7540):** Multiplexes multiple request/response streams over one TLS connection. Each stream is identified by a stream ID; HPACK compresses headers across streams; flow control operates at both the connection and stream levels. Implementing HTTP/2 correctly inside a tinct `Handle` is not possible — the framing layer requires stateful parsing that belongs in Rust, not in a lazy configuration language.

**HTTP/3 (RFC 9114):** Runs over QUIC (RFC 9000), which is UDP-based. QUIC provides its own stream multiplexing, packet ordering, and congestion control. A QUIC connection contains up to 2^62 independent streams. This is fundamentally not a byte stream — `Value::Handle` cannot model a QUIC connection.

The correct abstraction boundary for tinct: **Handle = one byte stream; HTTP/2 and HTTP/3 are protocol engines, not byte streams.** For HTTP/2 and HTTP/3 from tinct code, the right answer is a Rust-level `fetch` builtin that returns a response dict, hiding protocol selection, connection pooling, and stream multiplexing entirely.

```tinct
# Rust-level fetch with HTTP/1.1 + HTTP/2 + HTTP/3 via reqwest
# Protocol selected by ALPN; connection pooling managed by Rust
[resp: [fetch net "https://api.example.com/config"]]
# resp: [status: 200  headers: [...]  body: "..."]
```

This keeps the tinct surface simple and protocol-agnostic. The underlying implementation uses `reqwest` (which wraps hyper + h3 + QUIC via quinn) and the same NetCap allowlist for all requests.

### The `fetch` Boundary: stdlib vs Rust

The two-tier model:

| Tier | Function | Protocol | Implementation |
|------|----------|----------|----------------|
| Low-level | `connect`, `tls` | TCP, TLS | Rust (builtins) |
| Mid-level | `http-get` | HTTP/1.0 plain | Rust (atomic builtin) |
| Mid-level | `https-get` | HTTP/1.0 over TLS | Rust (atomic builtin) |
| Mid-level | `fetch` (stdlib) | HTTP/1.0, scheme dispatch | tinct (stdlib/net.llt) |
| High-level | `fetch` (Rust) | HTTP/1.1+/HTTP/2/HTTP/3 | Rust (builtin, replaces stdlib version) |

The stdlib `fetch` (HTTP/1.0, Connection: close, no redirect following) is the
initial implementation. The Rust `fetch` builtin shadows the stdlib version,
adding HTTP/2 and HTTP/3 support transparently. User code does not change.

## What Would Change

### Rust Builtins (`src/builtins.rs`)

**`http-get cap host port path headers timeout-ms`** — new builtin. Plain HTTP/1.0
GET over TCP. Returns body string. No new crates.

**`https-get cap host port path headers tls-opts timeout-ms`** — new builtin.
HTTP/1.0 GET over TLS with full options dict support.

**`tls net-cap host port`** — already specified in io.md. Extended to accept
optional fifth argument (TLS opts dict).

**`tls-peer-cert handle`** — new builtin. Reads TLS peer certificate info from a
Handle created by `tls`. Returns dict with `subject`, `issuer`, `sans`,
`not-before`, `not-after`, `spki-sha256`. Returns `Null` for non-TLS Handles.

**`fetch net-cap url`** — Rust-level HTTP client using `reqwest`. Replaces the
tinct-code `fetch` in `stdlib/net.llt`. Returns
`[status: Int  headers: Dict  body: Str]`. Handles HTTP/1.1 keepalive, HTTP/2
multiplexing, and HTTP/3 via ALPN negotiation and QUIC. Accepts optional opts
dict: `[tls: TLS-opts  follow-redirects: Bool  headers: Dict  method: Str  body: Str]`.

### Evaluator (`src/eval.rs`)

**`Value::Handle` extended:** Handles from `tls` carry an `Option<TlsInfo>` (leaf
cert metadata, negotiated ALPN protocol). `tls-peer-cert` reads this field.
Handles from `connect` or `open` have `None` and `tls-peer-cert` returns `Null`.

**`Value::TlsConfig`:** Transient value type for the opts dict parsed by `tls`.
Never exposed to tinct code — consumed internally during connection setup.

### Dependencies (`Cargo.toml`)

| Crate | Purpose | Required by |
|-------|---------|-------------|
| `rustls = "0.23"` | TLS engine | `tls`, `https-get` |
| `rustls-native-certs = "0.7"` | System root loading (`system-roots: true`) | `tls`, `https-get` |
| `webpki-roots = "0.26"` | Compiled-in Mozilla roots (default) | `tls`, `https-get` |
| `reqwest = { version = "0.12", features = ["http2", "brotli"] }` | High-level HTTP with HTTP/2 | Rust `fetch` |
| `reqwest = { ..., features = ["http3"] }` | HTTP/3 via QUIC | Rust `fetch` (HTTP/3) |

`rustls` and `webpki-roots` are already required by io.md. This document
extends the feature set on top of the same dependencies.

### stdlib (`stdlib/net.llt`)

`fetch` and `fetch-opts` implemented in tinct over `http-get`/`https-get`.
When the Rust `fetch` builtin lands, `stdlib/net.llt` becomes a thin
compatibility wrapper — user-visible behavior is unchanged.

### Type Checker (`src/typecheck.rs`)

TLS opts dict infers as `Any`. The type checker does not validate dict keys.
Future work: `Type::Handle` with a `tls: bool` tag so `tls-peer-cert` is a
type error on non-TLS Handles at the static level.

## Dependencies

- `http-get` has no dependencies beyond the existing `NetCap` infrastructure
  from io.md.
- `https-get` and `tls` opts dict require `rustls`, `webpki-roots`, and
  `rustls-native-certs` in `Cargo.toml`, and io.md network caps complete.
- `fetch` (stdlib) requires `http-get` and `https-get`, and `starts-with?`
  from `doc/whatif/lib-supplemental.md` §Extended String Utilities.
- Rust `fetch` requires `reqwest = "0.12"` with `http2` feature; HTTP/3
  additionally requires the `http3` feature.

## References

- RFC 8446. "The Transport Layer Security (TLS) Protocol Version 1.3." IETF, 2018. — TLS 1.3 specification; rustls implements this as its primary target.
- RFC 7301. Friedl, S., Popov, A., Langley, A. & Stephan, E. (2014). "Transport Layer Security (TLS) Application-Layer Protocol Negotiation Extension." IETF. — ALPN; the mechanism tinct uses to negotiate `h2` vs `http/1.1`.
- RFC 9000. Iyengar, J. & Thomson, M. (2021). "QUIC: A UDP-Based Multiplexed and Secure Transport." IETF. — QUIC transport specification; foundation for HTTP/3. Explains why QUIC connections cannot be modeled as byte-stream Handles.
- RFC 9114. Bishop, M. (2022). "HTTP/3." IETF. — HTTP/3 over QUIC. Defines the stream model that makes a Handle abstraction insufficient.
- RFC 7469. Evans, C., Palmer, C. & Sleevi, R. (2015). "Public Key Pinning Extension for HTTP." IETF. — HPKP specification (deprecated for browsers). The SPKI hash technique remains valid for non-browser config tooling.
- Thomson, M. & Stradling, R. (eds.). "rustls" crate documentation. — tinct's TLS engine. Provides `ClientConfig` builder, custom cert verifiers, and `CertificateVerificationMode` for pinning.
- Cooper, D., Santesson, S., Farrell, S., Boeyen, S., Housley, R. & Polk, W. (2008). RFC 5280. "Internet X.509 Public Key Infrastructure Certificate and Certificate Revocation List (CRL) Profile." IETF. — X.509 certificate structure; defines SPKI, SANs, validity period, and issuer fields exposed by `tls-peer-cert`.
- Rescorla, E. (2018). "TLS 1.3 and the Decline of Centralized PKI." — Background on why SPKI pinning is preferred over HPKP for internal service environments.
- Miller, M.S. (2006). *Robust Composition*. PhD thesis, Johns Hopkins University. — Capability model. mTLS client certificates as identity capabilities; cert+key Handles as auditable capability flows. Connects TLS identity to the ocap model in §Design.
- Dennis, J.B. & Van Horn, E.C. (1966). "Programming semantics for multiprogrammed computations." *Communications of the ACM*, 9(3), 143–155. — Handle-as-capability: the TLS Handle returned by `tls` is a capability in the Dennis/Van Horn sense; it embodies both connection authority and server identity verification.
