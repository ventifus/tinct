# What If: Network Serve and Connect Layers (lib-net-v3)

**State:** Draft — 2026-05-19

**Depends on:** [`runtime-v2.md`](runtime-v2.md) — requires `task`/`await`/`channel`/`select-once`, `Arc`-based thunks, and the async Tokio runtime to be complete. All concurrency primitives (`context`, `with-timeout`, `finally`, `signal-channel`) are defined in runtime-v2 and used here without redefinition.

**Extends:** [`completed/lib-tls.md`](completed/lib-tls.md), [`completed/lib-net-v2.md`](completed/lib-net-v2.md) — adds the compositional serve/connect layer model, HTTP/1.1 as pure tinct, and the client-side multiplexing model.

---

## Problem

After runtime-v2 provides the async foundation (`task`, `await`, `channel`, `select-once`, `tcp-listen`/`quic-listen` primitives), building a network server requires boilerplate: accept a connection, hand it off to a task, loop. Every protocol layer adds the same pattern. There is no compositional model for stacking protocol layers, and no separation between transport (how bytes move) and application protocol (what the bytes mean).

---

## The Proposal

Define the **serve/connect layer model** as composable tinct stdlib functions. Transport primitives (thin Rust builtins producing `Channel@Handle` or `Channel@QuicConn`) are composed with protocol layers via two generic factory functions. Client-side follows the same symmetry. Application protocols are codecs — pure marshal/unmarshal — independent of transport.

---

## The Symmetry

```text
                  Server (receive)                Client (initiate)
                  ──────────────────────          ──────────────────────
Transport     resource → Channel@A            resource → A
1:1 layer     Channel@A → Config → Channel@B  A → Config → B   (*-layer)
1:N layer     Channel@A → Channel@B           A → RequestClient@B
```

A "layer violation" — a protocol that skips or reorders traditional stack layers — is expressed through its input and output types. QUIC produces `QuicConn`, not `Handle`. HTTP/3 expects `QuicConn`. They compose. `tls-serve` expects `Channel@Handle`; it does not compose with `Channel@QuicConn` (QUIC already has TLS). No special cases: the type system is the rule.

---

## Server Layers

### `make-serve-layer` and `make-multiplex-serve`

```tinct
# 1:1 — each incoming connection is transformed into one outgoing connection
# (TLS handshake, WireGuard, Noise Protocol, SSH transport, ...)
make-serve-layer: [fn [let accept-fn]
  [fn [let conn-ch config]
    [out: [channel 100]]
    [task [loop [fn [let]
      [match [recv conn-ch]
        [case [let raw: Ok]  [send out [accept-fn raw config]]]
        [case [let _: Err]   null]]]]]    # channel closed — stop
    out]]

# 1:N — each incoming connection produces multiple items
# (HTTP/1.1 keep-alive, HTTP/2 streams, HTTP/3 QUIC streams, WebSocket frames, ...)
make-multiplex-serve: [fn [let conn-fn]
  [fn [let conn-ch]
    [out: [channel 1000]]
    [task [loop [fn [let]
      [match [recv conn-ch]
        [case [let conn: Ok]  [task [conn-fn conn out]]]
        [case [let _: Err]    null]]]]]
    out]]
```

Concrete serve layers:

```tinct
# Connection-promotion: one connection in, one upgraded connection out
tls-serve:       [make-serve-layer tls-accept]        # Handle   → TlsHandle
wireguard-serve: [make-serve-layer wireguard-accept]  # Handle   → WgHandle
noise-serve:     [make-serve-layer noise-accept]      # Handle   → NoiseHandle
h2-serve:        [make-serve-layer h2-accept]         # Handle   → H2Conn
h3-serve:        [make-serve-layer h3-accept]         # QuicConn → H3Conn
ws-serve:        [make-serve-layer ws-accept]         # Handle   → WsConn

# Message-extraction: one connection in, many protocol messages out
http1-serve:     [make-multiplex-serve http1-conn]    # Handle → RawRequest*
http2-requests:  [make-multiplex-serve http2-req-conn]# H2Conn → RawRequest*
http3-requests:  [make-multiplex-serve http3-req-conn]# H3Conn → RawRequest*
```

`ws-serve` is connection-promotion even though WebSocket is bidirectional — `WsConn` satisfies the `Handle` byte-stream interface, so any `*-serve` can be stacked on top.

