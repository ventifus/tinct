# What If: Composable Networking for tinct (lib-net-v2)

**State:** Proposal

What would it take to give tinct a fully composable networking stack — one where TCP, UDP, Unix sockets, TLS, QUIC, SOCKS5, HTTP/1.1, HTTP/2, HTTP/3, gRPC, WebSocket, and DNS all emerge from three orthogonal primitives that compose freely?

## Current State

`doc/whatif/completed/lib-tls.md` (accepted 2026-05-07) introduced the **Connector protocol** and the Handle capability row, giving tinct raw TCP connections, TLS, SPKI pinning, CA configuration, and a skeletal HTTP client. The implementation status:

- `connect cap Tcp host port` → `Handle[Binary Readable Writable Stream]` ✓
- `tls-connect cap Tcp host port opts` → `Handle[Binary Readable Writable Stream Tls]` ✓
- `tls-connect handle sni opts` → stub (architectural blocker: raw TcpStream not accessible from Handle)
- `connect cap Udp host port` → stub ("UDP not yet supported")
- `http-connect url` → reqwest-based `HttpConn` ✓ (but uncapped, not composable with Connector chain)
- `socks5-connect`, `proxy-connect` → removed

The lib-tls design has two structural gaps:

1. **Port is forced on all transports.** The signature `[connect cap Transport host port]` assumes every transport has a host and port. Unix domain sockets have a path. ICMP has a host but no port. A custom transport tunneling HTTP over ICMP has neither port nor socket path. Forcing `port: null` is wrong.

2. **Protocol layering is ad-hoc.** `tls-connect` was special-cased as both a Connector form and a Handle form. There is no general model for "layer a protocol on top of an existing Handle." SOCKS5, HTTP CONNECT, DTLS, NOISE, and STARTTLS all need this pattern, but each would require its own special-cased builtin.

### What's Missing

1. A transport-generic `connect` signature where address format is determined by the Transport variant, not hardwired as `(host, port)`.
2. A **Layer protocol** for Handle→Handle protocol upgrades (generalising the tls-connect Handle form).
3. Unix domain sockets and named pipes as first-class transport variants using `DirCap` rather than `NetCap`.
4. A **Session protocol** for multiplexed connections (QUIC, HTTP/2, HTTP/3).
5. QUIC (`quinn`-backed) and HTTP/2/HTTP/3 Sessions.
6. A `protocols/` stdlib subdirectory with SOCKS5, DNS, gRPC, and WebSocket as pure-tinct libraries.
7. A high-level `fetch` that auto-negotiates HTTP/1.1, HTTP/2, and HTTP/3 transparently.

## Why Composable Networking Matters for tinct

tinct runs at infrastructure boundaries. Infrastructure networking is not "send an HTTPS request." It is:

- Querying the Docker daemon over a Unix socket using the HTTP API
- Resolving Kubernetes service endpoints via DNS SRV records
- Reaching an internal API through a SOCKS5 corporate proxy, over TLS, with a pinned certificate
- Sending a health-check ping to a host that only responds to ICMP
- Subscribing to a Kubernetes event stream via WebSocket over HTTP/2
- Calling a gRPC service with JSON transcoding

A flat API (`connect`, `tls-connect`, `http-connect`) cannot express these. Three composable primitives can express all of them, and any protocol the user invents.

## Design

### Three Primitives

Every networking operation in tinct is one of three things:

**Connector** — establishes a fresh channel to a remote endpoint. Returns a Handle.

**Layer** — upgrades an existing Handle by speaking a protocol over it. Consumes one Handle, returns another with augmented capabilities.

**Session** — represents a multiplexed connection from which multiple logical streams can be opened. Created from a Handle; produces stream Handles.

These compose left-to-right. A complete HTTP/3-over-SOCKS5 stack is:

```tinct
[include %libdir "net.llt"]
[include %libdir "protocols/socks5.llt"]

# 1. Connector: TCP connection to proxy (NetCap)
[tcp:   [connect %nc Tcp "proxy.corp" 1080]]

# 2. Layer: SOCKS5 tunnel through proxy to target
[tun:   [socks5-layer tcp "api.internal" 443]]

# 3. Layer: TLS on the tunneled stream
[tls:   [tls-layer tun "api.internal" tls-opts]]

# 4. Session: QUIC over the tunneled TLS connection
[quic:  [quic-session tls "api.internal" quic-opts]]

# 5. Session: HTTP/3 over QUIC
[http3: [http3-session quic]]

# 6. Application: make a request
[r: [http-request http3 "GET" "/v1/services" []]]
```

Or, using the `fetch` convenience function which assembles this automatically:

