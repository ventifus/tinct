# What If: Network Serve and Connect Layers (lib-net-v3)

**State:** Draft — 2026-05-21

**Depends on:** [`runtime-v2.md`](runtime-v2.md) ✓ complete — `task`/`await`/`channel`/`recv`/`send`/`select-once`/`loop-select`, `Arc`-based thunks, async Tokio runtime, and `stdlib/async.llt` (including `collect-channel`, `loop-select`, `exit`, `drain`) are all available. All concurrency primitives are used here without redefinition. `tcp-connect`, `quic-connect`, and `tls-layer` are **not** runtime-v2 primitives — they are tinct stdlib functions defined in this whatif.

**Extends:** [`completed/lib-tls.md`](completed/lib-tls.md), [`completed/lib-net-v2.md`](completed/lib-net-v2.md) — replaces the opaque-Rust-type model for TLS, HTTP/2, HTTP/3, QUIC, WebSocket, WireGuard, and Noise with tinct stdlib implementations; adds the compositional serve/connect layer model; introduces `[Bytes N]` fixed-size byte types and a `cap-std::Pool`-backed `NetCap`.

---

## Problem

After runtime-v2 provides the async foundation (`task`, `await`, `channel`, `select-once`, `loop-select`), building a network server still requires boilerplate: accept a connection, hand it off to a task, loop. Every protocol layer adds the same pattern. There is no compositional model for stacking protocol layers, no separation between transport (how bytes move) and application protocol (what the bytes mean), and no typed representation for fixed-size byte sequences like IP addresses and cryptographic keys.

---

## The Proposal

Define the **serve/connect layer model** as composable tinct stdlib functions. OS-level transport primitives (`tcp-bind`/`tcp-accept`, `udp-socket`/`udp-recv`/`udp-send`) are the only Rust boundary; everything above — accept loops, connection multiplexing, protocol handshakes, HTTP/2, QUIC, TLS — is tinct. Client-side follows the same symmetry. Application protocols are codecs — pure marshal/unmarshal — independent of transport. `[Bytes N]` fixed-size byte sequences give the crypto and address types static size guarantees.

---

## The Symmetry

```text
                  Server (receive)                Client (initiate)
                  ──────────────────────          ──────────────────────
Transport     resource → Channel@A            resource → A
1:1 layer     Channel@A → Config → Channel@B  A → Config → B   (*-layer)
1:N layer     Channel@A → Channel@B           A → RequestClient@B
```

A "layer violation" — a protocol that skips or reorders traditional stack layers — is expressed through its input and output types. `quic-listen` (tinct, in `net.llt`) produces `Channel@QuicConnection`; HTTP/3 expects `QuicConnection`. They compose. `tls-serve` expects `Channel@[ByteStream h]`; it does not compose with `Channel@QuicConnection` (QUIC carries integrated TLS). No special cases: the type system is the rule.

---

## Connection Interfaces: `ByteStream` and `Datagram`

Two typeclasses cover all network IO. Every transport, tunnel, and framing layer is either a byte stream or a datagram socket — no other fundamental shapes exist.

### `ByteStream`

An ordered, reliable, connection-oriented byte pipe. Reading and writing are symmetric; addressing was resolved at connection time.

```tinct
[class [ByteStream h]
  read-bytes:  [Fn [h@h n@Int] Bytes]
  write-bytes: [Fn [h@h data@Bytes] Null]]
```

`Handle` (OS TCP connection from `tcp-accept`/`tcp-connect`) is the base instance, backed by the Rust `read-bytes`/`write-bytes` primitives. Every protocol layer that sits on top of a stream produces a new `ByteStream` instance — a tinct record wrapping the layer below:

```tinct
# stdlib/net.llt
[instance [ByteStream Handle]
  read-bytes:  builtin-read-bytes    # Rust: tokio AsyncRead
  write-bytes: builtin-write-bytes]  # Rust: tokio AsyncWrite

# stdlib/protocols/tls.llt
TlsConnection: [type
  [underlying: Handle
   write-key:  [Bytes 32]
   read-key:   [Bytes 32]
   cipher:     Symbol            # ChaCha20Poly1305 | Aes128Gcm | Aes256Gcm
   write-seq:  [Channel Int]     # TLS sequence number XOR'd with implicit IV
   read-seq:   [Channel Int]]]
[instance [ByteStream TlsConnection]
  read-bytes:  tls-read-bytes    # reads one TLS record, decrypts
  write-bytes: tls-write-bytes]  # encrypts, frames as TLS record

# stdlib/protocols/wireguard.llt
WireguardConnection: [type
  [underlying: Handle             # the byte stream below (TCP or any ByteStream)
   tx-key:     [Bytes 32]
   rx-key:     [Bytes 32]
   tx-nonce:   [Channel Int]]]    # atomic counter via channel — single-writer invariant:
                                  # only one task may call wg-write-bytes on a given
                                  # WireguardConnection at a time. If violated, two tasks
                                  # both recv the same nonce → same keystream for different
                                  # plaintexts → ChaCha20-Poly1305 keystream reuse → full
                                  # confidentiality loss for both messages.
                                  # Nonce must not exceed 2^64−1; implementation must reject
                                  # or trigger rekeying at the limit (WireGuard wire nonce
                                  # is 64-bit; tinct Int is unbounded).
[instance [ByteStream WireguardConnection]
  read-bytes:  wg-read-bytes     # reads one WG frame, decrypts
  write-bytes: wg-write-bytes]   # encrypts, stamps nonce, frames

# stdlib/protocols/noise.llt
NoiseConnection: [type [underlying: Handle  send-key: [Bytes 32]  recv-key: [Bytes 32]
                        send-n: [Channel Int]]]
[instance [ByteStream NoiseConnection] read-bytes: noise-read write-bytes: noise-write]

# stdlib/protocols/websocket.llt — WebSocketConnection implements MessageStream, not ByteStream.
# WebSocket frames carry semantic types (text/binary/ping/pong/close) that
# ByteStream's read-bytes interface cannot express. See §MessageStream below.
WebSocketConnection: [type [underlying: Handle  server-side: Bool]]
# (WebSocketConnection instance declared in the MessageStream section)
```

The `Channel Int` nonce pattern is the tinct-idiomatic atomic counter: the channel always holds exactly one value; `recv` then `send` is an indivisible read-modify-write without extra primitives.

All `*-accept` and `*-layer` functions are parametric over `ByteStream`. This is what makes arbitrary layering possible — WireGuard carrying TLS carrying HTTP/2 carrying gRPC:

```tinct
h2-accept:        [fn@[bind: [t]  return: Http2Connection      constraint: [t: ByteStream]] [let h@t cfg@Http2ServerConfig]    ...]
tls-accept:       [fn@[bind: [t]  return: TlsConnection        constraint: [t: ByteStream]] [let h@t cfg@TlsServerConfig]      ...]
wireguard-accept: [fn@[bind: [t]  return: WireguardConnection  constraint: [t: ByteStream]] [let h@t cfg@WireguardServerConfig] ...]
```

`wireguard-serve` produces `Channel@WireguardConnection`; `tls-serve` accepts `Channel@[ByteStream h]` — so `[tls-serve [wireguard-serve raw-ch] cert]` type-checks without any collapse to `Handle`.

### `Datagram`

An unordered, unreliable packet socket. Each send and receive is a discrete unit with its own source/destination address. UDP, ICMP, the QUIC packet layer, and WireGuard's UDP transport are all `Datagram`.

```tinct
[class [Datagram d]
  send-datagram: [Fn [d@d addr@SocketAddress data@Bytes] Null]
  recv-datagram: [Fn [d@d] UdpDatagram]]
```

`UdpSocket` (from `udp-socket`) is the base instance:

```tinct
# stdlib/net.llt
[instance [Datagram UdpSocket]
  send-datagram: builtin-udp-send   # Rust: UdpSocket::send_to
  recv-datagram: builtin-udp-recv]  # Rust: UdpSocket::recv_from
```

QUIC is built on `Datagram` — `quic-accept-loop` in `stdlib/protocols/quic.llt` receives datagrams from a `UdpSocket`, demultiplexes by connection ID, and hands each QUIC stream to the caller as a `ByteStream`. The same `quic-accept-loop` would work over any `Datagram d`, enabling QUIC over custom datagram transports.

The two typeclasses interact at QUIC's boundary:

```text
Datagram (UdpSocket) → QuicConnection (stream multiplexer)
QuicConnection → ByteStream (each individual QUIC stream)
ByteStream → ByteStream (TLS, HTTP/2, application protocols)
```

`QuicConnection` is neither `ByteStream` nor `Datagram` — it is a stream multiplexer, a third category with its own access pattern (`quic-open-stream`, `quic-incoming`).

### `MessageStream`

A bidirectional typed-message interface. You write a typed value and the implementation serializes it to the underlying transport using whatever framing semantics the protocol requires; you read and receive one complete typed value. The caller is fully insulated from byte framing.

```tinct
[class [MessageStream s t]
  FD: [s → t]                        # knowing the stream type uniquely determines message type
  send-message: [Fn [s@s t] Null]   # serialize t → underlying transport
  recv-message: [Fn [s@s] t]]       # deserialize → one complete t
```

`Channel T` is the base instance — it already delivers typed values with no framing:

```tinct
[instance [MessageStream [Channel t] t]
  send-message: send
  recv-message: recv]
```

Protocol connections that carry typed messages implement `MessageStream` for their message type:

```tinct
# stdlib/protocols/websocket.llt
WsFrame: [union
  [Text   data@String]
  [Binary data@Bytes]
  [Ping   data@Bytes]
  [Pong   data@Bytes]
  [Close  code@Int  reason@String]]

WebSocketConnection: [type [underlying: Handle  server-side: Bool]]

[instance [MessageStream WebSocketConnection WsFrame]
  send-message: ws-send-frame    # encodes as WS frame, applies mask (client), writes to underlying
  recv-message: ws-recv-frame]   # reads from underlying, strips WS framing + mask, returns frame
```

`ws-serve` produces `Channel@WebSocketConnection`; the application calls `recv-message wsconn` to get a `WsFrame`, branches on its type, and calls `send-message wsconn reply`. gRPC bidirectional streams, MQTT connections, and custom application protocols all implement `MessageStream T` for their respective message types.

### Summary: the three IO shapes

| Typeclass | Unit | Framing | Base instance |
|---|---|---|---|
| `ByteStream` | `n` bytes on demand | caller's job | `Handle` (TCP, TLS, WireGuard, ...) |
| `Datagram` | one packet + address | per-packet | `UdpSocket` |
| `MessageStream T` | one complete `T` | protocol's job | `Channel T` |

### What Changes in the Implementation

