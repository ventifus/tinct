# TLS, PKI, and HTTP Protocol Support (lib-tls)

## Overview

tinct provides a complete HTTP and TLS story — CA roots, client
certificates, certificate pinning, and protocol negotiation — while
keeping the capability model clean.

tinct runs at system boundaries: it reads secrets from Vault, validates
Kubernetes schemas from cluster APIs, posts webhook payloads to internal
services. These scenarios have heterogeneous PKI: corporate environments
have private CA hierarchies that browser roots reject; container
environments (distroless, Alpine without `ca-certificates`, scratch
images) have no system CA store at all; service-mesh environments use
mTLS everywhere; CI/CD pipelines need certificate pinning to resist
supply-chain attacks. A TLS story that only covers the public web is
insufficient. The configuration language that generates infrastructure is
exactly where these edge cases matter most.

`connect` returns a bidirectional `Handle@[Binary Readable Writable Stream]`.
`tls-connect` returns `Handle@[Binary Readable Writable Stream Tls]`. The
Handle IS the authenticated channel: the TLS handshake completes at call
time, so holding a `Tls`-capable Handle proves the connection was
established against a trusted server.

## Design

### The Connector Protocol

A **Connector** is any value that opens connections to remote endpoints.
`NetCap` is the stdlib implementation; user-written transports (WireGuard
clients, custom resolvers, test fakes) implement the same protocol and
substitute anywhere a `NetCap` is accepted.

**Protocol method:**

```
[connect connector Transport host port opts] → Handle@[... Stream|Datagram ...]
```

`Transport` is a nominal unit variant specifying the transport
semantics. The stdlib provides two; users define others:

| Variant | Semantics | Handle capability | Notes |
|---------|-----------|-------------------|-------|
| `Tcp` | reliable byte stream | `Stream` | default when unspecified |
| `Udp` | unreliable datagrams | `Datagram` | |
| *(user-defined)* | anything | user-specified | `Sctp`, `UdpLite`, `Quic`, … |

**`NetCap` is a built-in Connector** that implements `Tcp` and `Udp`
via OS sockets. `connect` (the existing builtin) becomes:

```tinct
# Explicit transport:
[connect net Tcp "api.example.com" 443]   # → Handle@[Binary Readable Writable Stream]
[connect net Udp "8.8.8.8" 53]            # → Handle@[Binary Readable Writable Datagram]

# Tcp is the default when Transport is omitted:
[connect net "api.example.com" 443]       # same as Tcp form
```

**User-defined Connector (e.g. WireGuard client):**

```tinct
[wg: [wg-connect wg-cap config]]   # → WgConnector

WgConnector: [
  connect: [fn [transport host port opts]
    [match transport
      Tcp: [wg-open-tcp  host port opts]
      Udp: [wg-open-udp  host port opts]
      _:   [error [str "unsupported transport: " [tag-of transport]]]]]]

# Use it anywhere NetCap was accepted:
[tls-connect wg Tcp "api.example.com" 443 tls-opts]
[http-connect wg "api.example.com" 443 []]
```

### Handle Types for Network Connections

```
connect     connector Tcp  host port      → Handle@[Binary Readable Writable Stream]
connect     connector Udp  host port      → Handle@[Binary Readable Writable Datagram]
tls-connect connector Tcp  host port opts → Handle@[Binary Readable Writable Stream Tls]
tls-connect h@[Handle [...Stream RW...]] sni opts → Handle@[Binary Readable Writable Stream Tls]
```

**Two forms for `tls-connect`:**

```tinct
# Connector form — opens TCP connection and layers TLS:
[tls-connect wg Tcp "api.example.com" 443 opts]

# Handle form — layers TLS on an existing stream Handle:
[tcp: [connect net Tcp "10.0.0.5" 443]]          # connect to specific IP
[tls: [tls-connect tcp "api.example.com" opts]]   # TLS with SNI for domain
```

The SNI hostname must always be provided explicitly (it may differ from
the IP actually connected to, e.g. when bypassing DNS or using a proxy).

With `Handle@[Binary Readable Writable Stream]`, HTTP/1.0 is pure-tinct.
`http-get` handles both `http://` and `https://` by dispatching on
`url.scheme`; `https-get` does not exist as a separate function:

```tinct
# stdlib/net.llt — HTTP/1.0 over any Readable Writable Stream Handle
http-get: [fn [connector@Connector url@Url headers@Dict tls-opts@[TlsOpts Null]]
  [conn: [match url.scheme
    "https": [tls-connect connector Tcp url.host url.port [tls-opts or []]]
    "http":  [connect connector Tcp url.host url.port]
    _:       [error [str "http-get: unsupported scheme: " url.scheme]]]]
  [write conn [str-bytes [build-http-request "GET" url.path headers]]]
  [parse-http-response [slurp conn]]]

fetch: [fn [connector@Connector url@Url]
  [http-get connector url [] null]]
```

### `HttpConn` and Connection Reuse

A single-shot `http-get` creates and closes a connection per request.
For connection reuse (required for HTTP/2 multiplexing), use
`http-connect` to obtain an `HttpConn` value:

```
http-connect connector host port opts → HttpConn
```

`HttpConn` wraps a persistent connection (or connection pool). Multiple
requests over the same `HttpConn` reuse the underlying HTTP/2 (or HTTP/3)
connection:

```tinct
# All three requests share one HTTP/2 connection:
[client: [http-connect wg "api.example.com" 443 []]]
[users:  [http-get  client "/v1/users"  []]]
[posts:  [http-get  client "/v1/posts"  []]]
[config: [http-get  client "/v1/config" []]]
```

**Two forms for `http-connect`:**

```tinct
# Connector form — http-connect picks the transport:
[client: [http-connect wg "api.example.com" 443 []]]

# Handle form — use an existing TLS stream:
[tcp: [connect net Tcp "10.0.0.5" 443]]
[tls: [tls-connect tcp "api.example.com" opts]]
[client: [http-connect tls "api.example.com"]]
```

**Full composability example:**

```tinct
# SOCKS5 proxy → TLS → HTTP/2
[proxy:    [connect net Tcp "proxy.internal" 1080]]
[tunneled: [socks5-connect proxy "api.example.com" 443 creds]]
[tls:      [tls-connect tunneled "api.example.com" opts]]
[client:   [http-connect tls "api.example.com"]]

# WireGuard → everything through the tunnel:
[wg:     [wg-connect wg-cap config]]
[client: [http-connect wg "api.example.com" 443 []]]
```

### HTTP/2, HTTP/3, and the Handle Boundary

`http-connect` picks the appropriate transport internally:

- For HTTP/1.1 / HTTP/2: asks the Connector for `Tcp`, layers TLS,
  negotiates via ALPN (`h2` vs `http/1.1`)
- For HTTP/3: asks the Connector for `Udp`, runs QUIC internally
  (QUIC is not a raw `Handle` — it multiplexes streams and is
  implemented in Rust via `quinn`/`reqwest`)

HTTP/3 over a user Connector (e.g. WireGuard): `http-connect wg ...`
asks `wg` for a `Udp` Handle, then runs QUIC over it. The WireGuard
client only needs to implement `connect wg Udp host port` — QUIC and
HTTP/3 are handled by `http-connect` internally.

### `tls-connect` Options Dict

`tls-connect net-cap host port` accepts an optional fifth argument — a dict of
TLS configuration options:

```tinct
# Common case: system roots (default), no client cert, default ALPN
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
  pins: [[spki-pin Sha256 [base64-decode "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]]]
]]]

# Combined: custom CA + mutual TLS + pinning
[h: [tls-connect net "vault.internal" 8200 [
  ca-bundle:    ca-pem
  client-cert:  cert
  client-key:   key
  pins: [
    [spki-pin Sha3-256 current-key-hash]
    [spki-pin Sha256   current-key-sha256]    # compatibility
    [spki-pin Sha3-256 next-rotation-hash]    # backup pin
  ]
]]]
```