```tinct
[fetch [socks5-layer [connect %nc Tcp "proxy.corp" 1080] "api.internal" 443]
       [url "https://api.internal/v1/services"]]
```

### The Connector Protocol

`connect` dispatches on the **Transport variant** to determine the address format. The Transport variant defines what additional arguments follow:

```tinct
# Stream transports (NetCap)
[connect cap Tcp  host port]           # → Handle[Binary Readable Writable Stream]
[connect cap Udp  host port]           # → Handle[Binary Readable Writable Datagram]
[connect cap Icmp host]                # → Handle[Binary Readable Writable Datagram]

# Local transports (DirCap)
[connect cap UnixStream    path]       # → Handle[Binary Readable Writable Stream]
[connect cap UnixDatagram  path]       # → Handle[Binary Readable Writable Datagram]
[connect cap NamedPipe     path]       # → Handle[Binary Readable Writable]

# User-defined: whatever the Connector's connect function expects
[connect my-conn MyTransport ...args]  # → Handle[...]
```

Port is absent for transports that have no port concept. A custom Transport tunneling HTTP over ICMP simply omits port:

```tinct
IcmpHttpConn: [
  connect: [fn [transport host]
    [match transport
      IcmpHttp [open-icmp-stream %nc host]
      _        [error [str "unsupported transport: " [tag-of transport]]]]]]

[h: [connect IcmpHttpConn IcmpHttp "10.0.0.5"]]
[http-get h [url "http://10.0.0.5/api"]]
```

**Capability routing by transport:**

| Transport family | Required capability | Mechanism |
|---|---|---|
| `Tcp`, `Udp`, `Icmp` | `NetCap` | Allowlist checked before syscall |
| `UnixStream`, `UnixDatagram`, `NamedPipe` | `DirCap` | cap_std path-based access |
| User-defined | Whatever the Connector checks | Connector is responsible |

`connect` with a `DirCap` opens a Unix socket relative to the cap's directory — consistent with how `open cap path mode` works for files. The socket's filesystem permissions apply on top.

**User-defined Connectors** implement the protocol:

```tinct
MyConnector: [
  connect: [fn [transport ...address]
    [match transport
      Tcp         [open-tcp-via-wireguard address.0 address.1]
      UnixStream  [open-unix address.0]
      _           [error "unsupported"]]]]
```

### The Layer Protocol

A Layer is any function that takes a Handle and returns a Handle with augmented capabilities:

```
Layer: Handle[R] → Handle[R ∪ NewCaps]
```

Any pure-tinct function with this signature is a Layer. There is no Layer typeclass or interface — the composition is structural.

**Standard library Layers:**

```tinct
# TLS upgrade — requires Handle[... Stream ...]; adds Tls capability
# Rust builtin — requires Handle refactor to expose raw stream
[tls-layer    handle@Handle sni@String opts@Dict]
  → Handle[... Stream Tls]

# DTLS upgrade — requires Handle[... Datagram ...]; adds Tls capability
# Architectural: deferred (requires dtls Rust dep)
[dtls-layer   handle@Handle sni@String opts@Dict]
  → Handle[... Datagram Tls]

# SOCKS5 proxy tunnel — pure tinct in protocols/socks5.llt
# Transparently routes the Handle through a SOCKS5 proxy
[socks5-layer handle@Handle host@String port@Int creds@[Dict Null]]
  → Handle[... Stream]

# HTTP CONNECT tunnel — pure tinct in net.llt
# Uses an HTTP CONNECT request to establish a tunneled stream
[http-connect-layer handle@Handle host@String port@Int headers@Dict]
  → Handle[... Stream]
```

The **tls-layer Handle form** is the fix for the STARTTLS use case: connect to a server's plain port, negotiate TLS mid-stream. This replaces the stub `tls-connect handle sni opts` from lib-tls.md:

```tinct
# PostgreSQL STARTTLS: connect plain, upgrade to TLS
[pg:     [connect %nc Tcp "db.internal" 5432]]
[pg-tls: [tls-layer pg "db.internal" tls-opts]]

# Docker over Unix socket, no TLS (daemon handles auth via socket permissions)
[docker: [connect %pwd UnixStream "docker.sock"]]
[r:      [http-get docker [url "http://localhost/v1.41/containers/json"] []]]
```

Any pure-tinct function can be a Layer. A user building an HTTP-over-ICMP tunnel wraps their `open-icmp-stream` result in a Layer function:

```tinct
icmp-layer: [fn [handle target-host]
  [# send ICMP echo requests carrying HTTP payload
   # returns a new Handle that speaks HTTP over ICMP datagrams
   wrap-icmp handle target-host]]
```