- **New**: `ByteStream`, `Datagram`, and `MessageStream` class declarations in `stdlib/net.llt`
- **New**: `[instance [ByteStream Handle] ...]`, `[instance [Datagram UdpSocket] ...]`, and `[instance [MessageStream [Channel t] t] ...]` in `stdlib/net.llt`
- **Removed**: `make-encrypted-handle` Rust primitive — superseded by tinct `ByteStream` instances
- **Updated**: `read-bytes`, `write-bytes` Rust primitives become `Handle`'s `ByteStream` instance methods; `udp-send`, `udp-recv` become `UdpSocket`'s `Datagram` instance methods. They remain in the primitives table as the Rust backing but are exposed through typeclass dispatch.
- **Updated**: All `*-accept` signatures use `constraint: [h: ByteStream]`; protocol layers are typed records; `WebSocketConnection` implements `MessageStream WsFrame` rather than `ByteStream`

---

## Server Layers

### `channel-map` and `channel-flat-map`

These are general concurrent channel operators defined in `stdlib/async.llt` — not networking-specific. `channel-map` applies a function to each element concurrently (1:1); `channel-flat-map` applies a function that emits multiple outputs per element (1:N). The networking serve layers are just applications of these.

```tinct
# stdlib/async.llt

# 1:1 — apply f to each element concurrently, collect results.
# buf: output channel buffer size; tune to match downstream consumption rate.
# (TLS handshake, WireGuard upgrade, JSON parsing, format conversion, ...)
channel-map: [fn@[bind: [element result]] [let f@[Fn [element] result]]
  [fn@[return: [Channel result]] [let in-ch@[Channel element]  buffer-size@[type: Int  default: 64]]
    [out: [channel buffer-size]]
    [task [loop [fn []
      [x: [recv in-ch]]
      [task [send out [f x]]]]]]
    out]]

# 1:N — apply f to each element; f emits multiple items to the shared out channel.
# buf: output buffer; size to absorb bursts when concurrent f calls complete together.
# (HTTP/1.1 keep-alive requests, HTTP/2 streams, log lines from files, ...)
channel-flat-map: [fn@[bind: [element item]] [let f@[Fn [element [Channel item]] Null]]
  [fn@[return: [Channel item]] [let in-ch@[Channel element]  buffer-size@[type: Int  default: 256]]
    [out: [channel buffer-size]]
    [task [loop [fn []
      [x: [recv in-ch]]
      [task [f x out]]]]]
    out]]
```

Both loops are fire-and-forget tasks (`[task [loop ...]]`). `recv` returns `T` directly and suspends until a value is available — it does not return `Result`. Loop termination happens via context cancellation: when the server shuts down (e.g., `exit` or `drain` from `stdlib/async.llt`), `recv` on a cancelled context raises a cancellation error that propagates out of the loop task. No channel-closed signalling or explicit break is needed.

The inner tasks (`[task [send out [f x]]]`) are also fire-and-forget and are **not** cancelled when the outer loop exits — they run to completion or until they error. This is correct: aborting a half-completed TLS handshake would leave the client in a broken state. For clean server shutdown that waits for in-flight handshakes to complete, call `drain` from `stdlib/async.llt` at the top level before `exit`.

Concrete serve layers:

```tinct
# Config-bearing serve layers live alongside their protocol, not in serve.llt.
# Each protocol file defines its own *-serve using channel-map:

# stdlib/protocols/tls.llt
tls-serve:       [fn [let in-ch cfg@TlsServerConfig]
                   [channel-map [fn [let h] [tls-accept h cfg]] in-ch]]

# stdlib/protocols/wireguard.llt
wireguard-serve: [fn [let in-ch cfg@WireguardConfig]
                   [channel-map [fn [let h] [wireguard-accept h cfg]] in-ch]]

# stdlib/protocols/noise.llt
noise-serve:     [fn [let in-ch cfg@NoiseConfig]
                   [channel-map [fn [let h] [noise-accept h cfg]] in-ch]]

# Config-free layers (in stdlib/serve.llt — no protocol-specific imports needed):
h2-serve:        [fn [let in-ch] [channel-map h2-accept in-ch]]
h3-serve:        [fn [let in-ch] [channel-map h3-accept in-ch]]
ws-serve:        [fn [let in-ch] [channel-map ws-accept in-ch]]

# Message-extraction (also in serve.llt):
http1-serve:     [fn [let in-ch] [channel-flat-map http1-conn in-ch]]
http2-requests:  [fn [let in-ch] [channel-flat-map http2-req-conn in-ch]]
http3-requests:  [fn [let in-ch] [channel-flat-map http3-req-conn in-ch]]
```

The serve layers that need configuration (`tls-serve`, `wireguard-serve`, `noise-serve`) close over their config and pass a 1-arg closure to `channel-map` — satisfying `channel-map`'s `f: [Fn [element] result]` type. Config-free layers (`h2-serve`, `ws-serve`, etc.) pass `*-accept` directly since those functions take only the connection handle.

Constraint propagation: the closure `[fn [let h] [tls-accept h cfg]]` has type `[Fn [t] TlsConnection constraint: [t: ByteStream]]`. When `channel-map` unifies this with `f: [Fn [element] result]`, the `ByteStream` constraint on `element` propagates to `in-ch: [Channel element]` — passing `Channel@QuicConnection` is caught at the call site as a compile-time type error.

WireGuard is a user-mode protocol: Noise_IKpsk2 handshake in tinct, data plane over `udp-socket`, no kernel TUN/TAP.

`h2-serve` and `h3-serve` call tinct-implemented `h2-accept`/`h3-accept` from `stdlib/protocols/h2.llt` and `stdlib/protocols/h3.llt`. `Http2Connection` and `Http3Connection` are tinct records holding frame-parsing state, HPACK/QPACK tables, and stream channels — not opaque Rust types. They do **not** implement `ByteStream` — they are stream multiplexers, not byte pipes. Individual streams are accessed via `h2-open-stream`/`h3-open-stream` which return `Handle` (a `ByteStream`). The extraction layers (`http2-requests`, `http3-requests`) pull request streams from those records using tinct loops and channels.

A complete stack (HTTP over TLS over WireGuard over Unix socket):

```tinct
raw:  [unix-listen dir-cap "/var/run/app.sock"]
wg:   [wireguard-serve raw wg-config]
tls:  [tls-serve wg server-cert]
reqs: [http1-serve tls]
```

---

## Client Layers

Client-side 1:1 layers (`*-layer`) are the dual of `channel-map` applied to a single connection. `connect-host` (from `stdlib/net.llt`) implements RFC 8305 Happy Eyeballs entirely in tinct using runtime-v2 primitives:

```tinct
# stdlib/net.llt — RFC 8305 Happy Eyeballs v2

connect-host: [fn [let cap@NetCap host@String port@Int]
  [config: %dns]
  # RFC 8305 §5.1: if single-request is set (resolv.conf option), make A and AAAA
  # queries sequentially — some broken appliances reject parallel queries to port 53.
  # Both branches return [v6: ... v4: ...] so the rest of the function is uniform.
  [resolved:
    [if config.single-request
      # Sequential — AAAA first, then A (RFC 8305 still prefers IPv6)
      [v6: [match [try [dns-resolve cap host AAAA]] [Ok a]: a [Err _]: []]
       v4: [match [try [dns-resolve cap host A]]    [Ok a]: a [Err _]: []]]
      # Concurrent — both at once per RFC 8305 §5
      [v6-task: [task [dns-resolve cap host AAAA]]
       v4-task: [task [dns-resolve cap host A]]
       v6-first: [match [timeout [millis 50] v6-task]
         [Ok addrs]: addrs
         [Err _]:    []]
       v4: [await v4-task]
       v6: [if [= [] v6-first]
         [match [try [await v6-task]] [Ok a]: a [Err _]: []]
         v6-first]]]]
  # Interleave families (IPv6 preferred per RFC 6724), race with 250ms stagger
  [happy-connect cap port [interleave resolved.v6 resolved.v4]]]

happy-connect: [fn [let cap@NetCap port@Int addrs@[Seq IpAddress]]
  [result-ch:  [channel 1]
   attempt-ms: 250]
  [tasks: [collect [map-indexed [fn [let i addr]
    [task
      [await [timer-channel %clock (* i attempt-ms)]]   # stagger start time
      [match [try [tcp-connect cap [addr: addr  port: port]]]
        [Ok  h]: [try [send result-ch h]]   # first success wins; try: ignores closed-ch error
        [Err _]: null]]]                     # connection failed — let others continue
    addrs]]]
  [result: [recv result-ch]]                 # blocks until first success
  [par-map [fn [let t] [cancel-task t]] tasks]  # cancel remaining attempts
  result]
```

```tinct
raw:    [connect-host cap "host" 443]      # DNS + Happy Eyeballs + tcp-connect
tls:    [tls-layer raw [sni: "host" ...]]
wg:     [wireguard-layer tls wg-config]
resp:   [http1-request wg [method: "GET" path: "/"]]
```

For 1:N multiplexing on the client — HTTP/2 and HTTP/3 — a **request client** manages stream multiplexing internally (in `stdlib/protocols/h2.llt` and `stdlib/protocols/h3.llt`):

```tinct
# HTTP/2: Happy Eyeballs connect → TLS with h2 ALPN → http2-client
raw:    [connect-host cap "host" 443]
tls:    [tls-layer raw [sni: "host" alpn: ["h2"] ...]]
client: [http2-client tls]
r1:     [http-request client [method: "GET" path: "/api/users"]]
r2:     [http-request client [method: "GET" path: "/api/posts"]]
[u p]:  [await-all r1 r2]    # both in-flight concurrently

# HTTP/3: quic-connect handles DNS + UDP + QUIC-TLS internally
client: [http3-client [quic-connect cap "host" 443]]
```

`http-request` returns `Task@RawResponse` immediately — never blocking. `http2-client`, `http3-client`, `quic-connect`, and `connect-host` are all tinct stdlib functions.

---

## Bidirectional Connections

For protocols where either side can initiate — HTTP/2 server push, HTTP/3, WebSocket — the connection type is the same on both sides. `h2-serve` and `h2-connect` both produce `Http2Connection`; `h3-serve` and `h3-connect` both produce `Http3Connection`. These are tinct records, so stream access and channel adapters are tinct stdlib functions in `h2.llt`/`h3.llt`/`quic.llt`:

```tinct
# Stream access for byte-level tunneling (stdlib/protocols/h2.llt, h3.llt)
h2-open-stream:  [Fn [c@Http2Connection] Handle]     # opens a new H2 stream as a raw Handle
h3-open-stream:  [Fn [c@Http3Connection] Handle]     # opens a new H3/QUIC stream

# Adapters: one-stream-per-connection flattening (stdlib/protocols/)
quic-stream-ch:  [Fn [ch@[Channel QuicConnection]] [Channel Handle]]
h2-stream-ch:    [Fn [ch@[Channel Http2Connection]]   [Channel Handle]]
h3-stream-ch:    [Fn [ch@[Channel Http3Connection]]   [Channel Handle]]

# WireGuard tunnelled over H3 streams — all tinct
h3-ch:  [h3-serve [quic-listen cap 443]]
s-ch:   [h3-stream-ch h3-ch]
wg-ch:  [wireguard-serve s-ch config]
```

`quic-stream-ch`, `h2-stream-ch`, and `h3-stream-ch` open exactly one stream per incoming connection. Use `h2-open-stream`/`h3-open-stream` directly when a single `Http2Connection`/`Http3Connection` needs multiple streams.