`h2-serve` and `h3-serve` produce `H2Conn`/`H3Conn` (not `Channel@RawRequest`) because HTTP/2 and HTTP/3 are bidirectional. The extraction layers sit on top when you want the request-response view.

A complete stack (HTTP over TLS over WireGuard over Unix socket):

```tinct
raw:  [unix-listen dir-cap "/var/run/app.sock"]
wg:   [wireguard-serve raw wg-config]
tls:  [tls-serve wg server-cert]
reqs: [http1-serve tls]
```

---

## Client Layers

Client-side 1:1 layers (`*-layer`) are the dual of `make-serve-layer` applied to a single connection:

```tinct
raw:    [tcp-connect cap "host" 443]
tls:    [tls-layer raw tls-config]
wg:     [wireguard-layer tls wg-config]
resp:   [http1-request tls [method: "GET" path: "/"]]
```

For 1:N multiplexing on the client — HTTP/2 and HTTP/3 — a **request client** manages stream multiplexing internally:

```tinct
# HTTP/2: TlsHandle → Http2Client
tls:    [tls-layer [tcp-connect cap "host" 443] tls-config]
client: [http2-client tls]
r1:     [http-request client [method: "GET" path: "/api/users"]]
r2:     [http-request client [method: "GET" path: "/api/posts"]]
[u p]:  [await-all r1 r2]    # both in-flight concurrently

# HTTP/3: QuicConn → Http3Client
conn:   [quic-connect cap "host" 443]
client: [http3-client conn]
```

`http-request` returns `Task@RawResponse` immediately — never blocking. `http2-client` and `http3-client` are Rust builtins (stream-ID correlation and flow control require tight protocol integration).

---

## Bidirectional Connections

For protocols where either side can initiate — HTTP/2 server push, HTTP/3, WebSocket — the connection type is the same on both sides. `h2-serve` and `h2-connect` both produce `H2Conn`.

```tinct
# Stream access for byte-level tunneling
h2-open-stream:   H2Conn   → Handle
h3-open-stream:   H3Conn   → Handle

# Adapter: multiplexed connection back to Channel@Handle
quic-stream-ch:  Channel@QuicConn → Channel@Handle
h2-stream-ch:    Channel@H2Conn   → Channel@Handle
h3-stream-ch:    Channel@H3Conn   → Channel@Handle

# WireGuard over H3 streams
h3-ch:  [h3-serve [quic-listen cap 443]]
s-ch:   [h3-stream-ch h3-ch]
wg-ch:  [wireguard-serve s-ch config]
```

---

## Transport-Agnostic Application Protocols

Application protocols are codecs — pure marshal/unmarshal between transport messages and domain objects. The `respond` closure pattern makes them transport-independent:

```tinct
# Domain message type carries a respond function
AppMessage: { ...fields...  respond: Fn@Null [AppResponse] }

# Codec adapter: Channel@TransportMsg → Channel@AppMessage
my-server: [fn [let transport-ch]
  [map [fn [let msg]
    [...fields decoded from msg...
     respond: [fn [let reply] [msg.respond [encode reply]]]]]
  transport-ch]]

# Application uses it without knowing the transport
[app-loop [my-server [http3-requests [h3-serve [quic-listen cap 443]]]]]
```

### DNS as the Worked Example

```tinct
# stdlib/dns.llt

dns-resolve: [fn [let resolver name type]
  [await [resolver [name: name type: type id: [random-id]]]]]

# All identical from dns-resolve's perspective:
[dns-resolve udp-resolver  "example.com" A]
[dns-resolve doh-resolver  "example.com" A]
[dns-resolve dot-resolver  "example.com" A]

# Resolver factories — return Fn@[Task DnsResponse] [DnsQuery]
dns-udp-resolver: [fn [let cap host port]
  [fn [let q] [task [decode-dns-wire [udp-request cap host port [encode-dns-wire q]]]]]]

dns-tls-resolver: [fn [let cap host port]
  [fn [let q] [task [dns-tls-send cap host port q]]]]

dns-https-resolver: [fn [let cap url]
  [client: [http-client cap url]]
  [fn [let q] [task [dns-https-send client q]]]]

# Server — loop is transport-agnostic
dns-server-loop: [fn [let query-ch]
  [loop [fn [let]
    [q: [recv query-ch]]
    [task [q.respond [dns-lookup q.name q.type]]]]]]

# Plug in any transport
dns-server-loop [dns-udp-server net-cap 53]
dns-server-loop [dns-tls-server [tls-serve [tcp-listen net-cap 853] cert]]
dns-server-loop [dns-https-server [http3-requests [h3-serve [quic-listen net-cap 443]]]]
```