### The Session Protocol

A Session is a multiplexed connection: one physical channel that carries multiple independent logical streams. Sessions are opened from Handles; Handles are opened from Sessions.

**QUIC Session** — implemented in Rust via `quinn`. QUIC integrates transport, TLS, and reliable ordered delivery at the UDP level. `quinn` must own the UDP socket (it manages path migration, congestion control, ACKs internally), so `quic-session` is Connector-style rather than a Layer over an existing UDP Handle:

```tinct
# quic-session opens its own UDP socket internally
[quic: [quic-session %nc "api.example.com" 443 quic-opts]]
  # → QuicSession

# Open a reliable bidirectional stream
[stream: [quic-open-stream quic]]
  # → Handle[Binary Readable Writable Stream]

# Open a unreliable unidirectional datagram channel (RFC 9297)
[dgram: [quic-open-datagram quic]]
  # → Handle[Binary Readable Writable Datagram]
```

`quic-opts` carries TLS configuration (CA roots, ALPN, client certs, SPKI pins) — QUIC's integrated TLS replaces a separate `tls-layer` step.

**HTTP/2 Session** — via reqwest/h2. Requires a `Handle[Stream Tls]` with `h2` in the ALPN negotiation, or a cleartext `Handle[Stream]` for h2c (HTTP/2 without TLS):

```tinct
[tls:  [tls-layer [connect %nc Tcp "api.example.com" 443] "api.example.com" [alpn: ["h2"]]]]
[h2:   [http2-session tls]]
  # → Http2Session

[r:    [http-request h2 "GET" "/api/data" []]]
  # → {status: 200, headers: Dict, body: Bytes}
```

**HTTP/3 Session** — over a QUIC session:

```tinct
[quic: [quic-session %nc "api.example.com" 443 [alpn: ["h3"]]]]
[h3:   [http3-session quic]]
  # → Http3Session

[r:    [http-request h3 "GET" "/api/data" []]]
```

**`http-request`** is the uniform application-level call across HTTP/2 and HTTP/3 sessions:

```tinct
http-request: [fn@Result [session method@String path@String headers@Dict body@[Bytes Null]]
  ...]
# → {ok: {status: Int  headers: Dict  body: Bytes}} | {err: String}
```

### `fetch` — The Convenience Function

`fetch` assembles the optimal protocol stack automatically and returns Result:

```tinct
fetch: [fn@Result [connector url@Url opts@Dict]
  [match url.scheme
    "https" [fetch-https connector url opts]
    "http"  [fetch-http  connector url opts]
    _       [err: [str "fetch: unsupported scheme: " url.scheme]]]]

# fetch-https tries HTTP/3 first (via QUIC), falls back to HTTP/2, then HTTP/1.1
# The negotiation happens via ALPN in TLS for HTTP/2,
# and via Alt-Svc header for HTTP/3 upgrade.
```

For the 90% case:

```tinct
[do result
  [r:    [fetch %nc [url "https://api.example.com/data"] []]]
  [data: [from-json r.body]]
  [get "items" data]]
```

For the 10% case (control over the stack):

```tinct
# Pin to HTTP/2, custom CA, timeout
[tls:  [tls-layer [connect %nc Tcp "api.example.com" 443] "api.example.com"
         [alpn: ["h2"] ca-bundle: ca-handle]]]
[h2:   [http2-session tls]]
[r:    [http-request h2 "GET" "/api/data" [authorization: "Bearer ..." x-request-id: "abc"]]]
```

### Protocol Library: `protocols/`

Heavy protocol implementations live in `stdlib/protocols/` — included explicitly, not loaded automatically:

**`protocols/socks5.llt`** (~50 lines, pure tinct)

SOCKS5 Layer (RFC 1928 + RFC 1929). Performs the SOCKS5 handshake over an existing Handle and returns the same Handle type, now routing through the proxy:

```tinct
[include %libdir "protocols/socks5.llt"]

[proxy: [connect %nc Tcp "proxy.corp" 1080]]
[tun:   [socks5-connect proxy "internal-api.corp" 443]]
[tls:   [tls-layer tun "internal-api.corp" opts]]
```

Supports: no-auth mode, username/password auth (RFC 1929), IPv4/IPv6/hostname target address.

**`protocols/dns.llt`** (~100 lines, pure tinct over UDP)

DNS wire protocol (RFC 1035). Constructs and parses DNS packets using `str-bytes`/`bytes-str`:

```tinct
[include %libdir "protocols/dns.llt"]

[dns-query %nc "svc.cluster.local" SRV]
# → {ok: [{priority: 10  weight: 100  port: 8080  target: "pod-a.svc.cluster.local"} ...]}

[dns-query %nc "token.example.com" TXT]
# → {ok: ["v=spf1 ..."]}

[dns-query %nc "api.example.com" AAAA]
# → {ok: ["2001:db8::1"]}
```

Record type variants: `A`, `AAAA`, `MX`, `TXT`, `SRV`, `CNAME`, `NS`, `PTR`.

**`protocols/grpc.llt`** (~40 lines, pure tinct over Http2Session)

gRPC-JSON transcoding (no protobuf). Wraps HTTP/2 with gRPC framing and content-type:

```tinct
[include %libdir "protocols/grpc.llt"]

[h2:  [http2-session [tls-layer [connect %nc Tcp "svc.internal" 443] "svc.internal" opts]]]
[r:   [grpc-request h2 "mypackage.MyService" "GetItem" [id: "abc123"]]]
# → {ok: Dict} | {err: {code: Int message: String}}
```

**`protocols/websocket.llt`** (~80 lines, pure tinct)

WebSocket upgrade and framing (RFC 6455). Upgrades an HTTP connection to a WebSocket and provides send/receive:

```tinct
[include %libdir "protocols/websocket.llt"]

[tls: [tls-layer [connect %nc Tcp "stream.example.com" 443] "stream.example.com" opts]]
[ws:  [websocket-connect tls "/events" [authorization: "Bearer ..."]]]

[websocket-send ws [type: "text" data: "{\"subscribe\": \"metrics\"}"]]
[msg: [websocket-recv ws]]   # blocks until next frame
```

### Stdlib Layout

```
stdlib/
  prelude.llt          # Auto-loaded
  strings.llt          # Explicit include
  math.llt
  encoding.llt
  numeric.llt
  path.llt
  io.llt
  datetime.llt
  regex.llt
  toml-lite.llt
  macros.llt

  net.llt              # Networking fundamentals: fetch, http-get, URI utils
                       # Explicit include

  protocols/           # Protocol implementations: each explicit include
    socks5.llt         # SOCKS5 Layer (~50 lines pure tinct)
    dns.llt            # DNS wire protocol (~100 lines pure tinct)
    grpc.llt           # gRPC-JSON over HTTP/2 (~40 lines pure tinct)
    websocket.llt      # WebSocket upgrade + framing (~80 lines pure tinct)

  in/                  # Input parsers (pipeline use)
    json.llt
    toml-lite.llt
  out/                 # Output formatters (pipeline use)
    json.llt
    yaml.llt
    csv.llt
    toml.llt
    env.llt
    raw.llt
  formatter/           # Internal formatting utilities
    compact.llt
    pretty.llt
```

`net.llt` contains: `fetch`, `http-get`, `parse-http-response`, `build-http-request`, `http-connect-layer`, URI utilities (`uri-params`, `uri-origin`, `uri->string`). It does not contain any of the `protocols/` content.

### Handle Refactor for tls-layer

`tls-layer` (the Handle form of TLS upgrade, enabling STARTTLS) requires extracting the raw `TcpStream` from a Handle to hand off to rustls. This requires adding a `raw_tcp: Option<TcpStream>` field to `Value::Handle`, populated when `connect cap Tcp ...` creates the Handle and consumed (moved out) when `tls-layer` is called:

```rust
Value::Handle {
    caps: HashMap<String, Value>,
    inner: Box<dyn BufRead>,          // existing
    write_inner: Option<Box<dyn Write>>, // existing
    seek_inner: Option<...>,           // existing
    raw_tcp: Option<TcpStream>,        // NEW — Some for TCP handles, None for files/unix
}
```

After `tls-layer` extracts `raw_tcp`, the original Handle is consumed; subsequent operations on it produce a runtime error. The new TLS Handle wraps the TLS stream as before (TlsReader + TlsWriter sharing `Rc<RefCell<StreamOwned>>`).

## What Would Change

### Builtins (`src/builtins_io.rs`, `src/builtins.rs`)

**`connect` signature change:** Inspect Transport tag to determine argument count:
- `Tcp`, `Udp` → 2 address args (host, port)
- `UnixStream`, `UnixDatagram`, `NamedPipe` → 1 address arg (path)
- `Icmp` → 1 address arg (host)
- Unknown variant → forward remaining args to user Connector's `connect` field