---

## Transport-Agnostic Application Protocols

Application protocols are codecs — pure marshal/unmarshal between transport messages and domain objects. The `respond` closure pattern makes them transport-independent. Each protocol message type carries a `respond` field that is a closure capturing the transport-specific response path:

```tinct
# Protocol message type — fields are protocol-specific; respond: is the convention.
# The respond closure hides the transport completely.
AppMessage: [type
  [id:      Int
   payload: Bytes
   respond: [Fn [Bytes] Null]]]   # send a response back to the peer

# Codec adapter: Channel@RawRequest → Channel@AppMessage
# Decodes the transport message and wraps its respond in a protocol-aware closure.
my-codec: [fn [let raw-ch]
  [map [fn [let raw]
    [id:      [decode-id raw.payload]
     payload: [decode-payload raw.payload]
     respond: [fn [let reply] [raw.respond [encode-reply reply]]]]]
  raw-ch]]

# Application layer sees only AppMessage — transport is invisible
[app-loop [my-codec [http3-requests [h3-serve [quic-listen cap 443]]]]]
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
# addr is a pre-resolved SocketAddress (resolver IPs come from system config, not DNS)
dns-udp-resolver: [fn [let cap addr@SocketAddress]
  [sock: [udp-ephemeral cap]]  # bind 0.0.0.0:0 — ephemeral port assigned by OS
  [fn [let q] [task
    [send-datagram sock addr [encode-dns-wire q]]
    [decode-dns-wire [recv-datagram sock].data]]]]

dns-tls-resolver: [fn [let cap addr@SocketAddress sni@String]
  [fn [let q] [task
    [raw: [tcp-connect cap addr]]
    [tls: [tls-layer raw [sni: sni ...]]]
    [dns-framed-send tls q]]]]

dns-https-resolver: [fn [let cap url]
  [client: [http3-client [quic-connect cap url.host url.port]]]
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
[loop-select
  [stream-ch [fn [let stream]    [send clients stream] [task [serve-icmp-stream stream]]]]
  [tick      [fn [let scheduled] [probe-all-clients clients scheduled]]]]
```

**What this demonstrates:** H3 as byte transport (not HTTP), bidirectional Http3Connection, per-connection tasks, shared client registry via channel, timer-driven server-initiated work, `par-map` fan-out, transport-agnostic ICMP logic.

---

## Worked Example: Simple HTTP Server

The NetCap is not a binary "can access the network" flag — it is a specific grant of address and port. A development server that only needs to listen on localhost gets a minimal, precise capability:

```sh
tinct run --cap-net 127.0.0.1:8080 server.llt
```

This grant lets the program bind on localhost:8080 and do nothing else with the network. It cannot connect to external hosts, cannot bind on other ports, and cannot accidentally become a public-facing server. The capability model makes the program's intent explicit and verifiable from the command line.

```tinct
# Run with: tinct run --cap-net server@b=127.0.0.1:8080 server.llt
[
  cap:      %server                     # Bindable on 127.0.0.1:8080 only
  port:     [@Port 8080]
  requests: [http-channel cap port]    # tcp-listen + http1-serve on localhost

  handler: [router
    ["/hello":   [fn [let _] [ok "world"]]]
    ["/healthz": [fn [let _] [ok "ok"]]]]
]
[loop [fn []
  [req: [recv requests]]
  [task [req.respond [handler req]]]]]
```

**What this demonstrates:** the full TCP → HTTP/1.1 stack hidden behind `http-channel`; `router` for path dispatch; `ok` for text responses; a `loop` that concurrently handles each request in its own task via `req.respond`; and the NetCap as a precise, minimal network grant rather than a broad permission.

Extending to a public HTTPS server requires a wider cap and a certificate:

```sh
tinct run --cap-net listen@b=0.0.0.0:443 --cap-fs certs@r=./certs server.llt
```

```tinct
[
  cap:      %listen                              # Bindable on 0.0.0.0:443 only
  key-cap:  %certs                              # ./certs read-only
  port:     [@Port 443]
  cert:     [slurp-secret key-cap "server.pem"]
  requests: [http-channel-tls cap port cert]      # tcp-listen + tls-serve + http1-serve

  handler: [router
    ["/hello":   [fn [let _] [ok "world"]]]
    ["/healthz": [fn [let _] [ok "ok"]]]]
]
[loop [fn []
  [req: [recv requests]]
  [task [req.respond [handler req]]]]]
```

---

## Worked Example: HTTP Client with SVCB/HTTPS Records

HTTPS DNS records (RFC 9460) add a protocol dimension to Happy Eyeballs. An HTTPS record contains three things relevant to connection establishment:

- **`alpn`** — which protocols the server supports (`["h3" "h2"]`); lets the client try QUIC before TCP connection is made
- **`ipv4hint`/`ipv6hint`** — pre-resolved IP addresses from the authoritative DNS server; lets the client skip the A/AAAA round-trip entirely
- **`ech`** — Encrypted Client Hello parameters; hides the SNI from network observers

HTTPS records have two forms. **ServiceMode** (priority > 0) carries the parameters above. **AliasMode** (priority = 0) redirects to another hostname — CDNs use this to point customer domains at their own SVCB infrastructure. The client follows the alias chain before it can read any parameters.

```tinct
# protocols/http.llt

SvcbRecord: [union
  [AliasMode  target@String]            # priority 0: redirect to target
  [ServiceMode                          # priority > 0: connection parameters
    alpn@[Seq String]
    ipv4@[Seq [Bytes 4]]
    ipv6@[Seq [Bytes 16]]
    ech@Bytes
    port@[or Port Null]]]

# Follow the SVCB alias chain. AliasMode records (priority=0) redirect
# to another hostname. Depth limit avoids loops.
resolve-svcb: [fn [let cap@NetCap host@String depth@Int]
  [if [> depth 3]
    null
    [match [try [await [task [dns-resolve cap host HTTPS]]]]
      [Ok [let rec ...rest]]:
        [match rec
          [let [AliasMode target]]: [resolve-svcb cap target [+ depth 1]]
          [let sm: ServiceMode]:    sm]
      [Err _]: null]]]

# SVCB-aware HTTP connection — the implementation behind fetch.
# Races HTTPS record lookup against A/AAAA, then races h3/QUIC against h2/TCP.
http-connect: [fn [let cap@NetCap host@String port@Int]
  [svcb-task: [task [resolve-svcb cap host 0]]
   v6-task:   [task [dns-resolve cap host AAAA]]
   v4-task:   [task [dns-resolve cap host A]]]

  # Use same 50ms window as AAAA preference (RFC 8305 §5) — if SVCB arrives first, use it
  [svcb: [match [timeout [millis 50] svcb-task]
    [Ok rec]: rec
    [Err _]:  null]]

  [match svcb
    [let sm: ServiceMode]
      # IP hints skip A/AAAA round-trips; fall back to DNS if hints absent
      [v6-addrs: [if [empty? sm.ipv6]
        [match [try [await v6-task]] [Ok a]: a [Err _]: []]
        [map Ipv6 sm.ipv6]]]
      [v4-addrs: [if [empty? sm.ipv4]
        [await v4-task]
        [map Ipv4 sm.ipv4]]]
      [http-protocol-race cap host [or sm.port port]
                          sm.alpn [interleave v6-addrs v4-addrs] sm.ech]

    null
      # No SVCB — plain Happy Eyeballs, negotiate h2/h1 via ALPN in TLS
      [addrs: [interleave
        [match [try [await v6-task]] [Ok a]: a [Err _]: []]
        [await v4-task]]]
      [raw: [happy-connect cap port addrs]]
      [tls-layer raw [sni: host  alpn: ["h2" "http/1.1"]]]]

# Race h3/QUIC and h2/TCP with 250ms stagger in ALPN preference order.
# QUIC gets a 250ms head start over TCP — enough to detect QUIC-blocking
# firewalls without adding perceptible latency when QUIC works.
http-protocol-race: [fn [let cap@NetCap host@String port@Int
                          alpn@[Seq String] addrs@[Seq IpAddress] ech@Bytes]
  [result-ch:  [channel 1]
   h3?:        [not [empty? [filter [fn [let p] [= p "h3"]] alpn]]]
   h2?:        [not [empty? [filter [fn [let p] [= p "h2"]] alpn]]]]

  [h3-tasks: [if h3?
    [collect [map-indexed [fn [let i addr]
      [task
        [recv [timer-channel %clock [millis (* i 250)]]]
        [match [try [quic-h3-connect cap host addr port ech]]
          [Ok h]:  [try [send result-ch h]]
          [Err _]: null]]]
      addrs]]
    []]]

  [h2-tasks: [if h2?
    [collect [map-indexed [fn [let i addr]
      [task
        [recv [timer-channel %clock [millis (+ [if h3? 250 0] (* i 250))]]]
        [match [try [tcp-h2-connect cap host addr port ech]]
          [Ok h]:  [try [send result-ch h]]
          [Err _]: null]]]
      addrs]]
    []]]

  [result: [recv result-ch]]
  [par-map [fn [let t] [cancel-task t]] [append h3-tasks h2-tasks]]
  result]

quic-h3-connect: [fn [let cap@NetCap sni@String addr@IpAddress port@Int ech@Bytes]
  [quic-connect-addr cap [addr: addr  port: port] [sni: sni  ech: ech]]]

tcp-h2-connect: [fn [let cap@NetCap sni@String addr@IpAddress port@Int ech@Bytes]
  [tls-layer [tcp-connect cap [addr: addr  port: port]]
             [sni: sni  alpn: ["h2"]  ech: ech]]]
```

**What this demonstrates:** recursive SVCB alias-chain following with depth limiting; parallel HTTPS + A/AAAA DNS lookups with 50ms window; IP hint extraction eliminating A/AAAA round-trips; simultaneous h3/QUIC and h2/TCP racing with 250ms stagger; ECH parameter threading end-to-end; first-success-wins with `par-map` cancellation — all in tinct, using only OS-level primitives and runtime-v2 async machinery.

**SNI note:** `resolve-svcb` may follow one or more `AliasMode` redirections before returning a `ServiceMode` record. Throughout this chain, `http-connect` uses the original `host` argument as the TLS SNI value — never any alias target. This is correct per RFC 9460 §7.2: SNI must identify the service, not the CDN infrastructure that happens to serve it.

**`channel-map` and non-ByteStream connections:** `channel-map` is fully polymorphic — it places no `ByteStream` constraint on the function `f` it applies. A function `my-proto-accept: [Fn [WebSocketConnection] MyConnection]` (or a closure closing over config) composes with `channel-map` just as `tls-accept` does. The serve layer model works for any connection type, not only `ByteStream` instances.

---

## New Rust Primitives

The primitives follow two rules: (1) the transport boundary maps 1:1 to cap-std's networking API — no higher-level protocol logic belongs in Rust; (2) all crypto operations are Rust for security correctness, not performance, because tinct's lazy evaluator cannot guarantee constant-time execution paths and variable-time crypto leaks secret key material via timing side-channels.

