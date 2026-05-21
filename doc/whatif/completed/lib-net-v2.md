# What If: Composable Networking for tinct (lib-net-v2)

**State:** Accepted — 2026-05-09

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

These compose left-to-right. An HTTP/2-over-SOCKS5 stack (TCP tunnel → TLS → HTTP/2):

```tinct
[include %libdir "net.llt"]
[include %libdir "protocols/socks5.llt"]

# 1. Connector: TCP connection to proxy (NetCap)
[tcp:   [connect %nc Tcp "proxy.corp" 1080]]

# 2. Layer: SOCKS5 tunnel to target (NetCap re-validated for tunnel target)
[tun:   [socks5-layer tcp %nc "api.internal" 443 null]]

# 3. Layer: TLS on the tunneled stream (negotiate h2 via ALPN)
[tls:   [tls-layer tun "api.internal" [alpn: ["h2"]]]]

# 4. Session: HTTP/2 over the tunneled TLS connection
[h2:    [http2-session tls]]

# 5. Application: make a request
[r:     [http-request h2 "GET" "/v1/services" [] null]]
```

HTTP/3 (QUIC) requires a direct UDP socket and cannot be proxied through a SOCKS5 TCP
CONNECT tunnel. For HTTP/3, `quic-session` opens its own UDP socket directly:

```tinct
[quic:  [quic-session %nc "api.example.com" 443 [alpn: ["h3"]]]]
[h3:    [http3-session quic]]
[r:     [http-request h3 "GET" "/v1/services" [] null]]
```

Proxying HTTP/3 through a tunnel requires MASQUE (RFC 9298) or SOCKS5 UDP ASSOCIATE —
future work outside this design.

Or, using the `fetch` convenience function:

```tinct
[fetch [socks5-layer [connect %nc Tcp "proxy.corp" 1080] %nc "api.internal" 443]
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

**Implementation note:** `cap_std::fs::Dir::connect_unix_stream()` is not yet implemented upstream (cap-std 3.4.x). The implementation must use `openat2` with `RESOLVE_BENEATH` to resolve the path to a file descriptor, then connect via that fd — do not use `PathBuf::join` + raw `UnixStream::connect`, which bypasses the capability sandbox.

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
# Architectural: deferred (requires dtls Rust dep + datagram-aware Handle I/O —
# the BufRead/Write interface loses UDP message boundaries that DTLS requires)
[dtls-layer   handle@Handle sni@String opts@Dict]
  → Handle[... Datagram Tls]

# SOCKS5 proxy tunnel — pure tinct in protocols/socks5.llt
# cap@NetCap is re-validated against the tunnel target to prevent SSRF
[socks5-layer handle@Handle cap@NetCap host@String port@Int creds@[Dict Null]]
  → Handle[... Stream]

# HTTP CONNECT tunnel — pure tinct in net.llt
# cap@NetCap is re-validated against the tunnel target to prevent SSRF
[http-connect-layer handle@Handle cap@NetCap host@String port@Int headers@Dict]
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

**QUIC Session** — implemented in Rust via `quinn`. QUIC integrates transport, TLS, and reliable ordered delivery at the UDP level. `quinn` requires exclusive control of the UDP socket after creation (it manages path migration, congestion control, ACKs internally — it can accept a pre-bound socket but must be the sole consumer after handoff), so `quic-session` is Connector-style rather than a Layer over an existing UDP Handle:

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

# fetch-https negotiates the best available protocol. HTTP/2 is selected via
# ALPN in TLS (h2 token). HTTP/3 requires prior discovery: an Alt-Svc header
# cached from a previous response, or a DNS HTTPS record (RFC 9460). The first
# request to a new server always uses HTTP/1.1 or HTTP/2 — Alt-Svc is a cache
# mechanism, not first-visit discovery. Alt-Svc redirects to a different host
# are validated against the NetCap before the upgrade connection is opened.
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
[tun:   [socks5-layer proxy %nc "internal-api.corp" 443 null]]
[tls:   [tls-layer tun "internal-api.corp" opts]]
```

Supports CONNECT command only (TCP tunneling — RFC 1928 §4): no-auth mode, username/password auth (RFC 1929), IPv4/IPv6/hostname target address. BIND (§6) and UDP ASSOCIATE (§7) are out of scope. Actual line count closer to 80–120 once all address types, auth modes, and error paths are covered; requires binary I/O primitives (`read-bytes`, `byte-at`, `write-bytes`) that are not yet in stdlib.

**`protocols/dns.llt`** (~200–300 lines, pure tinct over UDP)

DNS wire protocol (RFC 1035). Constructs and parses DNS packets; requires binary I/O primitives (`read-bytes`, `byte-at`, `write-bytes`) for byte-level encoding. RFC 1035 §4.1.4 compression pointers (required in received messages) need random access into the packet bytes, loop detection (per RFC 9267), and a pointer-follow depth limit. Line count is closer to 200–300 for a correct implementation with all 8 record types.

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