The options dict keys:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `ca-bundle` | `Handle@[Text Readable ...]` | — | PEM file via `[open cap path Readable]`; added to system roots |
| `no-system-roots` | `Bool` | `false` | Drop system roots — use only `ca-bundle` (private PKI) |
| `mozilla-roots` | `Bool` | `false` | Also load compiled-in Mozilla roots (`webpki-roots` opt-in) |
| `client-cert` | `Handle@[Text Readable ...]` | — | PEM client certificate (mTLS) |
| `client-key` | `Handle@[Text Readable ...]` | — | PEM private key for client cert |
| `pins` | `@[Seq SpkiPin]` | — | SPKI fingerprints; leaf cert must match one. See §SPKI Pins. |
| `alpn` | `[Seq String]` | `["http/1.1"]` | ALPN protocol list for negotiation |

### CA Root Selection

**System roots — the default.** `rustls-native-certs` reads the OS
certificate store at connection time (Linux: `/etc/ssl/certs`; macOS:
Keychain; Windows: Certificate Store).

**Custom CA bundle** — `ca-bundle` points to a PEM file opened via
`[open cap path Readable]`. Cert access flows through `DirCap` and is
auditable. The PEM is added to system roots by default; set
`no-system-roots: true` to trust only the custom CA:

```tinct
[tls-connect net "vault.internal" 8200 [
  ca-bundle:       ca-pem
  no-system-roots: true    # trust only our internal CA
]]
```

**Compiled-in Mozilla roots** — opt-in via `mozilla-roots: true`.
Useful in containers with no system CA store and no custom CA bundle.

All three trust sources union when combined.

### Client Certificates and Mutual TLS

In mTLS, the server requires a client certificate during the TLS
handshake. A client certificate is an **identity capability**: it asserts
who you are to a remote server. Cert and key are loaded via `open`
(gated by `DirCap`) and passed as Handles to `tls-connect`.

Loading a client cert requires a `DirCap` — grep `open` to find all
cert loads. The private key flows through the DirCap path and is consumed
by `tls-connect` at connection time. Raw key bytes never appear in
tinct's value space after that.

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
# conn : Handle@[Binary Readable Writable Tls]
[write conn [str-bytes "GET /config HTTP/1.0\r\nHost: api.internal\r\n\r\n"]]
[response: [slurp conn]]
```

### SPKI Pins

SPKI (Subject Public Key Info) hash pinning locks a connection to a
specific public key, providing defence against CA compromise. Pinning
survives certificate rotation as long as the key is reused.

**`SpkiPin`** is a strongly-typed value carrying the hash algorithm and
the raw fingerprint bytes. The algorithm is a `HashAlgorithm` nominal
variant (defined in `doc/feature/lib-supplemental.md` §Bitwise Primitives):

```tinct
[
  SpkiPin: [type [
    algorithm:   @HashAlgorithm   # Sha3-256 | Sha256 | Sha384 | Sha512 | ...
    fingerprint: @Bytes           # raw hash bytes (not base64 string)
  ]]
]

# Constructor:
spki-pin: [fn@SpkiPin [algorithm@HashAlgorithm  fingerprint@Bytes]]
```

**Post-quantum preferred.** SHA-3 (Keccak construction) is recommended
for new deployments. SHA-2 is accepted for compatibility with existing
tooling that generates SHA-256 fingerprints:

```tinct
# Preferred: SHA3-256 fingerprint (post-quantum preferred)
[spki-pin Sha3-256 [hex-decode "aabbcc..."]]

# Compatibility: SHA-256 (existing tooling)
[spki-pin Sha256 [base64-decode "AAAA...="]]
```

**Usage in `tls-connect`:**

```tinct
[h: [tls-connect net "vault.internal" 8200 [
  ca-bundle: ca-pem
  pins: [
    [spki-pin Sha3-256 current-key-sha3-256]   # preferred
    [spki-pin Sha256   current-key-sha256]      # compatibility
    [spki-pin Sha3-256 next-rotation-sha3-256]  # backup pin
  ]
]]]
```

Maintain current + next-rotation pins to allow key rotation without
a service outage. The connection proceeds if the leaf's SPKI matches
any pin in the list using the pin's specified algorithm;
`tls-connect` fails with an error if no pin matches.

### TLS Identity Introspection: `tls-peer-cert`

`tls-peer-cert` requires `Handle@[... Tls ...]` — the `Tls` capability
is only present on handles created by `tls-connect`. The type system
prevents calling `tls-peer-cert` on a plain TCP handle.

```tinct
[h:    [tls-connect net "api.internal" 443]]
[cert: [tls-peer-cert h]]