### Transport Primitives

These correspond directly to cap-std's `TcpListener`, `UdpSocket`, and `TcpStream` types. Everything above the OS socket layer — accept loops, connection multiplexing, protocol handshakes — is tinct.

| Primitive | Signature | Description | Why Rust |
|-----------|-----------|-------------|----------|
| `tcp-bind` | `[Fn [cap@NetCap target@BindTarget] TcpListener]` | Bind and listen on a TCP socket | **IpBind(SocketAddress)**: if address is `Ipv4` or `Ipv6` (no zone), `pool.bind_tcp_listener(addr)` — atomic check+bind. If address is `Ipv6Zone(bytes, zone)`, the zone name is looked up in `interface_entries` (Pool compares only IP bytes; zone ID must be validated against the cap separately); `if_nametoindex(zone)` converts interface name → kernel scope_id; `sockaddr_in6.sin6_scope_id` is set before bind. **InterfaceBind(name, port)**: `SO_BINDTODEVICE(name)` + scope prefix-check on the actual bind address; validates against `interface_entries` |
| `tcp-accept` | `[Fn [h@TcpListener] Handle]` | Accept one incoming TCP connection (async) | OS accept() syscall on a `TcpListener` handle + tokio reactor registration; tinct cannot make socket syscalls |
| `tcp-connect` | `[Fn [cap@NetCap addr@SocketAddress] Handle]` | Connect to a TCP endpoint | `pool.connect_tcp_stream(addr)` — capability check and socket syscall are one indivisible operation; no TOCTOU window |
| `udp-socket` | `[Fn [cap@NetCap target@BindTarget] UdpSocket]` | Bind a UDP socket; use `port = 0` for ephemeral | Same routing as `tcp-bind`: `pool.bind_udp_socket(addr)` for `IpBind` with non-zoned address; zone-aware path through `interface_entries` + `if_nametoindex()` for `Ipv6Zone`; `SO_BINDTODEVICE` + scope enforcement for `InterfaceBind` |
| `udp-recv` | `[Fn [sock@UdpSocket] UdpDatagram]` | Receive one UDP datagram with source address (async) | Reads from `UdpSocket` opaque Rust state; tinct cannot access UdpSocket internals |
| `udp-send` | `[Fn [sock@UdpSocket addr@SocketAddress data@Bytes] Null]` | Send a datagram to a specific address | Writes through `UdpSocket` opaque Rust state |
| `unix-listen` | `[Fn [cap@DirCap path@String] [Channel Handle]]` | Incoming Unix socket connections | cap-std's `UnixListener` is not yet implemented upstream; wraps the accept loop internally using `openat2(RESOLVE_BENEATH)` + raw `UnixListener`. When cap-std adds `UnixListener`, two new Rust primitives appear: `unix-bind: [Fn [DirCap String] UnixListener]` and `unix-accept: [Fn [UnixListener] Handle]`; one `[instance [Listener UnixListener] accept: unix-accept]` declaration is added; `unix-listen` becomes pure tinct using `listen-loop` |
| `read-bytes` | `[Fn [h@Handle n@Int] Bytes]` | Read exactly n bytes from a Handle (async, suspends until available) | Handle is opaque Rust state backed by tokio `AsyncRead`; tinct cannot access Handle internals or call tokio I/O directly |
| `write-bytes` | `[Fn [h@Handle b@Bytes] Null]` | Write bytes to a Handle (async) | Handle is opaque Rust state backed by tokio `AsyncWrite` |
| `try-recv` | `[Fn [ch@[Channel t]] [Result t]]` | Non-blocking recv — `Err` if no item immediately available | Requires reading Channel's internal buffer occupancy without consuming an item; `select-once` + 0ms-timer is scheduler-dependent and not guaranteed non-blocking |
| `channel-count` | `[Fn [ch@[Channel t]] Int]` | Current number of items in the channel buffer | Reads `Arc<ChannelInner>` buffer count without consuming; tinct cannot access Channel internals; used by `collect-channel` to snapshot size before draining |

`tcp-listen`, `udp-bind`, `quic-listen`, and all higher-level connection factories are tinct. The `Listener` typeclass (same pattern as `ByteStream`, `Datagram`, `MessageStream`) provides `accept` as a polymorphic dispatch method.

**Scope of `NetCap`:** everything here is IP-layer (layer 3 and above). Non-IP Ethernet protocols operate at layer 2 via `AF_PACKET` raw sockets with EtherType filtering and require a different capability type. The most immediately useful is **LLDP** (IEEE 802.1AB, EtherType `0x88CC`): tinct programs that discover network topology, map switch adjacency, or inspect LLDP neighbor advertisements are practical infrastructure tools. Following the same `name@flags=resource` pattern as `--cap-net` and `--cap-fs` (where `name` becomes `%name` in the program and `resource` identifies what is being accessed):

```sh
--cap-ethernet lldp@r=eth0:0x88CC    # %lldp: receive LLDP frames on eth0
--cap-ethernet arp@rw=eth0:0x0806    # %arp: send+receive ARP on eth0
--cap-ethernet net@r=eth0            # %net: receive all EtherTypes on eth0
```

The flag letters `@r` (receive) and `@w` (send/write) deliberately mirror DirCap's `@r` (read) and `@w` (write) — the analogy is: reading bytes from the wire ↔ reading bytes from a file; injecting frames ↔ writing bytes to a file. Both capabilities use the same semantic: `@r` = consume, `@w` = produce. This consistency is intentional, not a collision. A future EthernetCap spec should state this analogy explicitly to avoid confusion between "receive frame" and "read file".

Other EtherType candidates: ARP (`0x0806`), PTP (`0x88F7`), IS-IS (`0x8847`), 802.1X (`0x888E`). `NetCap` is IP-only by design, not by accident, and `EthernetCap` sits cleanly alongside it without retrofitting. `TcpListener` and `UnixListener` are opaque Rust types with `Listener` instances that delegate to `tcp-accept`/`unix-accept`. `listen-loop` is parametric over any `Listener l`. Adding a new listener type requires one Rust primitive pair and one `[instance [Listener ...] ...]` declaration — no changes to `accept`, `listen-loop`, or any call site. `read-bytes` and `write-bytes` are the `Handle` instance methods for `ByteStream`; `udp-recv` and `udp-send` are the `UdpSocket` instance methods for `Datagram`. All are called through typeclass dispatch in tinct code.

### Crypto Primitives

All operate on `Bytes` and `[Bytes N]`. All are Rust for one reason: timing-sensitive arithmetic cannot be done correctly in tinct regardless of whether `BigInt` exists, because tinct's lazy evaluator cannot guarantee constant-time execution paths. This is a correctness constraint, not a performance one. `BigInt` is already in tinct for general-purpose use; it has no role here — every operation that needs large-number arithmetic on secret material is a Rust primitive.

| Primitive | Signature | Description | Why Rust (security) |
|-----------|-----------|-------------|---------------------|
| `chacha20-poly1305-seal` | `[Fn [key@[Bytes 32]  nonce@[Bytes 12]  plaintext@Bytes  associated-data@Bytes] Bytes]` | ChaCha20-Poly1305 AEAD encrypt | Constant-time required |
| `chacha20-poly1305-open` | `[Fn [key@[Bytes 32]  nonce@[Bytes 12]  ciphertext@Bytes  associated-data@Bytes] [Result Bytes]]` | ChaCha20-Poly1305 AEAD decrypt | Constant-time; tag comparison must not branch on secret |
| `aes-128-gcm-seal` | `[Fn [key@[Bytes 16]  nonce@[Bytes 12]  plaintext@Bytes  associated-data@Bytes] Bytes]` | AES-128-GCM AEAD encrypt | Constant-time; hardware AES-NI |
| `aes-128-gcm-open` | `[Fn [key@[Bytes 16]  nonce@[Bytes 12]  ciphertext@Bytes  associated-data@Bytes] [Result Bytes]]` | AES-128-GCM AEAD decrypt | Constant-time; hardware AES-NI |
| `aes-256-gcm-seal` | `[Fn [key@[Bytes 32]  nonce@[Bytes 12]  plaintext@Bytes  associated-data@Bytes] Bytes]` | AES-256-GCM AEAD encrypt | Same |
| `aes-256-gcm-open` | `[Fn [key@[Bytes 32]  nonce@[Bytes 12]  ciphertext@Bytes  associated-data@Bytes] [Result Bytes]]` | AES-256-GCM AEAD decrypt | Same |
| `x25519-keypair` | `[Fn [] [private: [Bytes 32]  public: [Bytes 32]]]` | Generate X25519 key pair | CSPRNG + constant-time scalar multiplication |
| `x25519-dh` | `[Fn [private@[Bytes 32]  peer-public@[Bytes 32]] [Bytes 32]]` | X25519 Diffie-Hellman | Constant-time scalar multiplication over Curve25519 |
| `ed25519-keypair` | `[Fn [] [private: [Bytes 32]  public: [Bytes 32]]]` | Generate Ed25519 signing key pair | CSPRNG + constant-time |
| `ed25519-sign` | `[Fn [private@[Bytes 32]  msg@Bytes] [Bytes 64]]` | Ed25519 signature | Constant-time |
| `ed25519-verify` | `[Fn [public@[Bytes 32]  msg@Bytes  sig@[Bytes 64]] Bool]` | Ed25519 verification | Constant-time |
| `p256-keypair` | `[Fn [] [private: [Bytes 32]  public: [Bytes 65]]]` | Generate ECDSA P-256 key pair (public: uncompressed) | CSPRNG + constant-time |
| `p256-sign` | `[Fn [private@[Bytes 32]  msg@Bytes] [Bytes 64]]` | ECDSA P-256 signature (r\|\|s) | Constant-time |
| `p256-verify` | `[Fn [public@[Bytes 65]  msg@Bytes  sig@[Bytes 64]] Bool]` | ECDSA P-256 verification | Constant-time |
| `p384-sign` | `[Fn [private@[Bytes 48]  msg@Bytes] [Bytes 96]]` | ECDSA P-384 signature | Constant-time |
| `p384-verify` | `[Fn [public@[Bytes 97]  msg@Bytes  sig@[Bytes 96]] Bool]` | ECDSA P-384 verification | Constant-time |
| `rsa-pss-sign` | `[Fn [private@Bytes  msg@Bytes] Bytes]` | RSA-PSS signature | Constant-time private-key modular exponentiation; key and output size depend on key length |
| `rsa-pss-verify` | `[Fn [public@Bytes  msg@Bytes  sig@Bytes] Bool]` | RSA-PSS signature verification | Constant-time; required for X.509 cert validation in TLS |
| `rsa-pkcs1-verify` | `[Fn [public@Bytes  msg@Bytes  sig@Bytes] Bool]` | RSA PKCS#1 v1.5 verification | Constant-time; legacy TLS cert signatures |
| `sha1` | `[Fn [data@Bytes] [Bytes 20]]` | SHA-1 | Constant-time; WebSocket upgrade |
| `sha256` | `[Fn [data@Bytes] [Bytes 32]]` | SHA-256 | Constant-time |
| `sha384` | `[Fn [data@Bytes] [Bytes 48]]` | SHA-384 | Constant-time |
| `sha512` | `[Fn [data@Bytes] [Bytes 64]]` | SHA-512 | Constant-time |
| `blake2s` | `[Fn [data@Bytes] [Bytes 32]]` | BLAKE2s unkeyed | Constant-time; WireGuard hashing |
| `blake2s-mac` | `[Fn [key@[Bytes 32]  data@Bytes] [Bytes 32]]` | BLAKE2s keyed MAC | Constant-time; WireGuard in place of HMAC |
| `hmac-sha256` | `[Fn [key@Bytes  data@Bytes] [Bytes 32]]` | HMAC-SHA-256 | Constant-time |
| `hkdf-extract` | `[Fn [hash@HkdfHash  salt@Bytes  input-key-material@Bytes] Bytes]` | HKDF-Extract (hash: `[Sha256]` `[Sha384]` `[Sha512]` `[Blake2s]`) | Constant-time; output size = hash output size (32 for Sha256/Blake2s, 48 for Sha384, 64 for Sha512) |
| `hkdf-expand` | `[Fn [hash@HkdfHash  pseudorandom-key@Bytes  info@Bytes  len@Int] Bytes]` | HKDF-Expand | Constant-time; output length is runtime `len` — returns `Bytes`, annotate `@[Bytes N]` at call site |
| `crypto-random` | `[Fn [len@Int] Bytes]` | Cryptographically secure random bytes | OS entropy source; length is runtime — returns `Bytes`, annotate `@[Bytes N]` at call site |