`net.llt` contains: `fetch`, `http-get`, `parse-http-response`, `build-http-request`, `http-connect-layer`. URI utilities (`uri-params`, `uri-origin`, `uri->string`) are Rust builtins — always available without include. It does not contain any of the `protocols/` content.

### Handle Refactor for tls-layer

`tls-layer` (the Handle form of TLS upgrade, enabling STARTTLS) requires extracting the raw `TcpStream` from a Handle to hand off to rustls. This requires adding a `raw_tcp: Option<TcpStream>` field to `Value::Handle`, populated when `connect cap Tcp ...` creates the Handle and consumed (moved out) when `tls-layer` is called:

```rust
Value::Handle {
    caps: HashMap<String, Value>,
    inner: Box<dyn BufRead>,          // existing
    write_inner: Option<Box<dyn Write>>, // existing
    seek_inner: Option<...>,           // existing
    raw_tcp: Option<Rc<RefCell<Option<TcpStream>>>>,  // NEW — shared across clones
}
```

`raw_tcp` must be `Rc<RefCell<Option<TcpStream>>>` (not a plain `Option<TcpStream>`) because
`Value: Clone` is a fundamental contract — `TcpStream` is not `Clone`. All clones of a Handle
share the same `RefCell` slot. When `tls-layer` calls `.borrow_mut().take()` to extract the
stream, the `Option` becomes `None` in all clones. A second `tls-layer` call on any alias sees
`None` and produces a runtime error. Reads and writes on the original Handle's `inner` field
are unaffected — only further `tls-layer` calls fail. The new TLS Handle wraps the TLS stream
as before (TlsReader + TlsWriter sharing `Rc<RefCell<StreamOwned>>`).

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

- `quinn` — QUIC implementation (new direct dep; not currently in Cargo.toml); brings in `tokio` as a mandatory dependency — document the runtime strategy (see TODO `http-sessions`) before adding
- `ipnet` — CIDR range matching in NetCap (new direct dep; already transitive)
- No other new required deps: reqwest (HTTP/2 via `h2` feature), h3 via reqwest `http3` feature

**Impact:** Minor — `quinn` is the only substantial new dep; it is mature (used by many projects) and pure Rust.

## Prerequisites

- **Handle refactor** — `raw_tcp: Option<Rc<RefCell<Option<TcpStream>>>>` field in `Value::Handle`; required for `tls-layer` (STARTTLS use case). Can be implemented independently of the rest.
- **lib-tls.md** — implemented (TLS Connector form, SPKI pinning, CA configuration). The lib-net-v2 Handle form (`tls-layer`) extends rather than replaces this.
- **Boolean Algebraic Subtyping** — not required for runtime behavior. With BAS, the type checker can express `Handle[R ∪ {Tls}]` as a proper type and verify Layer compatibility. Without BAS, the types are checked at runtime via the capability row.
- **`open` capability flag refactor** — required before `connect cap UnixStream path` can share the DirCap path-resolution infrastructure with `open cap path Readable`.

## References

- Bishop, M. (2022). "HTTP/3." *RFC 9114*. — HTTP/3 over QUIC; Http3Session API.
- Belshe, M., Peon, R. & Thomson, M. (2015). "Hypertext Transfer Protocol Version 2 (HTTP/2)." *RFC 7540*. — Http2Session API.
- Fette, I. & Melnikov, A. (2011). "The WebSocket Protocol." *RFC 6455*. — `protocols/websocket.llt` framing.
- Leech, M. et al. (1996). "SOCKS Protocol Version 5." *RFC 1928* + *RFC 1929*. — `protocols/socks5.llt` handshake.
- Mockapetris, P. (1987). "Domain Names — Implementation and Specification." *RFC 1035*. — `protocols/dns.llt` wire format.
- Miller, M.S. (2006). *Robust Composition: Towards a Unified Approach to Access Control and Concurrency Control.* — Object-capability model; capability routing by transport type (NetCap vs DirCap).
- Iyengar, J. & Thomson, M. (2021). "QUIC: A UDP-Based Multiplexed and Secure Transport." *RFC 9000*. — QUIC as a Session primitive; quinn crate.
- Hardaker, W. et al. (2022). "Common Implementation Anti-Patterns Related to DNS Resource Record (RR) Processing." *RFC 9267*. — DNS compression pointer loop detection in `protocols/dns.llt`.
- Pauly, T. et al. (2023). "HTTP Datagrams and the Capsule Protocol." *RFC 9297*. — `quic-open-datagram` enabling datagram extensions over HTTP/3.
- Schwartz, B. et al. (2023). "Service Binding and Parameter Specification via the DNS." *RFC 9460*. — DNS HTTPS records as first-visit HTTP/3 discovery alternative to Alt-Svc.
- Thomson, M. & Turner, S. (2021). "Using TLS to Secure QUIC." *RFC 9001*. — QUIC's integrated TLS replaces the separate tls-layer step.