---

## Worked Example: ICMP Ping Tunnel over H3

```tinct
[
  cap:            %net-cap
  port:           [@Port 4500]
  probe-interval: [seconds 5]

  quic-ch:   [quic-listen cap port]
  h3-ch:     [h3-serve quic-ch]
  stream-ch: [h3-stream-ch h3-ch]

  clients:   [channel 256]
  tick:      [timer-channel %clock probe-interval]

  serve-icmp-stream: [fn@Null [let stream@Handle]
    [loop [fn [let]
      [pkt: [read-icmp stream]]
      [match pkt
        [case [let req: EchoRequest]  [send-icmp stream [EchoReply req.id req.seq req.data]]]
        [case [let rep: EchoReply]    [log [str "RTT reply seq=" rep.seq]]]
        [case [let _]                 null]]]]]

  probe-all-clients: [fn@Null [let clients-ch scheduled]
    [lag:    [timestamp-diff [now %clock] scheduled]
     seq:    [random-id]
     probe:  [EchoRequest 0 seq "rtt-probe"]
     active: [collect-channel clients-ch]]
    [par-map [fn [let stream] [send-icmp stream probe] [send clients-ch stream]] active]
    [log [str "lag=" lag "ms  clients=" [length active]]]]
]
[loop [fn [let]
  [select
    [stream-ch [fn [let stream] [send clients stream] [task [serve-icmp-stream stream]]]]
    [tick      [fn [let scheduled] [probe-all-clients clients scheduled]]]]]]
```

**What this demonstrates:** H3 as byte transport (not HTTP), bidirectional H3Conn, per-connection tasks, shared client registry via channel, timer-driven server-initiated work, `par-map` fan-out, transport-agnostic ICMP logic.

---

## New Rust Primitives

| Primitive | Signature | Description |
|-----------|-----------|-------------|
| `tcp-listen` | `NetCap → Int → Channel@Handle` | Incoming TCP connections |
| `quic-listen` | `NetCap → Int → Channel@QuicConn` | Incoming QUIC connections |
| `unix-listen` | `DirCap → Str → Channel@Handle` | Incoming Unix socket connections |
| `tcp-listen-on` | `WgTunnel → Int → Channel@Handle` | TCP over WireGuard virtual network |
| `wireguard-bind` | `NetCap → WgConfig → WgTunnel` | Start WireGuard server |
| `wireguard-dial` | `NetCap → WgConfig → WgTunnel` | Connect as WireGuard peer |
| `tcp-connect-on` | `WgTunnel → Str → Int → Handle` | TCP through WireGuard tunnel |
| `tls-accept` | `Handle → TlsServerConfig → TlsHandle` | Server-side TLS handshake |
| `wireguard-accept` | `Handle → WgServerConfig → WgHandle` | Server-side WireGuard |
| `noise-accept` | `Handle → NoiseConfig → NoiseHandle` | Server-side Noise Protocol |
| `h2-accept` | `Handle → H2ServerConfig → H2Conn` | Server-side HTTP/2 |
| `h3-accept` | `QuicConn → H3ServerConfig → H3Conn` | Server-side HTTP/3 |
| `ws-accept` | `Handle → WsServerConfig → WsConn` | Server-side WebSocket |
| `h2-connect` | `TlsHandle → H2ClientConfig → H2Conn` | Client-side HTTP/2 |
| `h3-connect` | `QuicConn → H3ClientConfig → H3Conn` | Client-side HTTP/3 |
| `ws-connect` | `Handle → WsClientConfig → WsConn` | Client-side WebSocket |
| `h2-open-stream` | `H2Conn → Handle` | Raw H2 stream as byte pipe |
| `h3-open-stream` | `H3Conn → Handle` | Raw H3/QUIC stream as byte pipe |
| `http-request` | `H2Conn\|H3Conn → RawRequest → Task@RawResponse` | Send HTTP request; returns immediately |
| `h2-push` | `H2Conn → PushPromise → RawResponse → Null` | HTTP/2 server push |
| `h2-incoming` | `H2Conn → Channel@H2Message` | Incoming requests or server pushes |
| `ws-send` | `WsConn → WsFrame → Null` | Send a WebSocket frame |
| `ws-incoming` | `WsConn → Channel@WsFrame` | Incoming WebSocket frames |
| `http2-client` | `TlsHandle → Http2Client` | HTTP/2 request client (stream multiplexing) |
| `http3-client` | `QuicConn → Http3Client` | HTTP/3 request client |