---

## Stdlib Module Map

Each module is fully specified as a draft `.llt` file in [`doc/whatif/lib-net-v3/`](lib-net-v3/), following the two-dict stdlib convention (internal helpers first, exported API last). The four minor protocol files (icmp, socks5, grpc, mqtt) are stub entries pending their own `.llt` files.

| Draft file | Target stdlib path | Key exports |
|---|---|---|
| [`async.llt`](lib-net-v3/async.llt) | `stdlib/async.llt` | `channel-map`, `channel-flat-map`, `collect-channel` |
| [`net.llt`](lib-net-v3/net.llt) | `stdlib/net.llt` | IO typeclasses (`ByteStream`, `Datagram`, `MessageStream`, `Listener`, `Indexed`); `IpAddress`, `Port`, `SocketAddress`, `UdpDatagram`; `BindTarget`, `BindScope`; `ByteLabel`/`ByteOrder`; `tcp-listen`, `udp-bind`, `udp-ephemeral`, `listen-loop`, `connect-host` (RFC 8305), `ip->string`, `Url`, `parse-url`, `url-decode` |
| [`dns.llt`](lib-net-v3/dns.llt) | `stdlib/dns.llt` | `DnsQuery`, `DnsRecord`, `DnsResponse`, `Nameserver`, `DnsConfig`; `encode-dns-wire`, `decode-dns-wire`; resolver factories (`dns-udp-resolver`, `dns-tls-resolver`, `dns-https-resolver`, `dns-quic-resolver`); `dns-framed-send`; `resolve-host`, `dns-resolve`, `dns-server-loop` |
| [`tls.llt`](lib-net-v3/tls.llt) | `stdlib/protocols/tls.llt` | `HkdfHash`, `TlsServerConfig`, `TlsClientConfig`, `CipherSuite`, `TlsConnection`; `hkdf-expand-label`, `derive-secret`, `tls13-key-schedule`; `tls-accept`, `tls-layer`, `tls-serve` |
| [`quic.llt`](lib-net-v3/quic.llt) | `stdlib/protocols/quic.llt` | `QuicFrame` (all RFC 9000 types), `QuicConnection`, `QuicKeys`, `QuicLossState`; `quic-listen`, `quic-connect`, `quic-open-stream` |
| [`h2.llt`](lib-net-v3/h2.llt) | `stdlib/protocols/h2.llt` | `H2Frame`, `HpackTable`, `Http2Connection`; `hpack-static-table` (61 entries); `hpack-decode`, `hpack-encode`, `h2-accept`, `h2-connect`, `http-request`, `http2-client` |
| [`h3.llt`](lib-net-v3/h3.llt) | `stdlib/protocols/h3.llt` | `H3Frame`, `Http3Connection`; `read-varint`/`encode-varint`; `h3-accept`, `h3-connect`, `http3-client`, `h3-http-request`, `http3-req-conn`, `qpack-decode`, `qpack-encode` |
| [`http1.llt`](lib-net-v3/http1.llt) | `stdlib/protocols/http1.llt` | `RawRequest`, `RawResponse`; `ok`, `json-ok`, `redirect`, `not-found`, `server-error`; `parse-request`, `write-response`, `http1-conn`, `http1-request` |
| [`http.llt`](lib-net-v3/http.llt) | `stdlib/protocols/http.llt` | `SvcbRecord`; `http-channel`, `http-channel-tls`, `resolve-svcb`, `http-connect`, `fetch`; `router`, `headers-map`; `with-compression`, `with-logging`, `with-timeout`, `with-cors`, `with-auth` |
| [`websocket.llt`](lib-net-v3/websocket.llt) | `stdlib/protocols/websocket.llt` | `WsFrame`, `WebSocketConnection`; `ws-accept`, `ws-connect`, `ws-recv-frame`, `ws-send-frame` |
| [`wireguard.llt`](lib-net-v3/wireguard.llt) | `stdlib/protocols/wireguard.llt` | `WireguardConfig`, `WireguardConnection`; `wireguard-serve`, `wireguard-accept`, `wireguard-layer`, `wg-read-bytes`, `wg-write-bytes` |
| [`noise.llt`](lib-net-v3/noise.llt) | `stdlib/protocols/noise.llt` | `NoisePattern`, `NoiseConfig`, `NoiseState`, `NoiseConnection`; `noise-serve`, `noise-accept`, `noise-layer`, `noise-read`, `noise-write`; patterns XX, IK, NK |
| [`crypto.llt`](lib-net-v3/crypto.llt) | `stdlib/crypto.llt` | `AsnValue` union; `parse-cert`, `cert-san`, `cert-public-key`, `verify-cert-chain` |
| [`text.llt`](lib-net-v3/text.llt) | `stdlib/text.llt` | `TextCodec` typeclass + instances (UTF-8, UTF-16, Windows-1252, ~38 WHATWG encodings); `Codec`, `TextBytes`; `to-codec`, `codec-for-name`, `text-encode`, `text-decode`, `decode-http-body` |
| [`compress.llt`](lib-net-v3/compress.llt) | `stdlib/compress.llt` | `CompressionCodec` typeclass (with `codec-name`) + instances (Gzip, Deflate, Brotli, Zstd, Identity); `CompressedCodec` runtime record; `to-compressed-codec`; `encode`, `decode`, `encode-compressed`, `decode-compressed`; `codec-for-encoding`, `negotiate-encoding` (both accept optional `registry@[Map String CompressedCodec]` for user codecs) |
| [`serve.llt`](lib-net-v3/serve.llt) | `stdlib/serve.llt` | `h2-serve`, `h3-serve`, `ws-serve`; `http1-serve`, `http2-requests`, `http3-requests`; `quic-stream-ch` |
| *(not yet written)* | `stdlib/protocols/icmp.llt` | `EchoRequest`, `EchoReply`; `read-icmp`, `send-icmp` |
| *(not yet written)* | `stdlib/protocols/socks5.llt` | SOCKS5 proxy protocol |
| *(not yet written)* | `stdlib/protocols/grpc.llt` | gRPC framing over `h2.llt` |
| *(not yet written)* | `stdlib/protocols/mqtt.llt` | MQTT frame parsing |

**HTTP/1.1 request lifecycle — where the Rust/tinct boundary sits:**

```text
OS TCP accept → Rust: tcp-bind/tcp-accept → Handle
              → tinct: stdlib/net.llt tcp-listen accept loop
              → tinct: stdlib/protocols/http1.llt parse-request (text parsing via read-bytes)
              → tinct: stdlib/serve.llt http1-serve attaches respond fn
              → tinct: user handler (router, headers-map, json-ok — all in stdlib/protocols/http.llt)
              → tinct: stdlib/protocols/http1.llt write-response serializes to wire format
              → Rust: write-bytes sends bytes through Handle
OS TCP send
```

---

## Fixed-Size Bytes: `[Bytes N]`

This whatif introduces `[Bytes N]` as a new type form — a fixed-size byte sequence where `N` is a natural number literal in the type annotation. The need appears throughout this spec: `[Bytes 4]` for IPv4 addresses, `[Bytes 16]` for IPv6 addresses and AES-128 keys, `[Bytes 32]` for X25519/Ed25519 keys and SHA-256 output, `[Bytes 12]` for ChaCha20-Poly1305 nonces. Without it, all crypto primitive size contracts are documentation-only and every wrong-sized key is a runtime error rather than a type error.

### Conceptual foundation — the closed-Map interpretation

A fixed-size byte string of length N is isomorphic to a closed Map from integer keys `{0, 1, …, N-1}` to `UInt8` values, or equivalently a Record with integer literal field names. This is the same structure as a C array: `uint8_t ip[4]` is a record `{ .0=uint8_t .1=uint8_t .2=uint8_t .3=uint8_t }`. The integer-key Record view justifies why `[Bytes N]` is not a wholly new concept: it is what you already get when you write a Record with `N` consecutive integer field names. `[Bytes N]` is notation for this concept with compact contiguous storage rather than a hash map of keys.

The subtyping relationship is that of refinement types: `[Bytes N]` is `Bytes` refined by the constraint `length = N`. A more constrained type is a subtype of its base type — the same relationship as `UInt8 <: Int`. So `[Bytes N] <: Bytes` — a fixed-size byte sequence is a valid `Bytes` value and can be used anywhere variable-length `Bytes` are accepted.

### Type system change

Add `Type::SizedBytes(usize)` alongside the existing `Type::Bytes`:

- `[Bytes 4]` in an annotation resolves to `Type::SizedBytes(4)` — the `4` is a `Kind::Nat` argument
- `Type::SizedBytes(N) <: Type::Bytes` — fixed-size is a subtype of variable-size
- `Type::SizedBytes(M) ≠ Type::SizedBytes(N)` when `M ≠ N` — sizes are statically distinct
- A `Bytes` value narrows to `[Bytes N]` at a `TypeAssert` boundary when the `is:` predicate validates the length — the same runtime validation mechanism used by `UInt8`, `Port`, etc.

`Kind::Nat` is a bounded extension to the kind system: integer literals used as type arguments. Only `Bytes` takes a `Kind::Nat` argument initially. Type-level arithmetic over `Nat` is supported through the existing type-stage evaluator: `[+ m n]` in a return type position evaluates at type-check time when `m` and `n` are known `Kind::Nat` values. When either is unresolvable (because the caller passed a variable-length `Bytes`), the return type degrades to `Bytes` via the `[Bytes N] <: Bytes` subtyping. No new type-stage machinery is needed beyond `Kind::Nat` itself — `+` over `Nat` is just type-stage arithmetic. The `[instance [MessageStream [Channel t] t] ...]` declaration requires HKT instance head matching (matching on `App(Operator("Channel"), TypeVar("t"))` during instance resolution); this must be verified as supported or added alongside `Kind::Nat`.