# cert: [
#   subject:     "CN=api.internal,O=Internal Corp"
#   issuer:      "CN=Internal CA,O=Internal Corp"
#   sans:        ["api.internal" "api.internal.corp.net"]
#   not-before:  <Timestamp>
#   not-after:   <Timestamp>
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

Default: `["http/1.1"]`. Negotiating `h2` with an HTTP/1.0 client
produces a protocol error — set `alpn` explicitly when using the
high-level Rust `fetch`.

### Network Stack Summary

| Tier | Abstraction | Implementation |
|------|-------------|----------------|
| Transport | `connect connector Transport host port` | Connector protocol; `NetCap` is stdlib impl |
| TLS | `tls-connect connector\|Handle host port opts` | Rust (rustls); two forms |
| HTTP/1.0 | `http-get connector url headers tls-opts`, `fetch` | pure-tinct stdlib/net.llt; dispatches on `url.scheme` |
| HTTP sessions | `http-connect connector\|Handle url opts` | Rust (reqwest); returns `HttpConn` |
| Proxy tunnels | `socks5-connect`, `proxy-connect` | Rust; return `Handle@[...Stream RW]` |

`http-get` and `fetch` are pure-tinct. `http-connect` is Rust because
HTTP/2 and HTTP/3 require protocol engines that can't be expressed as
Handle streams.

## Implementation

### Rust Builtins (`src/builtins.rs`)

**`connect connector Transport host port opts`** — generalised from
`connect cap host port`. Accepts any Connector (not just `NetCap`) and
an explicit `Transport` variant (`Tcp`, `Udp`, or user-defined).
Returns `Handle@[Binary Readable Writable Stream]` for `Tcp`,
`Handle@[Binary Readable Writable Datagram]` for `Udp`. `Tcp` is
default when `Transport` is omitted.

**`tls-connect`** — two forms:

- Connector form: `tls-connect connector Transport host port opts`
  opens the connection via `connect connector Transport ...` then
  layers TLS. `Transport` must produce a `Stream` Handle.
- Handle form: `tls-connect h@[Handle [...Stream RW...]] sni opts`
  layers TLS on an existing stream Handle.

Returns `Handle@[Binary Readable Writable Stream Tls]`. Default trust:
system roots via `rustls-native-certs`. `mozilla-roots: true` opt-in
loads `webpki-roots`. The `Tls` tag carries leaf cert metadata and
negotiated ALPN for `tls-peer-cert`.

**`tls-peer-cert handle`** — requires `Handle@[... Tls ...]`. Returns
dict with `subject`, `issuer`, `sans`, `not-before`, `not-after`,
`spki-sha256`. Type error on non-Tls handles.

**`fetch connector url`** — Rust-level HTTP client using `reqwest`.
Accepts any Connector. Returns
`[status: Int  headers: Dict  body: String]`. Accepts optional opts
dict: `[tls: TLS-opts  follow-redirects: Bool  headers: Dict  method: String  body: String]`.

### CLI — `--cap-net`

`--cap-net NAME=ENTRY` injects `$NAME` as a `NetCap` — the stdlib
Connector. A `NetCap` supports `connect $NAME Tcp host port` and
`connect $NAME Udp host port` for any host/port that matches the
allowlist `ENTRY` (hostname, `host:port`, or `*.glob`).

Multiple `--cap-net` flags with the same NAME accumulate into one
NetCap allowlist:

```
tinct run --cap-net api=api.internal --cap-net api=metrics.internal script.llt
# $api allows both hosts
```

### Handle Type (`src/value.rs`)

`Value::Handle` carries a capability row. `connect Tcp` sets
`{Binary Readable Writable Stream}`; `connect Udp` sets
`{Binary Readable Writable Datagram}`; `tls-connect` sets
`{Binary Readable Writable Stream Tls}`. The `Tls` tag carries
`Option<TlsInfo>` (leaf cert + negotiated ALPN).