---

## Stdlib Module Map

```text
stdlib/
  net.llt           — Port type, parse-url, url-encode/decode, form-encode/decode, resolve-host
  http1.llt         — HTTP/1.1 framing in pure tinct on top of Handle:
                        parse-request, write-response, serve-conn (server)
                        build-request, send-request, parse-response (client)
  http3.llt         — thin wrapper around h3-request Rust builtin; same RawRequest/RawResponse shape
  serve.llt         — make-serve-layer, make-multiplex-serve
                      Connection-promotion: tls-serve, wireguard-serve, noise-serve,
                        h2-serve, h3-serve, ws-serve
                      Message-extraction: http1-serve, http2-requests, http3-requests
                      Stream adapters: quic-stream-ch, h2-stream-ch, h3-stream-ch
  http.llt          — http-channel (unified server: TCP+QUIC)
                      fetch, fetch-h1, fetch-h3 (client)
                      router, headers-map, parse-query
                      ok, json-ok, redirect, not-found, server-error
                      with-logging, with-cors, with-auth, with-timeout (middleware)
  dns.llt           — dns-resolve, resolver factories (udp/tls/https/quic), dns-server-loop
  protocols/        — SOCKS5, gRPC, MQTT, WebSocket framing, custom codecs
```

**HTTP/1.1 request lifecycle — where the Rust/tinct boundary sits:**

```text
OS TCP accept → Rust: tcp-listen → Value::Handle
              → tinct: stdlib/http1.llt parse-request (text parsing)
              → tinct: stdlib/serve.llt pump attaches respond fn
              → tinct: user handler (router, headers-map, json-ok — all tinct)
              → tinct: stdlib/http1.llt write-response serializes to wire format
              → Rust: write-handle sends bytes through Handle
OS TCP send
```

---

## What Would Change

### Add Rust primitives

All primitives in the New Rust Primitives table above, registered in `standard_builtins()`.

### Add stdlib modules

`stdlib/net.llt`, `stdlib/http1.llt`, `stdlib/http3.llt`, `stdlib/serve.llt`, `stdlib/http.llt`, `stdlib/dns.llt`, `stdlib/protocols/`.

### Change `Cargo.toml`

- **Add** `h3` — HTTP/3 framing (QPACK) on top of quinn; for `quic-listen` and the `h3-request` builtin used by `stdlib/http3.llt`
- **Remove** `hyper` — HTTP/1.1 framing moves to `stdlib/http1.llt`
- **Remove** `reqwest` — HTTP client moves to `stdlib/http.llt` on top of `tcp-connect` + `tls-layer`

(`tokio-util`, `notify`, `num_cpus` are added by runtime-v2, not here.)

---

## Prerequisites

- [`runtime-v2.md`](runtime-v2.md) complete — `task`, `await`, `channel`, `select-once`, `context`, `with-timeout`, `finally`, `tcp-connect`, `quic-connect`, `tls-layer`, `Arc`-based async runtime all present

---

## References

- Marlow, S. et al. (2009). "Runtime Support for Multicore Haskell." *ICFP '09*. — `par`/`seq` sparks; implicit parallelism that underlies the serve/connect layer model.
- Syme, D., Petricek, T. & Lomov, D. (2011). "The F# Asynchronous Programming Model." *PADL '11*. — Async workflows as first-class values; request-client pattern.
- Go language specification. "Select statements." — `select` over channels; `select-once` is the primitive.
- Leijen, D., Schulte, W. & Burckhardt, S. (2009). "The Design of a Task Parallel Library." *OOPSLA '09*. — `await-all`/`await-any` semantics.