### How it cleans up this whatif's signatures

The Crypto Primitives table above uses `[Bytes N]` throughout — wrong key sizes are type errors rather than runtime panics. `crypto-random` and `hkdf-expand` take a runtime `len` argument so their output is `Bytes`; callers annotate `@[Bytes N]` at the use site when the size is statically known.

### `UInt8`, `IpAddress`, `SocketAddress`, and `UdpDatagram`

`UInt8` and width-typed integers are already in stdlib from the `numeric-types` sprint:

```tinct
# stdlib/numeric.llt (already implemented)
UInt8:  [type Int@[is: [between 0 255]    repr: u8]]
UInt16: [type Int@[is: [between 0 65535]  repr: u16]]
UInt32: [type Int@[is: [between 0 4294967295]  repr: u32]]
```

`[Bytes N]` builds on `UInt8` conceptually (each element is a `UInt8`). `IpAddress`, `SocketAddress`, and `UdpDatagram` are defined in `stdlib/net.llt`:

```tinct
# stdlib/net.llt

# Fixed-size addresses using [Bytes N]
IpAddress: [union
  [Ipv4     [Bytes 4]]               # any IPv4 address (incl. 169.254.x.x link-local,
                                     # 0.0.0.0 all-interfaces, 127.x.x.x loopback)
  [Ipv6     [Bytes 16]]              # any IPv6 address without zone ID (incl. ::1 loopback,
                                     # :: all-interfaces, 2001:db8::, fc00::/7 ULA,
                                     # ::ffff:x.x.x.x IPv4-mapped; fec0::/10 deprecated)
  [Ipv6Zone [Bytes 16] zone@String]] # IPv6 with zone ID (RFC 4007): required for any scoped
                                     # address on multi-interface nodes — link-local unicast
                                     # fe80::/10 AND link-local multicast ff02::/16 (mDNS etc.)
                                     # zone = interface name; maps to sin6_scope_id at bind time
                                     # IPv4 169.254.x.x does NOT need a zone variant — each
                                     # IPv4 link-local address is unique per link by protocol

Port:      [type Int@[is: [between 0 65535]  repr: u16]]
           # 0 = ephemeral (OS assigns); valid for udp-ephemeral and any ephemeral bind
SocketAddress: [type [addr: IpAddress  port: Port]]

# UDP datagram as received — respond sends back to the peer
UdpDatagram: [type
  [src:     SocketAddress
   data:    Bytes
   respond: [Fn [Bytes] Null]]]
```

`[decode BigEndian @[Bytes 4]: ip-bytes]` converts a `[Bytes 4]` to `[Result UInt32]` for IPv4 CIDR masking via the `ByteLabel ByteOrder UInt32 [Bytes 4]` instance. The byte order is always explicit — no implicit native-endian conversion. IPv4 and all standard internet protocols use `BigEndian` (network byte order).

### Construction and operations for `[Bytes N]`

`[Bytes N]` values are constructed the same way as any other annotated value — the TypeAssert boundary validates the size using the existing `is:` predicate mechanism, exactly as `UInt8` and `Port` work:

```tinct
key@[Bytes 32]:  [192 168 1 1 ...]    # annotation validates: length = 32
addr@[Bytes 4]:  [192 168 1 1]        # [Seq UInt8] narrows to [Bytes 4] at TypeAssert
nonce@[Bytes 12]: crypto-random-bytes # result annotated at call site
```

No separate `bytes` constructor function — uppercase `[Bytes N]` is a type annotation, and seq literals with `@[Bytes N]` annotations handle construction uniformly.

`get` and `slice` are generic indexed-access operations, shared with `String` and `[Seq T]` through the `Indexed` typeclass:

```tinct
[class [Indexed s e]
  get:    [Fn [s@s i@Int] e]
  slice:  [Fn [s@s start@Int len@Int] s]
  length: [Fn [s@s] Int]]

[instance [Indexed [Bytes N] UInt8] ...]   # element type is statically UInt8; index bounds
                                            # are checked at runtime (not type level — that
                                            # would require dependent types)
[instance [Indexed String Char] ...]
[instance [Indexed [Seq T] T] ...]
```

Integer interpretation of byte sequences uses `encode`/`decode` via the `ByteLabel ByteOrder` instances. The byte order is always explicit — there is no implicit native-endian conversion, which would silently produce wrong results when tinct runs on a CPU with different endianness than the protocol expects:

```tinct
[decode BigEndian @[Bytes 4]: ip-bytes]    # → [Result UInt32] — IPv4 for CIDR masking
[decode BigEndian snmp-length]             # → [Result UInt16] — SNMP PDU length field
[decode LittleEndian ble-bytes]            # → [Result UInt32] — Bluetooth LE 32-bit value
[encode BigEndian 443@UInt16]              # → [Bytes 2] — port in network byte order
```

`concat` is variadic and works for all `Appendable` types — `String`, `Bytes`, `[Bytes N]`, and `[Seq T]`. No type-encoded name needed:

```tinct
concat: [fn@[bind: [t]  return: t  constraint: [t: Appendable]] [...args@t]
  [reduce append empty args]]
```

`[concat a b]` where both `a: [Bytes 32]` and `b: [Bytes 32]` gives `[Bytes 64]` — the type-stage evaluator propagates `Kind::Nat` arithmetic through the `append` reduction. `[concat a b c]` gives `[Bytes [+ [+ m n] k]]`. When any argument is variable-length `Bytes`, the result is `Bytes`. The same `concat` works for strings: `[concat "hello" " " "world"]` → `String`.

### What changes in the implementation

- **Kind system**: add `Kind::Nat` — a new kind for integer literal type arguments. `Kind::Nat` does not exist today (`src/type_def.rs` has only `Kind::Type`, `Kind::Operator`, `Kind::Label`). This is a prerequisite for `[Bytes N]` and must be added in the same sprint. Type-stage arithmetic (`+`, `-`, `*` over `Kind::Nat`) plugs into the existing `NormCtxt::normalize()` infrastructure from the CHR sprint (`src/type_normalize.rs`): concrete `NatLit` values reduce immediately; unresolved `NatVar` variables produce a stuck `TypeStageApp` that is handled by `process_deferred_equalities`. When stuck, the return type of `[Bytes [+ m n]]` degrades to `Bytes` via the `SizedBytes(N) <: Bytes` subtyping rule — the same fallback already in place for other stuck `TypeStageApp` results. A new `NatVar` type variant (distinct from `TypeVar`) is needed to track kind correctly.
- **Parser**: `[Bytes N]` in annotation position — parse `N` as a `NatLit` type argument of kind `Kind::Nat`
- **Type checker**: `Type::SizedBytes(usize)` variant; subtype rule `SizedBytes(N) <: Bytes` (refinement); `Kind::Nat` for the `N` position in `Bytes` type application; well-formedness check that `N` is a positive integer literal
- **Unification**: `SizedBytes(M)` unifies with `SizedBytes(N)` only if `M = N`; `SizedBytes(N)` unifies with `Bytes` (via subtyping)
- **Instance resolution**: verify or add support for HKT instance heads — `[instance [MessageStream [Channel t] t] ...]` requires matching `App(Operator("Channel"), TypeVar("t"))` during instance lookup. If not yet supported, add alongside `Kind::Nat`.
- **Eval/materialize**: no change — runtime representation is `Value::Bytes`; size is checked at `TypeAssert` boundaries via `is:` predicate on `bytes-length`
- **Builtins**: update crypto primitive return type registrations; add `bytes-to-int`; add `Indexed` typeclass with `Handle`/`String`/`[Bytes N]`/`[Seq T]` instances (making `get`, `slice`, `length` work uniformly); `concat` already exists via `Appendable` — extend `Appendable` instance for `Bytes` if not present

---

## What Would Change

### Add Rust primitives

All primitives in the New Rust Primitives table above, registered in `standard_builtins()`.

### Add stdlib modules

`stdlib/net.llt`, `stdlib/text.llt`, `stdlib/compress.llt`, `stdlib/crypto.llt`, `stdlib/serve.llt`, `stdlib/dns.llt`, extend `stdlib/async.llt`; `stdlib/protocols/` (tls, quic, h2, h3, http1, http, compress, wireguard, noise, websocket, icmp, socks5, grpc, mqtt).

### Unified capability flag syntax — `name@flags=resource` for both DirCap and NetCap

Both `--cap-fs` (DirCap) and `--cap-net` (NetCap) use the same `name@flags=resource` syntax. The `@` mirrors tinct's type annotation syntax and is unambiguous: everything before `@` is the cap name (which becomes `%name` in the program), the letters between `@` and `=` are the flags, and everything after `=` is the resource. This replaces the existing DirCap `:flags` suffix (which is not extended to NetCap because colons already appear in network addresses).

**NetCap flags** (same `@` syntax as tinct type annotations):

| Flag | Name | Gates |
|---|---|---|
| `b` | Bindable | `tcp-bind`, `udp-socket` (listen/receive on this address) |
| `c` | Connectable | `tcp-connect`, `udp-send` (connect/send to this address) |
| (no `@`) | both | default when flags omitted — equivalent to `@bc` |

DNS resolution is **implied** by any named hostname entry — if you grant `api@c=api.example.com:443`, you implicitly can resolve `api.example.com` to reach it. A separate `Resolvable` flag is unnecessary: a hostname in the NetCap serves no purpose unless the program can resolve it.

**Two separate flags for two separate concerns:**

- `--cap-net name@flags=resource` — IP address, hostname, or CIDR range
- `--cap-net-interface name@flags=interface-name:port` — OS interface name (avoids ambiguity with hostnames; full interface name including `@` in veth names is safe after `=`)

**`--cap-net-interface` scope qualifiers** (appended to `b`):

| Flag | Meaning |
|---|---|
| `b` | Bindable, any scope |
| `bl` | Bindable, link-local only (`169.254.0.0/16`, `fe80::/10`) |
| `bg` | Bindable, globally-routable only |
| `bk` | Bindable, loopback only (`127.x.x.x`, `::1`) |

**Parsing rule:** find `=`, split left on `@` for name and flags; resource is everything after `=`.

**Resource parsing rules** (for `--cap-net`):

The resource portion follows RFC 3986 §3.2.2 `host ":" port` grammar — the same grammar used for URI authority components. Accept any valid form; normalize IPv6 to RFC 5952 canonical (lowercase, maximum `::` compression) for storage.

```text
host = IP-literal / IPv4address / reg-name   (RFC 3986)

IP-literal  = "[" IPv6address [ "%" ZoneID ] "]"   # zone ID: bare % in CLI, %25 in URIs
IPv4address = dec-octet "." dec-octet "." dec-octet "." dec-octet
reg-name    = hostname, e.g. localhost, api.example.com
```

