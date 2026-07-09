# What If: TLS, PKI, and HTTP Protocol Support for tinct (lib-tls)

**State:** Accepted — 2026-05-07

What would it take to give tinct a complete HTTP and TLS story — CA roots,
client certificates, certificate pinning, and protocol negotiation — while
keeping the capability model clean and the design appropriate for a
configuration language?

## Current State

`doc/whatif/io.md` (accepted and archived) defines `connect` and `tls`
builtins for TCP and TLS connections. The current implementation has
`connect` returning a read-only `Handle` (`Box<dyn BufRead>`) — the write
half of the TCP stream was discarded. This document redesigns both
builtins: `connect` now returns a bidirectional `Handle[Binary Readable
Writable Stream]`, and `tls` is renamed `tls-connect` returning
`Handle[Binary Readable Writable Stream Tls]`. The Handle IS the
authenticated channel: the TLS handshake completed at call time, so
holding a `Tls`-capable Handle proves the connection was established
against a trusted server.

What is not yet implemented (all are specified in this document's Design section):

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

### The Connector Protocol

A **Connector** is any value that can open connections to remote
endpoints. `NetCap` is the stdlib implementation; user-written
transports (WireGuard clients, custom resolvers, test fakes) implement
the same protocol and can be substituted anywhere a `NetCap` is accepted.

**Protocol method:**

```text
[connect connector Transport host port opts] → Handle[... Stream|Datagram ...]
```

`Transport` is a nominal unit variant specifying the transport
semantics. The stdlib provides two; users define others:

| Variant | Semantics | Handle capability | Notes |
|---------|-----------|-------------------|-------|
| `Tcp` | reliable byte stream | `Stream` | default when unspecified |
| `Udp` | unreliable datagrams | `Datagram` | |
| *(user-defined)* | anything | user-specified | `Sctp`, `UdpLite`, `Quic`, … |

The Handle returned carries `Stream` or `Datagram` (and `Readable
Writable Binary`) to indicate what higher layers can do with it.

**`NetCap` is a built-in Connector** that implements `Tcp` and `Udp`
via OS sockets. `connect` (the existing builtin) becomes:

```tinct
# Explicit transport:
[connect net Tcp "api.example.com" 443]   # → Handle[Binary Readable Writable Stream]
[connect net Udp "8.8.8.8" 53]            # → Handle[Binary Readable Writable Datagram]

# Tcp is the default when Transport is omitted:
[connect net "api.example.com" 443]       # same as Tcp form
```

**User-defined Connector (e.g. WireGuard client):**

```tinct
[wg: [wg-connect wg-cap config]]   # → WgConnector

# WgConnector implements the protocol:
WgConnector: [
  connect: [fn [transport host port opts]
    [match transport
      Tcp  [wg-open-tcp  host port opts]
      Udp  [wg-open-udp  host port opts]
      _    [error [str "unsupported transport: " [tag-of transport]]]]]]

# Use it anywhere NetCap was accepted:
[tls-connect wg Tcp "api.example.com" 443 tls-opts]
[http-connect wg "api.example.com" 443 []]
```

### Handle Types for Network Connections

Network handles produced by `connect` carry `Readable` and `Writable`
(TCP is bidirectional) but not `Seekable` (streams are sequential).
`tls-connect` adds the `Tls` capability, which gates `tls-peer-cert`.

```text
connect     connector Tcp  host port      → Handle[Binary Readable Writable Stream]
connect     connector Udp  host port      → Handle[Binary Readable Writable Datagram]
tls-connect connector Tcp  host port opts → Handle[Binary Readable Writable Stream Tls]
tls-connect h@Handle[...Stream RW...] sni opts → Handle[Binary Readable Writable Stream Tls]
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

With `Handle[Binary Readable Writable Stream]`, HTTP/1.0 is pure-tinct.
`http-get` handles both `http://` and `https://` by dispatching on `url.scheme`;
`https-get` no longer exists as a separate function:

```tinct
# stdlib/net.llt — HTTP/1.0 over any Readable Writable Stream Handle
http-get: [fn [connector@Connector url@Url headers@Dict tls-opts@[TlsOpts Null]]
  [conn: [match url.scheme
    "https" [tls-connect connector Tcp url.host url.port [tls-opts or []]]
    "http"  [connect connector Tcp url.host url.port]
    _       [error [str "http-get: unsupported scheme: " url.scheme]]]]
  [write conn [str-bytes [build-http-request "GET" url.path headers]]]
  [parse-http-response [slurp conn]]]

fetch: [fn [connector@Connector url@Url]
  [http-get connector url [] null]]
```

### `HttpConn` and Connection Reuse

A single-shot `http-get` creates and closes a connection per request.
For connection reuse (required for HTTP/2 multiplexing), use
`http-connect` to obtain an `HttpConn` value:

```text
http-connect connector host port opts → HttpConn
```

`HttpConn` wraps a persistent connection (or connection pool). It
accepts a `Connector` and internally calls `connect connector Tcp ...`
or `connect connector Udp ...` depending on the protocol negotiated.
Multiple requests over the same `HttpConn` reuse the underlying
HTTP/2 (or HTTP/3) connection:

```tinct
# All three requests share one HTTP/2 connection:
[client: [http-connect wg "api.example.com" 443 []]]
[users:  [http-get  client "/v1/users"  []]]
[posts:  [http-get  client "/v1/posts"  []]]
[config: [http-get  client "/v1/config" []]]
```

**Two forms for `http-connect`:**

```tinct
# Connector form — http-connect picks the transport (Tcp or Udp for QUIC):
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
| `ca-bundle` | `Handle[Text Readable ...]` | — | PEM file via `[open cap path Readable]`; added to system roots |
| `no-system-roots` | `Bool` | `false` | Drop system roots — use only `ca-bundle` (private PKI) |
| `mozilla-roots` | `Bool` | `false` | Also load compiled-in Mozilla roots (`webpki-roots` opt-in) |
| `client-cert` | `Handle[Text Readable ...]` | — | PEM client certificate (mTLS) |
| `client-key` | `Handle[Text Readable ...]` | — | PEM private key for client cert |
| `pins` | `@[Seq SpkiPin]` | — | SPKI fingerprints; leaf cert must match one. See §SPKI Pins. |
| `alpn` | `Seq[String]` | `["http/1.1"]` | ALPN protocol list for negotiation |

### CA Root Selection

**System roots — the default.** `rustls-native-certs` reads the OS
certificate store at connection time (Linux: `/etc/ssl/certs`; macOS:
Keychain; Windows: Certificate Store). This is what the operator
maintains and trusts.

**Custom CA bundle** — `ca-bundle` points to a PEM file opened via
`[open cap path Readable]`. Cert access flows through `DirCap` and is
auditable. The PEM is added to system roots by default; set
`no-system-roots: true` to trust only the custom CA (fully private PKI
where public CAs must be excluded):

```tinct
[tls-connect net "vault.internal" 8200 [
  ca-bundle:       ca-pem
  no-system-roots: true    # trust only our internal CA
]]
```

**Compiled-in Mozilla roots** — opt-in via `mozilla-roots: true`. Only
pulls in the `webpki-roots` crate when used. Useful in containers with
no system CA store and no custom CA bundle (e.g. connecting to a public
API from a distroless image).

All three trust sources union when combined.

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

### SPKI Pins

SPKI (Subject Public Key Info) hash pinning locks a connection to a
specific public key, providing defence against CA compromise. Pinning
survives certificate rotation as long as the key is reused.

**`SpkiPin`** is a strongly-typed value carrying the hash algorithm and
the raw fingerprint bytes. The algorithm is a `HashAlgorithm` nominal
variant (defined in `doc/whatif/lib-supplemental.md` §Bitwise Primitives):

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
#   not-before:  <Timestamp>   # lib-datetime Timestamp; use format-timestamp to display
#   not-after:   <Timestamp>   # compare with [now clock] for expiry checks
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

### HTTP/2, HTTP/3, and the Handle Boundary — Design Rationale

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

### Network Stack Summary

| Tier | Abstraction | Implementation |
|------|-------------|----------------|
| Transport | `connect connector Transport host port` | Connector protocol; `NetCap` is stdlib impl |
| TLS | `tls-connect connector\|Handle host port opts` | Rust (rustls); two forms |
| HTTP/1.0 | `http-get connector url headers tls-opts`, `fetch` | **pure-tinct** stdlib/net.llt; dispatches on `url.scheme` |
| HTTP sessions | `http-connect connector\|Handle url opts` | Rust (reqwest); returns `HttpConn` |
| Proxy tunnels | `socks5-connect`, `proxy-connect` | Rust; return `Handle[...Stream RW]` |

`http-get` and `fetch` are pure-tinct. `http-get` handles both `http://`
and `https://` by dispatching on `url.scheme` — `https-get` does not
exist as a separate function. `http-connect` is Rust because HTTP/2 and
HTTP/3 require protocol engines that can't be expressed as Handle streams.

## What Would Change

### Rust Builtins (`src/builtins.rs`)

**`connect connector Transport host port opts`** — generalised from
`connect cap host port`. Accepts any Connector (not just `NetCap`) and
an explicit `Transport` variant (`Tcp`, `Udp`, or user-defined).
Returns `Handle[Binary Readable Writable Stream]` for `Tcp`,
`Handle[Binary Readable Writable Datagram]` for `Udp`. `Tcp` is
default when `Transport` is omitted.

**`tls-connect`** — two forms:

- Connector form: `tls-connect connector Transport host port opts`
  opens the connection via `connect connector Transport ...` then
  layers TLS. `Transport` must produce a `Stream` Handle.
- Handle form: `tls-connect h@Handle[...Stream RW...] sni opts`
  layers TLS on an existing stream Handle.

Returns `Handle[Binary Readable Writable Stream Tls]`. Default trust:
system roots via `rustls-native-certs`. `mozilla-roots: true` opt-in
loads `webpki-roots`. The `Tls` tag carries leaf cert metadata and
negotiated ALPN for `tls-peer-cert`.

**`tls-peer-cert handle`** — new builtin. Requires
`Handle[... Tls ...]`. Returns dict with `subject`, `issuer`, `sans`,
`not-before`, `not-after`, `spki-sha256`. Type error on non-Tls handles.

**`fetch connector url`** — Rust-level HTTP client using `reqwest`.
Accepts any Connector (not just `NetCap`). Replaces `fetch` in
`stdlib/net.llt` for HTTP/2+ support. Returns
`[status: Int  headers: Dict  body: String]`. Accepts optional opts
dict: `[tls: TLS-opts  follow-redirects: Bool  headers: Dict  method: String  body: String]`.

### CLI — `--cap-net`

`--cap-net NAME=ENTRY` injects `$NAME` as a `NetCap` — the stdlib
Connector. A `NetCap` supports `connect $NAME Tcp host port` and
`connect $NAME Udp host port` for any host/port that matches the
allowlist `ENTRY` (hostname, `host:port`, or `*.glob`).

`NetCap` is a built-in Connector; user-defined Connectors (WireGuard
clients, SOCKS5 wrappers, custom resolvers) are constructed inside the
tinct script itself and do not have a CLI flag. The CLI only needs to
grant the raw network capability; the script composes it into whatever
transport stack it needs.

```sh
# Inject a NetCap allowing access to api.internal:
tinct run --cap-net net=api.internal script.llt

# Script then composes freely:
[tls: [tls-connect net Tcp "api.internal" 443 opts]]
[client: [http-connect tls "api.internal"]]
```

Multiple `--cap-net` flags with the same NAME accumulate into one
NetCap allowlist:

```sh
tinct run --cap-net api=api.internal --cap-net api=metrics.internal script.llt
# $api allows both hosts
```

### Handle Type (`src/value.rs`)

`Value::Handle` gains a capability row per
`doc/whatif/lib-supplemental.md` §Streaming File I/O. `connect Tcp`
sets `{Binary Readable Writable Stream}`; `connect Udp` sets
`{Binary Readable Writable Datagram}`; `tls-connect` sets
`{Binary Readable Writable Stream Tls}`. The `Tls` tag carries
`Option<TlsInfo>` (leaf cert + negotiated ALPN).

### stdlib (`stdlib/net.llt`)

`http-get`, `https-get`, `fetch`, `fetch-opts` are pure-tinct
implementations over the capability-typed handles. When the Rust `fetch`
builtin lands, `stdlib/net.llt` becomes a thin compatibility wrapper.

### Dependencies (`Cargo.toml`)

| Crate | Required | Purpose |
|-------|----------|---------|
| `rustls = "0.23"` | always | TLS engine |
| `rustls-native-certs = "0.7"` | always | System roots (the default) |
| `webpki-roots = "0.26"` | always | Compiled-in Mozilla roots; needed at runtime when `mozilla-roots: true` |
| `sha3 = "0.10"` | always | SHA3-256/384/512 SPKI pin algorithms; needed at runtime for those variants |
| `blake3` | already present | `Blake3` SPKI pin variant; tinct already uses blake3 for `$include` hashes |
| `reqwest = { version = "0.12", features = ["http2", "brotli"] }` | always | HTTP/2 `fetch` |
| `reqwest = { ..., features = ["http3"] }` | always | HTTP/3 `fetch` |

All crates are compiled in unconditionally. The tinct binary must
support all runtime options without recompilation — script authors
cannot be expected to recompile tinct to enable `mozilla-roots: true`
or SHA3-256 pins.
| `reqwest = { version = "0.12", features = ["http2", "brotli"] }` | Rust `fetch` | High-level HTTP/2 |
| `reqwest = { ..., features = ["http3"] }` | HTTP/3 | HTTP/3 via QUIC |

All crates are compiled in unconditionally. `rustls` has no built-in
root CAs; `rustls-native-certs` provides the default system roots.
`webpki-roots` is always compiled in so that `mozilla-roots: true` is
available at runtime without recompilation.

### Type Checker (`src/typecheck.rs`)

```tinct
# Type aliases declared in stdlib/net.llt

[
  # Uri — generic RFC 3986 URI (RFC 3986 §3); covers all URI forms.
  # Use when you need to parse or store arbitrary URIs including non-hierarchical ones.
  # Constructed via: [uri "https://host/path"]
  #                  [uri "mailto:user@example.com"]
  #                  [uri "urn:isbn:978-0-306-40615-7"]
  #                  [uri "tel:+1-816-555-1212"]
  Uri: [type [
    scheme:   @String            # lowercase scheme
    username: @[String Null]     # null if absent or non-hierarchical; splitting userinfo on ":"
    password: @[String Null]     # is a practical convention — RFC 3986 §3.2.1 treats userinfo
                                 # as opaque. Password in URIs is deprecated per §7.5.
    host:     @[String Null]     # null for non-hierarchical (mailto:, tel:, urn:, news:)
    port:     @[Int Null]        # null for non-hierarchical or unspecified; empty port string
                                 # (e.g. "http://host:/path") parsed as null, not error
    path:     @String            # always present per RFC 3986 §3.3 ("though may be empty")
    query:    @[String Null]     # raw query string without "?"; null if absent
    fragment: @[String Null]     # fragment without "#"; null if absent
  ]]

  # Url — hierarchical URI with required authority (RFC 3986 §3.2).
  # All network functions (http-get, http-connect, tls-connect) accept Url, not Uri.
  # Constructed via: [url "https://user:pass@host:443/path?q=1"]
  #                  [url "postgres://user:pass@localhost:5432/mydb"]
  #                  [url "s3://bucket/key?region=us-east-1"]
  Url: [type [
    scheme:   @String            # lowercase: "https", "http", "postgres", "s3", "amqp", etc.
    username: @[String Null]     # null if absent
    password: @[String Null]     # null if absent; splitting userinfo on ":" is a convention
                                 # not mandated by RFC 3986 §3.2.1; deprecated for HTTP (§7.5)
    host:     @String            # always present (validated at parse time); IPv6 without brackets
    port:     @Integer               # always present; scheme-defaulted if absent; empty port string
                                 # ("http://host:/path") treated as absent → defaulted
    path:     @String            # always present per RFC 3986 §3.3; "/" if absent in string
    query:    @[String Null]     # raw query string without "?"; null if absent
    fragment: @[String Null]     # fragment without "#"; null if absent
  ]]

  # Urn — URN per RFC 8141: urn:NID:NSS[?+r][?=q][#f]
  # Constructed via: [urn "urn:isbn:978-0-306-40615-7"]
  #                  [urn "urn:uuid:6e8bc430-9c3a-11d9-9669-0800200c9a66"]
  #                  [urn "urn:oasis:names:specification:docbook:dtd:xml:4.1.2"]
  Urn: [type [
    nid:         @String            # Namespace Identifier: "isbn", "uuid", "oasis", etc.
    nss:         @String            # Namespace Specific String
    r-component: @[String Null]     # RFC 8141 §2.3: resolution parameters (?+...); null if absent
    q-component: @[String Null]     # RFC 8141 §2.3: query parameters (?=...); null if absent
    fragment:    @[String Null]     # fragment (#...); null if absent
  ]]
  # Note: r-component is SHOULD NOT be used per RFC 8141 §2.3.1 ("reserved for future use").
  # It is included for completeness; parsers should silently accept it.

  # TLS options dict — passed as fifth arg to tls-connect
  TlsOpts: [type [
    ca-bundle:      @[Handle Null]   # PEM file via [open cap path Readable]
    no-system-roots: @[Bool Null]   # default false; drop system roots
    mozilla-roots:  @[Bool Null]    # default false; opt-in webpki-roots
    client-cert:    @[Handle Null]  # PEM client cert (mTLS)
    client-key:     @[Handle Null]  # PEM private key for client cert
    pins:           @[[Seq SpkiPin] Null] # typed SPKI fingerprints; see §SPKI Pins
    alpn:           @[[Seq String] Null]  # default ["http/1.1"]
  ]]

  # tls-peer-cert return type
  PeerCert: [type [
    subject:     @String
    issuer:      @String
    sans:        @[Seq String]      # Subject Alternative Names
    not-before:  @Timestamp         # lib-datetime Timestamp (depends on lib-datetime.md)
    not-after:   @Timestamp         # compare directly with [now clock]; no parse-timestamp needed
    spki-sha256: @String            # sha256//base64= format
  ]]

  # Connector protocol type (structural)
  Connector: [type [
    connect: [fn@Handle [transport@Any  host@String  port@Integer  opts@Any]]
  ]]
]

# Builtin signatures:
connect     : [fn@Handle      [connector@Connector  transport@Any  host@String  port@Integer]]
              # Tcp → Handle[Binary Readable Writable Stream]
              # Udp → Handle[Binary Readable Writable Datagram]
              # Transport omitted → Tcp implied

tls-connect : [fn@Handle      [connector@Connector  transport@Any  host@String  port@Integer  opts@TlsOpts]]
tls-connect : [fn@Handle      [h@Handle  sni@String  opts@TlsOpts]]
              # Either form → Handle[Binary Readable Writable Stream Tls]

tls-peer-cert : [fn@PeerCert  [h@Handle]]   # h must carry Tls capability

http-connect  : [fn@HttpConn  [connector@Connector  uri@Uri  opts@Any]]
http-connect  : [fn@HttpConn  [h@Handle  uri@Uri]]

socks5-connect : [fn@Handle   [h@Handle  host@String  port@Integer  creds@Any]]
proxy-connect  : [fn@Handle   [h@Handle  host@String  port@Integer]]

# Uri/Url/Urn builtins:
uri           : [fn@Uri     [s@String]]    # parse any URI string → Uri (generic)
url           : [fn@Url     [s@String]]    # parse hierarchical URL → Url; error if no authority
urn           : [fn@Urn     [s@String]]    # parse URN → Urn; error if not urn: scheme
uri-params    : [fn@Dict    [u@[Uri Url]]] # parse u.query → {key: value, ...}; {} if null
uri-origin    : [fn@String  [u@Url]]       # "scheme://host:port" (Url only — host is required)
uri->string   : [fn@String  [u@[Uri Url Urn]]] # reconstruct full URI/URL/URN string

# In stdlib/net.llt:
# http-get handles both http:// and https:// via url.scheme dispatch
# https-get does not exist — use http-get with an https:// Uri
http-get      : [fn@Dict  [connector@Connector  uri@Uri  headers@Dict  tls-opts@[TlsOpts Null]]]
                # dispatches on url.scheme; tls-opts ignored for http://, used for https://
                # returns [status: @Integer  headers: @Dict  body: @String]
fetch         : [fn@Dict  [connector@Connector  uri@Uri]]
                # convenience: http-get with empty headers and null tls-opts
```

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