**New builtins:**
- `tls-layer` — Handle form TLS upgrade; requires Handle refactor
- `quic-session` — QUIC session via quinn; 4 args: cap host port opts
- `http2-session` — HTTP/2 session via reqwest/h2; 1 arg: Handle[Stream Tls]
- `http3-session` — HTTP/3 session via h3/reqwest; 1 arg: QuicSession
- `http-request` — unified request across Http2Session and Http3Session
- `quic-open-stream` — open a reliable stream from QuicSession
- `quic-open-datagram` — open a datagram channel from QuicSession (RFC 9297)
- `icmp-ping` — platform-conditional; returns `{ok: {latency-ms: Int}}` or `{err: String}`

**`connect` extended:** UnixStream, UnixDatagram, NamedPipe transport variants with DirCap.

**Impact:** Moderate. The `connect` signature change is breaking for callers using `Udp` with host+port (they still work; the change only affects transports where port was previously required but meaningless).

### Value Types (`src/value.rs`)

New value variants:
- `Value::QuicSession` — opaque QUIC session (quinn)
- `Value::Http2Session` — opaque HTTP/2 session (reqwest/h2)
- `Value::Http3Session` — opaque HTTP/3 session

Handle modification:
- `raw_tcp: Option<std::net::TcpStream>` field added to `Value::Handle`

**Impact:** Moderate.

### `stdlib/net.llt`

Updated to use new primitives: `http-get` uses explicit Handle from `connect` rather than constructing connections internally. `fetch` dispatches across HTTP versions. `http-connect-layer` replaces the removed `proxy-connect`. Bug fixed: `parse-http-response` Sequential binding moved to helper function.

**Impact:** Minor — replaces existing implementation; existing callers of `fetch` are unchanged.

### New `stdlib/protocols/` Files

Four new files, each pure tinct, each standalone:
- `socks5.llt`, `dns.llt`, `grpc.llt`, `websocket.llt`

**Impact:** Additive.

### Cargo Dependencies

- `quinn` — QUIC implementation (new direct dep; already present? check lock)
- `ipnet` — CIDR range matching in NetCap (new direct dep; already transitive)
- No other new required deps: reqwest (HTTP/2 via `h2` feature), h3 via reqwest `http3` feature

**Impact:** Minor — `quinn` is the only substantial new dep; it is mature (used by many projects) and pure Rust.

## Prerequisites

- **Handle refactor** — `raw_tcp: Option<TcpStream>` field in `Value::Handle`; required for `tls-layer` (STARTTLS use case). Can be implemented independently of the rest.
- **lib-tls.md** — implemented (TLS Connector form, SPKI pinning, CA configuration). The lib-net-v2 Handle form (`tls-layer`) extends rather than replaces this.
- **Boolean Algebraic Subtyping** — not required for runtime behavior. With BAS, the type checker can express `Handle[R ∪ {Tls}]` as a proper type and verify Layer compatibility. Without BAS, the types are checked at runtime via the capability row.
- **`open` capability flag refactor** — required before `connect cap UnixStream path` can share the DirCap path-resolution infrastructure with `open cap path Readable`.

## References

- Bernstein, D.J. (2012). "QUIC: A UDP-Based Multiplexed and Secure Transport." — Quinn crate is the Rust implementation of RFC 9000; QUIC's integrated TLS replaces a separate tls-layer step.
- Bishop, M. (2022). "HTTP/3." *RFC 9114*. — HTTP/3 over QUIC; Http3Session API.
- Belshe, M., Peon, R. & Thomson, M. (2015). "Hypertext Transfer Protocol Version 2 (HTTP/2)." *RFC 7540*. — Http2Session API.
- Fette, I. & Melnikov, A. (2011). "The WebSocket Protocol." *RFC 6455*. — `protocols/websocket.llt` framing.
- Leech, M. et al. (1996). "SOCKS Protocol Version 5." *RFC 1928* + *RFC 1929*. — `protocols/socks5.llt` handshake.
- Mockapetris, P. (1987). "Domain Names — Implementation and Specification." *RFC 1035*. — `protocols/dns.llt` wire format.
- Miller, M.S. (2006). *Robust Composition: Towards a Unified Approach to Access Control and Concurrency Control.* — Object-capability model; capability routing by transport type (NetCap vs DirCap).
- Iyengar, J. & Thomson, M. (2021). "QUIC: A UDP-Based Multiplexed and Secure Transport." *RFC 9000*. — QUIC as a Session primitive; quinn crate.
- Pauly, T. et al. (2023). "HTTP Datagrams and the Capsule Protocol." *RFC 9297*. — `quic-open-datagram` enabling datagram extensions over HTTP/3.
- Thomson, M. & Turner, S. (2021). "Using TLS to Secure QUIC." *RFC 9001*. — QUIC's integrated TLS replaces the separate tls-layer step.