| Pattern | Interpretation |
|---|---|
| starts with `[` | IP-literal (RFC 3986): extract content; `%` or `%25` suffix = zone ID (`Ipv6Zone`) |
| matches IPv4 dotted-decimal (with optional `/prefix`) | IPv4 literal or CIDR |
| everything else | `reg-name` — hostname; resolved at startup; IPs added to `listen_pool`/`connect_pool` |

Note: `+` is a valid `sub-delim` in RFC 3986 `reg-name` (hostnames) without percent-encoding — this further confirms `@flags=resource` with `+` inside resource is unambiguous.

**Only RFC 3986 `IPv4address` (strictly dotted-decimal) is accepted.** Decimal (`2130706433`), octal (`0177.0.0.1`), and hex (`0x7f000001`) encodings of IPv4 are rejected — they are not part of the RFC 3986 grammar and are a known SSRF bypass vector against allowlists that check only dotted-decimal. A non-dotted-decimal string that looks numeric is treated as a `reg-name` (hostname), DNS resolution is attempted, and if it fails the cap is rejected at startup with a clear error.

**Hostname-bind:** `--cap-net server@b=localhost:8080` resolves `localhost` at process start (via `/etc/hosts` then DNS) and adds all resolved IPs to `listen_pool`. If `localhost` resolves to `127.0.0.1` and `::1`, both are added. The program binds to whichever its OS socket prefers.

**Localhost non-loopback warning:** if any resolved IP for a `@b` hostname entry is outside `127.0.0.0/8` and is not `::1` (e.g., `localhost` resolves to a routable IP in a misconfigured container), tinct emits a startup warning: `"Warning: hostname 'localhost' resolved to <ip> which is not a loopback address; Bindable cap %name grants binding on this IP — the service may be reachable beyond loopback"`. This is a warning, not an error, because the resolution may be intentional (service mesh environments). The user can suppress it with `--cap-net server@b=127.0.0.1:8080` (explicit IP, no resolution).

**IPv6 with zone ID:** CLI accepts bare `%` (non-URI context) or `%25` (URI-encoded). Shell quoting required when `%` appears: `--cap-net "v6ll@b=[fe80::48ca:a4ff:fef8:9dbb%eno1.601]:443"`. Link-local bindings are more naturally expressed via `--cap-net-interface`.

```sh
# --- --cap-net examples (IP/hostname/CIDR) ---

tinct run --cap-net server@b=127.0.0.1:8080 server.llt
  # %server: Bindable on localhost:8080 only; cannot connect to anything

tinct run --cap-net listen@b=0.0.0.0:443 --cap-fs certs@r=./certs server.llt
  # %listen: Bindable all-interfaces:443; %certs: ./certs read-only

tinct run --cap-net api@c=api.example.com:443 client.llt
  # %api: Connectable to api.example.com (implies DNS resolution for that name)

tinct run --cap-net "v6ll@b=[::1]:8080" server.llt
  # IPv6 loopback; brackets required (RFC 3986 IP-literal)

tinct run --cap-net listen@b=127.0.0.1:8080 --cap-net upstream@c=10.0.0.0/8 proxy.llt
  # proxy: listen-only cap + connect-to-internal cap, fully separated

# --- --cap-net-interface examples (OS interface name + scope) ---

tinct run --cap-net-interface internal@b=eth0:8080 server.llt
  # %internal: Bindable on eth0, any scope, port 8080

tinct run --cap-net-interface mdns4@bl=eth0:5353 mdns.llt
  # %mdns4: Bindable on eth0, link-local IPv4 only (169.254.x.x)
  # cannot bind to 10.0.1.5:5353 or any global address on eth0

tinct run --cap-net-interface "mdns6@bl=eth0:5353" mdns.llt
  # %mdns6: Bindable on eth0, link-local IPv6 only (fe80::/10 AND ff02::/16 multicast)
  # cannot bind to global IPv6 on eth0 even if eth0 has a 2001:db8:: address

tinct run --cap-net-interface "veth@b=veth26@if2:8080" test.llt
  # interface name veth26@if2 contains @ — safe after = (RFC 3986 reg-name reserved)

tinct run --cap-net-interface "lo@bk=lo:8080" server.llt
  # %lo: Bindable on loopback interface, loopback scope only

# Unbound-style DNS: UDP/TCP on all interfaces, link-local capable, separate caps
tinct run --cap-net-interface dns-ll@bl=eth0:53 \
          --cap-net dns-any@b=0.0.0.0:53 \
          --cap-net dns-ctl@b=127.0.0.1:8953 unbound.llt
```

**DirCap flags** — the old `:flags` suffix is replaced by the unified `@flags=` prefix. This is a **breaking change** to existing `--cap-fs` users; the old form is removed outright (no deprecation period — the syntax is pre-1.0 and the test corpus is the migration guide):

```sh
--cap-fs data@rw=./data          # was: --cap-fs data=./data:rw
--cap-fs logs@a=./logs           # was: --cap-fs logs=./logs:a
--cap-fs config@r=./config       # was: --cap-fs config=./config:r
--cap-fs "code@r=./src/c++/lib"  # + in path: no problem, it's after =
```

With named caps, each network operation receives the minimal capability it needs:

```tinct
[
  listen-cap:   %listen    # Bindable on 127.0.0.1:8080 only
  upstream-cap: %upstream  # Connectable to 10.0.0.0/8 only
]
```

A compromised component holding `%upstream` can only connect to the internal range — it cannot bind on any port, cannot reach external hosts. The capability surface is explicit in both the command line and the code.

### Restructure `Value::NetCap` — action-split Pools

`Value::NetCap` currently holds `Rc<Vec<NetCapEntry>>` with a single allowlist checked by `check_net_cap_allowlist`. This design conflates listening and connecting, and does not use cap-std's networking capability primitives.

Replace with two cap-std Pools plus interface entries for name-based and scope-restricted bindings:

```rust
struct NetCapInner {
    // Hostname-level entries (globs, wildcards) checked before DNS resolution.
    // Resolution is always implied for any named entry.
    hostname_entries: Vec<NetCapEntry>,   // Hostname, HostPort, HostnameGlob, Any + action

    // Bindable IP addresses: tcp-bind, udp-socket go through listen_pool.
    listen_pool:  cap_std::net::Pool,

    // Connectable IP addresses: tcp-connect, udp-send go through connect_pool.
    connect_pool: cap_std::net::Pool,

    // Interface-name bindings: tcp-bind, udp-socket with InterfaceBind target.
    // Supports scope restriction (link-local only, global only, etc.).
    interface_entries: Vec<InterfaceEntry>,
}

struct InterfaceEntry {
    name:  String,        // "eth0", "lo", "veth26@if2" (full Linux interface name)
    port:  Option<u16>,   // None = any port
    flags: NetCapFlags,   // Bindable and/or Connectable
    scope: BindScope,     // AnyScope | LinkLocal | GlobalScope | Loopback
}
```

`--cap-net` parsing routes entries by flag: `@b` entries populate `listen_pool`; `@c` entries populate `connect_pool`; `@bc` or no-flag (default) populates both. Named hostname entries with `@c` also populate `hostname_entries` (for glob support) AND `connect_pool` (for IP-level enforcement after resolution).

**`Ipv6Zone` addresses do NOT go into the Pool.** When parsing `[fe80::1%eth0]:8080`, tinct recognises the zone ID, converts the entry into an `InterfaceEntry { name: "eth0", port: Some(8080), scope: LinkLocal, flags: Bindable }`, and stores it in `interface_entries` — not in `listen_pool`. The reason: `cap_std::net::Pool` compares only IP bytes; two `Ipv6Zone` values with the same bytes but different interface names (e.g., `fe80::1%eth0` and `fe80::1%wlan0`) are indistinguishable at the Pool level. Zone-aware capability checking requires an exact interface-name match, which only `interface_entries` provides. At bind time, `if_nametoindex(zone)` converts the interface name to the kernel scope_id and sets `sockaddr_in6.sin6_scope_id` — the Pool is not involved for zoned addresses.

**Flag parsing:** flags are parsed character-by-character from the `@flags` suffix; `@bc` means Bindable (`b`) + Connectable (`c`). The bracket form `@[Bindable Connectable]` accepts full names space-separated.

**Hostname+Bindable rejected at startup:** `--cap-net server@b=api.example.com:8080` is rejected at parse time with a clear error: `"@b (Bindable) entries must be IP literals, not hostnames — you cannot bind() on a remote address"`. Bindable requires a local address; a hostname that resolves to a remote IP would always fail `bind()` with `EADDRNOTAVAIL`.

`tcp-bind` calls `listen_pool.bind_tcp_listener(addr)` — atomic capability check + OS bind + listen. `tcp-connect` calls `connect_pool.connect_tcp_stream(addr)` — atomic capability check + OS connect. `udp-socket` calls `listen_pool.bind_udp_socket(addr)`. All are indivisible Pool operations with no TOCTOU window.

**Capability error messages:** when a Bindable-only cap is used where Connectable is needed, tinct produces a clear structured error — not an opaque pool failure:

- `tcp-connect %server some-addr` with `%server@b=...` → `"cap %server has no Connectable grant (@c) — cannot tcp-connect"`
- `resolve-host %server "api.example.com"` with `%server@b=...` → `"cap %server has no Connectable grant — resolve-host requires @c to send DNS queries"`

`resolve-host` checks upfront whether `connect_pool` is empty and errors immediately with a capability message, not a DNS failure.

`resolve-host` checks `hostname_entries` for the name, then validates the resolved IP against `connect_pool`'s CIDR entries as a defence-in-depth step before returning the `IpAddress`.

### `tcp-connect` takes an IP address, not a hostname

The Rust primitive `tcp-connect` takes a resolved `SocketAddress`. DNS resolution is a higher-layer tinct concern handled by `%dns` and `stdlib/net.llt`. TLS SNI is the hostname, not the IP — passed explicitly in `tls-config` as `sni: host`.

### Inject `%dns`

`%dns` is a `DnsConfig` record injected at process start. The `%` prefix means it is injected from the system environment (like `%libdir`), not that it is a security capability. The security boundary for DNS is `%net-cap` — it gates the UDP/TCP packets sent to the nameserver. `%dns` is resolver configuration: where to send queries, how to expand short names, how long to wait. A program that has `%net-cap` but no `%dns` can still do `tcp-connect` with a pre-resolved `SocketAddress`; it just cannot do hostname resolution.

Because tinct reads `/etc/resolv.conf` directly rather than delegating to glibc, it takes responsibility for implementing everything glibc would have handled automatically.

**Bootstrap:** `/etc/resolv.conf` `nameserver` lines contain only numeric IP addresses per the POSIX format specification — hostnames are not valid in `nameserver` directives. Tinct rejects non-numeric nameserver entries at startup with `"invalid nameserver: expected IP address, got '<value>'"`. This means there is no circular dependency: nameserver IPs are known before DNS is operational. If `/etc/resolv.conf` is absent or empty, `%dns.nameservers` is `[]` and resolution fails immediately with a clear message directing the user to `--nameservers`.