### stdlib (`stdlib/net.llt`)

`http-get`, `fetch`, `fetch-opts` are pure-tinct implementations over
the capability-typed handles. When the Rust `fetch` builtin lands,
`stdlib/net.llt` becomes a thin compatibility wrapper.

### Dependencies (`Cargo.toml`)

| Crate | Required | Purpose |
|-------|----------|---------|
| `rustls = "0.23"` | always | TLS engine |
| `rustls-native-certs = "0.7"` | always | System roots (the default) |
| `webpki-roots = "0.26"` | always | Compiled-in Mozilla roots; needed at runtime when `mozilla-roots: true` |
| `sha3 = "0.10"` | always | SHA3-256/384/512 SPKI pin algorithms |
| `blake3` | already present | `Blake3` SPKI pin variant |
| `reqwest = { version = "0.12", features = ["http2", "brotli"] }` | always | HTTP/2 `fetch` |
| `reqwest = { ..., features = ["http3"] }` | always | HTTP/3 `fetch` |

All crates are compiled in unconditionally. The tinct binary supports
all runtime options without recompilation — script authors cannot be
expected to recompile tinct to enable `mozilla-roots: true` or SHA3-256
pins.

### Type Checker (`src/typecheck.rs`)

```tinct
[
  Uri: [type [
    scheme:   @String
    username: @[String Null]
    password: @[String Null]
    host:     @[String Null]
    port:     @[Int Null]
    path:     @String
    query:    @[String Null]
    fragment: @[String Null]
  ]]

  Url: [type [
    scheme:   @String
    username: @[String Null]
    password: @[String Null]
    host:     @String
    port:     @Int
    path:     @String
    query:    @[String Null]
    fragment: @[String Null]
  ]]

  Urn: [type [
    nid:         @String
    nss:         @String
    r-component: @[String Null]
    q-component: @[String Null]
    fragment:    @[String Null]
  ]]

  TlsOpts: [type [
    ca-bundle:       @[Handle Null]
    no-system-roots: @[Bool Null]
    mozilla-roots:   @[Bool Null]
    client-cert:     @[Handle Null]
    client-key:      @[Handle Null]
    pins:            @[[Seq SpkiPin] Null]
    alpn:            @[[Seq String] Null]
  ]]

  PeerCert: [type [
    subject:     @String
    issuer:      @String
    sans:        @[Seq String]
    not-before:  @Timestamp
    not-after:   @Timestamp
    spki-sha256: @String
  ]]

  Connector: [type [
    connect: [fn@Handle [transport@Any  host@String  port@Int  opts@Any]]
  ]]
]

# Builtin signatures:
connect     : [fn@Handle      [connector@Connector  transport@Any  host@String  port@Int]]
tls-connect : [fn@Handle      [connector@Connector  transport@Any  host@String  port@Int  opts@TlsOpts]]
tls-connect : [fn@Handle      [h@Handle  sni@String  opts@TlsOpts]]
tls-peer-cert : [fn@PeerCert  [h@Handle]]
http-connect  : [fn@HttpConn  [connector@Connector  uri@Uri  opts@Any]]
http-connect  : [fn@HttpConn  [h@Handle  uri@Uri]]
socks5-connect : [fn@Handle   [h@Handle  host@String  port@Int  creds@Any]]
proxy-connect  : [fn@Handle   [h@Handle  host@String  port@Int]]
uri           : [fn@Uri     [s@String]]
url           : [fn@Url     [s@String]]
urn           : [fn@Urn     [s@String]]
uri-params    : [fn@Dict    [u@[Uri Url]]]
uri-origin    : [fn@String  [u@Url]]
uri->string   : [fn@String  [u@[Uri Url Urn]]]
http-get      : [fn@Dict  [connector@Connector  uri@Uri  headers@Dict  tls-opts@[TlsOpts Null]]]
fetch         : [fn@Dict  [connector@Connector  uri@Uri]]
```

## Dependencies

- Capability-typed Handle (§Streaming File I/O, lib-supplemental.md)
  is a prerequisite — `connect` and `tls-connect` return typed handles
  before `http-get`/`fetch` are pure-tinct.
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