```tinct
# stdlib/net.llt

Nameserver: [union
  [UdpNameserver  addr@SocketAddress]                          # UDP/53 — standard; from /etc/resolv.conf
  [DotNameserver  addr@SocketAddress  sni@String]              # DNS-over-TLS (port 853)
  [DohNameserver  addr@SocketAddress  sni@String  path@String] # DNS-over-HTTPS
  [DoqNameserver  addr@SocketAddress  sni@String]]             # DNS-over-QUIC (port 853)

DnsConfig: [type
  [nameservers:    [Seq Nameserver]  # pre-resolved SocketAddresses — no circular DNS dependency
   search:         [Seq String]      # search domain list (search/domain keywords)
   ndots:          Int               # dot threshold for search-first vs absolute; default 1, max 15
   timeout:        Duration          # per-query timeout; default [seconds 5], max [seconds 30]
   attempts:       Int               # retries across all nameservers before giving up; default 2, max 5
   rotate:         Bool              # round-robin nameserver selection; default false
   no-aaaa:        Bool              # suppress AAAA queries (glibc 2.36+); default false
   edns0:          Bool              # EDNS0 extended query size (RFC 2671); default true
   use-vc:         Bool              # force TCP instead of UDP for all queries; default false
   no-check-names: Bool              # skip BIND hostname character validation (underscore etc); default false
   trust-ad:       Bool              # trust DNSSEC AD bit in responses (glibc 2.31+); default false
   single-request: Bool              # make A and AAAA queries sequential not concurrent; default false
                                     # (needed for appliances that mishandle parallel DNS queries)
   sortlist:       [Seq String]]]    # IP/mask pairs for sorting returned addresses; rarely used
```

All `Nameserver` variants take a pre-resolved `SocketAddress` — no DNS needed to reach the nameserver itself. `/etc/resolv.conf` has no protocol-selection syntax (only `options use-vc` for TCP/53, not TLS), so parsing always produces `UdpNameserver` entries. On modern Linux with systemd-resolved, `/etc/resolv.conf` typically lists `127.0.0.53` — the local stub. DoT/DoH configuration in `/etc/systemd/resolved.conf` affects the stub invisibly; tinct sees `UdpNameserver { addr: 127.0.0.53:53 }` and the stub handles upstream protocol. Users who want tinct itself to speak DoT/DoH to a remote resolver set `--nameservers` explicitly.

**CLI:**

```sh
--nameservers udp:1.1.1.1                               # UDP/53
--nameservers dot:1.1.1.1:853@one.one.one.one           # DoT with SNI
--nameservers doh:1.1.1.1:443@cloudflare-dns.com/dns-query  # DoH
--nameservers doq:1.1.1.1:853@one.one.one.one           # DoQ
--dns-search corp.example.com                           # append/override search domains
--dns-ndots 5                                           # override ndots (e.g. Kubernetes default)
--no-dns                                                # disable; %dns.nameservers = []
--no-dns-search                                         # disable search expansion; %dns.search = []
```

Multiple `--nameservers` and `--dns-search` flags are additive. `--no-dns` disables all DNS.

**`resolve-host` implements the full ndots + search domain algorithm:**

```tinct
# stdlib/net.llt

resolve-host: [fn [let cap@NetCap host@String]
  [config: %dns]
  [if [empty? config.nameservers]
    [error "DNS disabled — use --nameservers to configure or --no-dns to opt out explicitly"]
    [dns-try-names cap config [candidate-names host config.search config.ndots] A]]]

# Build candidate list per RFC 1535 / POSIX resolv.conf ndots semantics.
# If dots in host >= ndots: try absolute first, then each search domain.
# If dots in host <  ndots: try each search domain first, then absolute.
# A trailing dot in the original hostname means absolute only — no search expansion.
candidate-names: [fn [let host@String search@[Seq String] ndots@Int]
  [if [ends-with? host "."]
    [list host]   # already absolute — no expansion
    [dot-count: [length [filter [fn [let c] [= c "."]] [str-to-chars host]]]
     absolute:  [str host "."]
     searched:  [map [fn [let d] [str host "." d "."]] search]
     [if [>= dot-count ndots]
       [cons absolute searched]
       [append searched [list absolute]]]]]]

# Try each candidate name in order; first successful resolution wins.
dns-try-names: [fn [let cap@NetCap config@DnsConfig names@[Seq String] type@Symbol]
  [match names
    []: [error "DNS resolution failed — all candidates exhausted"]
    [let [name ...rest]]:
      [match [try [dns-query-with-retry cap config.nameservers name type
                                       config.attempts config.rotate]]
        [Ok addrs]: addrs
        [Err _]:    [dns-try-names cap config rest type]]]]

ns-to-resolver: [fn [let cap@NetCap ns@Nameserver]
  [match ns
    [let [UdpNameserver  addr]]:          [dns-udp-resolver  cap addr]
    [let [DotNameserver  addr sni]]:      [dns-tls-resolver  cap addr sni]
    [let [DohNameserver  addr sni path]]: [dns-https-resolver cap addr sni path]
    [let [DoqNameserver  addr sni]]:      [dns-quic-resolver  cap addr sni]]]
```

**Cargo:** `resolv-conf` crate parses the full `/etc/resolv.conf` — `nameserver`, `search`, `domain`, `options` (ndots, timeout, attempts, rotate, no-aaaa, edns0, use-vc), `sortlist`. All fields map directly to `DnsConfig` fields. Thin addition — no DNS implementation dependency since DNS runs in tinct.

### Change `Cargo.toml`

**Add** (RustCrypto — all pure Rust, no C dependencies):

- `chacha20poly1305` — ChaCha20-Poly1305 AEAD
- `aes-gcm` — AES-128-GCM and AES-256-GCM
- `x25519-dalek` — X25519 Diffie-Hellman (Curve25519)
- `ed25519-dalek` — Ed25519 signatures
- `p256`, `p384` — ECDSA/ECDH on NIST P-256 and P-384
- `sha2` — SHA-256/384/512
- `sha1` — SHA-1 (WebSocket upgrade only)
- `blake2` — BLAKE2s (WireGuard)
- `hmac`, `hkdf` — HMAC-SHA-256 and HKDF
- `rsa` — RSA-PSS and PKCS#1 v1.5 sign/verify (`rsa-pss-sign`, `rsa-pss-verify`, `rsa-pkcs1-verify`)
- `resolv-conf` — parse `/etc/resolv.conf` to populate default `%dns` at process start
- `encoding_rs` — WHATWG-compliant character encoding library (~38 encodings); backs `Encoding` opaque type and `encode`/`decode`/`decode-lossy` in `stdlib/text.llt`
- `flate2` — gzip and deflate compression (uses `miniz_oxide` pure-Rust backend); backs `Gzip` and `Deflate` instances in `stdlib/compress.llt`
- `brotli` — Brotli compression; backs `Brotli` instance
- `zstd` — Zstandard compression; backs `Zstd` instance

`num-bigint` is already in `Cargo.toml` from the `numeric-bigint` sprint and is not re-added here. `BigInt` is available in tinct for general use; it plays no role in this whatif because tinct-side arithmetic on secret key material cannot be constant-time regardless of `BigInt` availability.

**Remove**:

- `hyper` — HTTP/1.1 framing moves to `stdlib/protocols/http1.llt`
- `reqwest` — HTTP client moves to `stdlib/protocols/http.llt`
- `quinn` — QUIC moves to `stdlib/protocols/quic.llt` on top of cap-std UDP
- `h3` — HTTP/3 framing moves to `stdlib/protocols/h3.llt`
- `rustls` — TLS moves to `stdlib/protocols/tls.llt`

**Keep**: `tokio` (rt-multi-thread, net, time, signal, sync), `cap-std`, `notify`

(`tokio-util`, `num_cpus` are added by runtime-v2, not here.)

---

## Prerequisites

- [`runtime-v2.md`](runtime-v2.md) complete — `task`, `await`, `channel`, `recv`, `send`, `select-once`, `loop-select`, `context`, `with-timeout`, `finally`, `Arc`-based async runtime, and `stdlib/async.llt` all present. (`tcp-connect`, `quic-connect`, and `tls-layer` are tinct functions defined in this whatif's stdlib, not runtime-v2 primitives.)

---

## References

- Marlow, S. et al. (2009). "Runtime Support for Multicore Haskell." *ICFP '09*. — `par`/`seq` sparks; implicit parallelism underlying the serve/connect layer model.
- Syme, D., Petricek, T. & Lomov, D. (2011). "The F# Asynchronous Programming Model." *PADL '11*. — Async workflows as first-class values; request-client pattern.
- Go language specification. "Select statements." — `select` over channels; `select-once` is the primitive, `loop-select` the recurring wrapper.
- Leijen, D., Schulte, W. & Burckhardt, S. (2009). "The Design of a Task Parallel Library." *OOPSLA '09*. — `await-all`/`await-any` semantics.
- Donenfeld, J. A. (2017). "WireGuard: Next Generation Kernel Network Tunnel." *NDSS '17*. — Protocol design (Noise_IKpsk2, ChaCha20-Poly1305, BLAKE2s) for `stdlib/protocols/wireguard.llt`; tinct implements the user-mode protocol, not the kernel TUN variant.
- Perrin, T. (2018). "The Noise Protocol Framework." *noiseprotocol.org*. — `stdlib/protocols/noise.llt` generic pattern combinator; `wireguard.llt` uses Noise_IKpsk2.
- Langley, A., et al. (2017). "The QUIC Transport Protocol: Design and Internet-Scale Deployment." *SIGCOMM '17*. — QUIC design rationale; `stdlib/protocols/quic.llt`.
- Thomson, M. & Iyengar, J. (2021). RFC 9000 — QUIC: A UDP-Based Multiplexed and Secure Transport. — Wire format for `stdlib/protocols/quic.llt`.
- Bishop, M. (2022). RFC 9114 — HTTP/3. — Framing for `stdlib/protocols/h3.llt`.
- Belshe, M., Peon, R. & Thomson, M. (2015). RFC 7540 — HTTP/2. — Framing and HPACK for `stdlib/protocols/h2.llt`.
- Fette, I. & Melnikov, A. (2011). RFC 6455 — The WebSocket Protocol. — Frame format for `stdlib/protocols/websocket.llt`.
- Bernstein, D.J. (2006). "Curve25519: New Diffie-Hellman Speed Records." *PKC '06*. — X25519 key agreement underlying `x25519-dh` and all Noise/WireGuard/TLS handshakes.
- Rescorla, E. (2018). RFC 8446 — TLS 1.3. — Handshake and record layer for `stdlib/protocols/tls.llt`.
- Schwartz, B. & Bishop, M. (2022). RFC 9460 — Service Binding and Parameter Specification via the DNS. — SVCB and HTTPS record types; `SvcbRecord`, `resolve-svcb`, and `http-connect` alias-chain logic in `stdlib/protocols/http.llt`.
- Pauly, T. et al. (2023). RFC 8305 — Happy Eyeballs Version 2. — Parallel A/AAAA resolution and staggered connection racing; extended here to the protocol dimension (h3/QUIC vs h2/TCP).
