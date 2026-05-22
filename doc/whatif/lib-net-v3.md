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

### `make-serve-layer` and `make-multiplex-serve`

```tinct
# 1:1 — each incoming connection is transformed into one outgoing connection.
# accept-fn runs in its own task so handshakes proceed concurrently — the loop
# returns to recv immediately without waiting for the handshake to complete.
# (TLS, WireGuard, Noise Protocol, SSH transport, ...)
make-serve-layer: [fn [let accept-fn]
  [fn [let conn-ch config]
    [out: [channel 100]]
    [task [loop [fn [let]
      [raw: [recv conn-ch]]
      [task [send out [accept-fn raw config]]]]]]
    out]]

# 1:N — each incoming connection produces multiple items.
# (HTTP/1.1 keep-alive, HTTP/2 streams, HTTP/3 QUIC streams, WebSocket frames, ...)
make-multiplex-serve: [fn [let conn-fn]
  [fn [let conn-ch]
    [out: [channel 1000]]
    [task [loop [fn [let]
      [conn: [recv conn-ch]]
      [task [conn-fn conn out]]]]]
    out]]
```

Both loops are fire-and-forget tasks (`[task [loop ...]]`). `recv` returns `T` directly and suspends until a value is available — it does not return `Result`. Loop termination happens via context cancellation: when the server shuts down (e.g., `exit` or `drain` from `stdlib/async.llt`), `recv` on a cancelled context raises a cancellation error that propagates out of the loop task. No channel-closed signalling or explicit break is needed.

The inner handshake tasks (`[task [send out [accept-fn raw config]]]`) are also fire-and-forget and are **not** cancelled when the outer loop exits — they run to completion or until they error. This is correct: aborting a half-completed TLS handshake would leave the client in a broken state. For clean server shutdown that waits for in-flight handshakes to complete, call `drain` from `stdlib/async.llt` at the top level before `exit`.

Concrete serve layers:

```tinct
# Connection-promotion: one connection in, one upgraded connection out
tls-serve:       [make-serve-layer tls-accept]        # ByteStream → TlsConnection
wireguard-serve: [make-serve-layer wireguard-accept]  # ByteStream → WireguardConnection
noise-serve:     [make-serve-layer noise-accept]      # ByteStream → NoiseConnection
h2-serve:        [make-serve-layer h2-accept]         # ByteStream → Http2Connection
h3-serve:        [make-serve-layer h3-accept]         # QuicConnection  → Http3Connection
ws-serve:        [make-serve-layer ws-accept]         # ByteStream → WebSocketConnection (MessageStream WsFrame)

# Message-extraction: one connection in, many protocol messages out
http1-serve:     [make-multiplex-serve http1-conn]    # ByteStream → RawRequest*
http2-requests:  [make-multiplex-serve http2-req-conn]# Http2Connection → RawRequest*
http3-requests:  [make-multiplex-serve http3-req-conn]# Http3Connection → RawRequest*
```

`wireguard-serve`, `noise-serve`, `ws-serve`, and `tls-serve` call tinct-implemented handshake functions in `stdlib/protocols/`. Each returns a typed tinct record (`WireguardConnection`, `NoiseConnection`, `WebSocketConnection`, `TlsConnection`) that implements `ByteStream`. Because all `*-serve` and `*-accept` functions are parametric over `ByteStream`, these records compose directly — `h2-serve` accepts `Channel@WireguardConnection` as readily as `Channel@Handle`. WireGuard is a user-mode protocol: Noise_IKpsk2 handshake in tinct, data plane over `udp-socket`, no kernel TUN/TAP.

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

Client-side 1:1 layers (`*-layer`) are the dual of `make-serve-layer` applied to a single connection. `connect-host` (from `stdlib/net.llt`) implements RFC 8305 Happy Eyeballs entirely in tinct using runtime-v2 primitives:

```tinct
# stdlib/net.llt — RFC 8305 Happy Eyeballs v2

connect-host: [fn [let cap@NetCap host@String port@Int]
  # Resolve both address families concurrently
  [v6-task: [task [dns-resolve cap host AAAA]]
   v4-task: [task [dns-resolve cap host A]]]
  # Prefer IPv6: use it if it arrives within 50ms of A records
  [v6-first: [match [timeout 50 v6-task]
    [Ok addrs]: addrs
    [Err _]:    []]]
  [v4-addrs: [await v4-task]]
  [v6-addrs: [if [= [] v6-first]
    [match [try [await v6-task]] [Ok a]: a [Err _]: []]
    v6-first]]
  # Interleave families (IPv6 preferred per RFC 6724), race with 250ms stagger
  [happy-connect cap port [interleave v6-addrs v4-addrs]]]

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
  [sock: [udp-socket cap 0]]   # ephemeral local port
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

  # Use same 50ms window as AAAA preference — if SVCB arrives first, use it
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

**`make-serve-layer` and non-ByteStream connections:** `make-serve-layer` is fully polymorphic — it places no `ByteStream` constraint on the `accept-fn`. A function `my-proto-accept: [Fn [WebSocketConnection config] MyConnection]` composes with `make-serve-layer` just as `tls-accept` does. The serve layer model works for any connection type, not only `ByteStream` instances.

---

## New Rust Primitives

The primitives follow two rules: (1) the transport boundary maps 1:1 to cap-std's networking API — no higher-level protocol logic belongs in Rust; (2) all crypto operations are Rust for security correctness, not performance, because tinct's lazy evaluator cannot guarantee constant-time execution paths and variable-time crypto leaks secret key material via timing side-channels.

### Transport Primitives

These correspond directly to cap-std's `TcpListener`, `UdpSocket`, and `TcpStream` types. Everything above the OS socket layer — accept loops, connection multiplexing, protocol handshakes — is tinct.

| Primitive | Signature | Description | Why Rust |
|-----------|-----------|-------------|----------|
| `tcp-bind` | `[Fn [cap@NetCap port@Int] TcpListener]` | Bind and listen on a TCP port | OS syscall (socket + bind + listen); cap-std capability enforcement; produces `TcpListener` opaque handle |
| `tcp-accept` | `[Fn [listener@TcpListener] Handle]` | Accept one incoming TCP connection (async, suspends until one arrives) | OS accept() syscall + tokio reactor registration; tinct tasks cannot make socket syscalls |
| `tcp-connect` | `[Fn [cap@NetCap addr@SocketAddress] Handle]` | Connect to a TCP endpoint | cap-std `Pool::connect(SocketAddress)` — capability check and socket syscall are one indivisible operation; no TOCTOU window |
| `udp-socket` | `[Fn [cap@NetCap port@Int] UdpSocket]` | Bind a UDP socket — use port 0 for ephemeral (client use) | OS syscall; cap-std UdpSocket capability enforcement; produces `UdpSocket` opaque handle |
| `udp-recv` | `[Fn [sock@UdpSocket] UdpDatagram]` | Receive one UDP datagram with source address (async) | Reads from `UdpSocket` opaque Rust state; tinct cannot access UdpSocket internals |
| `udp-send` | `[Fn [sock@UdpSocket addr@SocketAddress data@Bytes] Null]` | Send a datagram to a specific address | Writes through `UdpSocket` opaque Rust state |
| `unix-listen` | `[Fn [cap@DirCap path@String] [Channel Handle]]` | Incoming Unix socket connections | cap-std's `UnixListener` is not implemented in cap-std upstream; this primitive wraps the accept loop internally using `openat2(RESOLVE_BENEATH)` + raw `UnixListener`. When cap-std adds `UnixListener`, this refactors to `unix-bind` + `unix-accept` following the same pattern as `tcp-bind`/`tcp-accept` |
| `read-bytes` | `[Fn [h@Handle n@Int] Bytes]` | Read exactly n bytes from a Handle (async, suspends until available) | Handle is opaque Rust state backed by tokio `AsyncRead`; tinct cannot access Handle internals or call tokio I/O directly |
| `write-bytes` | `[Fn [h@Handle b@Bytes] Null]` | Write bytes to a Handle (async) | Handle is opaque Rust state backed by tokio `AsyncWrite` |
| `try-recv` | `[Fn [ch@[Channel t]] [Result t]]` | Non-blocking recv — `Err` if no item immediately available | Requires reading Channel's internal buffer occupancy without consuming an item; `select-once` + 0ms-timer is scheduler-dependent and not guaranteed non-blocking |

`tcp-listen`, `udp-bind`, `quic-listen`, and all higher-level connection factories are tinct. `read-bytes` and `write-bytes` are the `Handle` instance methods for `ByteStream`; `udp-recv` and `udp-send` are the `UdpSocket` instance methods for `Datagram`. All are called through typeclass dispatch in tinct code.

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
| `hkdf-extract` | `[Fn [hash@Symbol  salt@Bytes  input-key-material@Bytes] [Bytes 32]]` | HKDF-Extract (hash: `Sha256` `Sha384` `Sha512` `Blake2s`) | Constant-time; output size = hash output size; shown for Sha256 |
| `hkdf-expand` | `[Fn [hash@Symbol  pseudorandom-key@Bytes  info@Bytes  len@Int] Bytes]` | HKDF-Expand | Constant-time; output length is runtime `len` — returns `Bytes`, annotate `@[Bytes N]` at call site |
| `crypto-random` | `[Fn [len@Int] Bytes]` | Cryptographically secure random bytes | OS entropy source; length is runtime — returns `Bytes`, annotate `@[Bytes N]` at call site |

---

## Stdlib Module Map

```text
stdlib/
  net.llt           — ByteStream typeclass + [instance [ByteStream Handle] ...]
                      Datagram typeclass + [instance [Datagram UdpSocket] ...]
                      MessageStream typeclass + [instance [MessageStream [Channel t] t] ...]
                      ByteStream typeclass + [instance [ByteStream Handle] ...]
                      Datagram typeclass + [instance [Datagram UdpSocket] ...]
                      MessageStream typeclass + [instance [MessageStream [Channel t] t] ...]
                      IpAddress ([union [Ipv4 [Bytes 4]] [Ipv6 [Bytes 16]]])
                      Port, SocketAddress, UdpDatagram types

                      ByteLabel — the unified encode/decode typeclass for all labeled bytes.
                      TextCodec, ByteOrder, and CompressionCodec are all instances where
                      encode means "serialize value → Bytes" and decode means "Bytes → value".
                      The value type a and bytes type b vary per instance.

                        [class [ByteLabel t a b]
                          FD: [(t b) → a]   # knowing label type + bytes type uniquely determines value type;
                                             # required to resolve [decode BigEndian some-bytes] unambiguously
                          encode: [Fn [t@t a] b]
                          decode: [Fn [t@t b] [Result a]]]

                        # TextCodec: String ↔ Bytes (decode can fail on invalid sequences)
                        [instance [ByteLabel Codec String Bytes] ...]

                        # ByteOrder: UInt ↔ fixed-size Bytes (decode always succeeds)
                        [instance [ByteLabel ByteOrder UInt16 [Bytes 2]] ...]
                        [instance [ByteLabel ByteOrder UInt32 [Bytes 4]] ...]
                        [instance [ByteLabel ByteOrder UInt64 [Bytes 8]] ...]

                        # CompressionCodec: Bytes ↔ Bytes (decode fails on corrupt data)
                        [instance [ByteLabel CompressionCodec Bytes Bytes] ...]

                      Unified call syntax regardless of which label type t is:
                        [encode UTF8      "hello"]           # → Bytes
                        [encode BigEndian 80@UInt16]         # → [Bytes 2]
                        [encode Gzip      body-bytes]        # → Bytes
                        [decode UTF8      raw]               # → [Result String]
                        [decode BigEndian @[Bytes 4]: ip]    # → [Result UInt32]
                        [decode Gzip      compressed]        # → [Result Bytes]

                      ByteOrder — wire endianness, independent of executing CPU:
                        [type [BigEndian]]    — network byte order (IPv4, DNS, SNMP, HTTP/2, QUIC)
                        [type [LittleEndian]] — Bluetooth LE integers, some Microsoft formats
                        [type [NativeEndian]] — executing CPU; only for same-machine IPC;
                                                silently wrong for cross-platform protocol code
                        OrderedBytes: [type [data: Bytes  label: ByteOrder]]
                          — bytes labeled with their wire endianness (e.g. SNMP counter).
                          Note: labels a contiguous region with a SINGLE byte order. For
                          mixed-endian protocols (SMB fields, SCTP chunk flags, some vendor
                          formats), parse fields individually with explicit encode/decode
                          calls per field — OrderedBytes is not suitable for heterogeneous structs.

                      Nameserver ([union UdpNameserver DotNameserver DohNameserver DoqNameserver]); ns-to-resolver
                      tcp-listen  (tcp-bind + tinct accept loop)
                      udp-bind    (udp-socket + tinct recv loop, Channel@UdpDatagram)
                      quic-listen (udp-socket + quic.llt accept loop, Channel@QuicConnection)
                      tcp-connect (takes SocketAddress — calls pool.connect atomically)
                      resolve-host  (tries each %dns.nameserver in order; returns [Seq IpAddress])
                      dns-query-first, ns-to-resolver
                      connect-host  (RFC 8305 Happy Eyeballs — resolve-host + happy-connect)
                      parse-url, url-encode/decode
  text.llt          — Character encoding for text protocols and file I/O.
                      String in tinct is always Unicode (UTF-8 internally). Encoding is a
                      property of the wire representation, not the string.

                      TextCodec is a compile-time typeclass. User code defines instances
                      in tinct — no Rust changes required. Because tinct has structural
                      typing, any record with the matching function fields satisfies
                      TextCodec automatically; an explicit [instance ...] declaration is
                      optional but makes intent clear and enables typeclass dispatch.

                      [class [TextCodec c]
                        encode:       [Fn [c@c s@String] [Result Bytes]]
                        decode:       [Fn [c@c b@Bytes]  [Result String]]
                        decode-lossy: [Fn [c@c b@Bytes]  String]
                        codec-name:   [Fn [c@c] String]]

                      # compile-time polymorphic dispatch
                      encode:       [fn@[bind: [c]  return: [Result Bytes]   constraint: [c: TextCodec]] [let c@c s@String] ...]
                      decode:       [fn@[bind: [c]  return: [Result String]  constraint: [c: TextCodec]] [let c@c b@Bytes] ...]
                      decode-lossy: [fn@[bind: [c]  return: String           constraint: [c: TextCodec]] [let c@c b@Bytes] ...]

                      # Built-in codec types and instances (backed by encoding_rs Rust crate)
                      [type [UTF8]] [instance [TextCodec UTF8] ...]
                      [type [UTF16LE]] [instance [TextCodec UTF16LE] ...]
                      [type [Windows1252]] [instance [TextCodec Windows1252] ...]
                      # ... all ~38 WHATWG encodings

                      # User-defined codec — declare type and instance in tinct; no Rust needed:
                      # [type [EBCDIC037]]
                      # [instance [TextCodec EBCDIC037]
                      #   encode:     ebcdic037-encode
                      #   decode:     ebcdic037-decode
                      #   decode-lossy: ebcdic037-decode-lossy
                      #   codec-name: [fn [_] "ibm037"]]

                      # For dynamic dispatch (codec determined at runtime from Content-Type
                      # header etc.), an explicit dictionary is needed. Codec is the runtime
                      # representation of a TextCodec instance:
                      Codec: [type
                        [name:         String
                         encode:       [Fn [String] [Result Bytes]]
                         decode:       [Fn [Bytes]  [Result String]]
                         decode-lossy: [Fn [Bytes]  String]]]

                      to-codec: [fn@[bind: [t]  return: Codec  constraint: [t: TextCodec]] [let c@t] ...]
                        — converts any TextCodec instance to an explicit Codec dictionary

                      codec-for-name: [Fn [name@String] [Result Codec]]
                        — dynamic lookup from IANA/WHATWG charset name;
                          "utf-8", "windows-1252", "shift_jis", "x-sjis", "iso-8859-1" ...

                      # decode notes:
                      #   encode: Err if String contains chars not in codec's repertoire
                      #   decode: Err if bytes invalid for codec; Shift-JIS/GBK byte ranges
                      #           overlap valid UTF-8 — always specify codec, never guess
                      #   decode-lossy: replaces undecodable bytes with U+FFFD

                      TextBytes: [type [data: Bytes  codec: Codec]]
                        — stores the explicit Codec dictionary for round-trip fidelity;
                          used in HTTP response bodies, file reads, any boundary where the
                          encoding must survive as a runtime value

                      text-encode: [fn@[bind: [t]  return: TextBytes  constraint: [t: TextCodec]] [let c@t s@String] ...]
                      text-decode: [Fn [b@TextBytes] [Result String]]
```

**Example — built-in codec, dynamic charset from HTTP header:**

```tinct
# Decode an HTTP response body using the charset declared in Content-Type.
# Falls back to UTF-8 (the correct default per HTML5) if charset is absent or unknown.
decode-http-body: [fn [let body@Bytes content-type@String]
  [codec: [match [extract-charset content-type]  # parse "charset=windows-1252"
    [Some name]: [match [codec-for-name name]
      [Ok c]:   c
      [Err _]:  UTF8]     # unknown charset → UTF-8 fallback
    [None]:      UTF8]]   # no charset declared → UTF-8 default
  [decode-lossy codec body]]  # decode-lossy: replace undecodable bytes with U+FFFD
                               # rather than erroring on the common "mostly UTF-8" web page
```

**Example — user-defined EBCDIC-037 codec (no Rust changes required):**

```tinct
# EBCDIC-037 (IBM US English) implemented entirely in tinct using lookup tables.
# No Char type in tinct — single-character String is the unit.
# [Seq String] avoids integer dict keys: position = EBCDIC byte (0–255), value = character.

ebcdic037-decode: @[Seq String]:
  [" " "" "" "" "" "\t"     "" ""   # 0x00–0x07
   "" "" "" "" "" "\r"     "" ""   # 0x08–0x0F
   # ... (full 256-entry table omitted for brevity) ...
   " "                                                                         # 0x40 = space
   # ...
   "A" "B" "C" "D" "E" "F" "G" "H" "I"                                       # 0xC1–0xC9
   # ...
   "0" "1" "2" "3" "4" "5" "6" "7" "8" "9"]                                  # 0xF0–0xF9

# Reverse: single-character String → EBCDIC byte.
# Built from the decode table so the two always stay in sync.
ebcdic037-encode: [reduce
  [fn [let table pair]
    [let [idx: [nth pair 0]   ch: [nth pair 1]]]
    [if [= ch ""] table [assoc table ch idx]]]
  []
  [map-indexed [fn [let i ch] [i ch]] ebcdic037-decode]]

# Declare the type and instance
[type [EBCDIC037]]

[instance [TextCodec EBCDIC037]
  codec-name:   [fn [_] "ibm037"]

  encode: [fn [let _ s@String]
    [match [try [map [fn [let c]
          [match [get? ebcdic037-encode c]
            [Some byte]: byte
            [None]:      [error [str "Not in EBCDIC-037: " c]]]]
        [str-to-chars s]]]
      [Ok bytes]: [Ok [bytes-from-ints bytes]]
      [Err e]:    [Err e]]]

  decode: [fn [let _ b@Bytes]
    [Ok [str-join "" [map [fn [let byte] [get ebcdic037-decode byte]] b]]]]

  decode-lossy: [fn [let _ b@Bytes]
    [str-join "" [map [fn [let byte]
        [let [ch: [get ebcdic037-decode byte]]]
        [if [= ch ""] "?" ch]]      # substitute "?" for undefined code points
      b]]]]

# Usage — identical call sites to built-in codecs; typeclass dispatch resolves the instance
[encode EBCDIC037 "Hello, IBM!"]                         # → [Ok Bytes]
[decode EBCDIC037 some-mainframe-bytes]                  # → [Ok String] or [Err ...]
[text-encode [to-codec EBCDIC037] "Hello"]            # → TextBytes (explicit Codec)
[text-decode [text-encode [to-codec EBCDIC037] "Hello"]]  # → [Ok "Hello"]
```

The EBCDIC codec participates in `encode`/`decode`/`text-encode`/`text-decode` identically to the built-in codecs. The only difference between `EBCDIC037` and `UTF8` at the call site is the name — they are both `Codec` records with the same three function fields.

```text
  crypto.llt        — X.509: parse-cert (ASN.1 DER in tinct via read-bytes/bytes-get),
                              verify-cert-chain, cert-public-key, cert-san
                      Key wrappers: parse-rsa-public-key, parse-ec-public-key
                      No arithmetic — all crypto math is in Rust primitives above
  serve.llt         — make-serve-layer, make-multiplex-serve
                      Connection-promotion: tls-serve, wireguard-serve, noise-serve,
                        h2-serve, h3-serve, ws-serve
                      Message-extraction: http1-serve, http2-requests, http3-requests
                      Stream adapters: quic-stream-ch, h2-stream-ch, h3-stream-ch
  dns.llt           — dns-resolve (returns [Seq IpAddress] for A/AAAA/etc.)
                      resolver factories: dns-udp-resolver, dns-tls-resolver,
                        dns-https-resolver (all take SocketAddress, not hostname)
                      dns-server-loop.
                      Note: dns-try-names tries candidates in order; for each candidate it
                      tries all nameservers before moving to the next candidate. In
                      Kubernetes (ndots:5, 5 search domains), a slow nameserver adds
                      (attempts × nameserver-count) seconds of delay before the absolute
                      name is tried. For latency-sensitive resolution, limit attempts to 1
                      or use a single fast nameserver.
  async.llt         — (extended from runtime-v2) adds:
                        collect-channel: drain immediately-available items using try-recv,
                          bounded to items present at call time (snapshots channel size
                          before draining to prevent racing a concurrent producer)
  protocols/
    tls.llt         — TLS 1.3 record layer (read-bytes/write-bytes framing) + handshake
                      state machine + certificate chain verification (via crypto.llt);
                      TlsConnection tinct record: [underlying: Handle  write-key/read-key: [Bytes 32]
                        cipher: Symbol  write-seq/read-seq: [Channel Int]];
                      [instance [ByteStream TlsConnection] ...];
                      tls-accept, tls-connect: [fn@[bind: [h]  return: TlsConnection  constraint: [h: ByteStream]] ...]
    quic.llt        — QUIC built on udp-socket/recv/send + tls.llt:
                      connection ID parsing, packet number spaces, TLS integration,
                      stream multiplexing, flow control, loss detection, congestion control;
                      QuicConnection tinct record: [socket streams crypto-state ...];
                      quic-connect, h3-open-stream, h3-stream-ch.
                      Known limitation: connection migration (RFC 9000 §9) is not supported
                      in the initial implementation — packets arriving from a new source
                      address are rejected. Migration is a future sprint.
    h2.llt          — HTTP/2 frame parsing, HPACK (static + dynamic table + Huffman in tinct),
                      stream multiplexing, flow control;
                      Http2Connection tinct record: [handle streams hpack-table ...];
                      h2-accept, h2-connect, http-request, h2-push, h2-open-stream,
                      h2-incoming, http2-client, h2-stream-ch
    h3.llt          — HTTP/3 on top of quic.llt: QPACK header compression,
                      request/response mapping to QUIC streams;
                      h3-accept, h3-connect, http3-client
    http1.llt       — HTTP/1.1 framing in pure tinct on top of read-bytes/write-bytes:
                        parse-request, write-response, serve-conn (server)
                        build-request, send-request, parse-response (client)
    http.llt        — http-channel (unified server: TCP+QUIC via serve.llt)
                      SvcbRecord ([union AliasMode ServiceMode]), resolve-svcb (alias chain)
                      http-connect (SVCB-aware: IP hints + h3/h2 protocol race)
                      http-protocol-race, quic-h3-connect, tcp-h2-connect
                      fetch (calls http-connect; transparent h3/h2/h1 negotiation;
                             negotiates Accept-Encoding and decompresses response body);
                      fetch-h1, fetch-h2, fetch-h3 (protocol-pinned variants)
                      router, headers-map, parse-query
                      ok, json-ok, redirect, not-found, server-error
                      with-compression (adds Content-Encoding to responses;
                        SECURITY: disable for endpoints that reflect user input alongside
                        secrets — CRIME/BREACH attacks exploit compression ratio leakage
                        when attacker-controlled and secret bytes share a compressed stream)
                      with-logging, with-cors, with-auth, with-timeout (middleware)
    compress.llt    — CompressionCodec typeclass + built-in instances (Rust-backed;
                      streaming compression through the thunk system would be impractically
                      slow for multi-KB HTTP bodies):
                      [class [CompressionCodec c]
                        encode: [Fn [c@c b@Bytes] Bytes]           # compress
                        decode: [Fn [c@c b@Bytes] [Result Bytes]]]  # decompress; Err if corrupt
                      [type [Gzip]]    [instance [CompressionCodec Gzip] ...]     — gzip (RFC 1952)
                      [type [Deflate]] [instance [CompressionCodec Deflate] ...]  — zlib (RFC 1950)
                      [type [Brotli]]  [instance [CompressionCodec Brotli] ...]   — br (RFC 7932)
                      [type [Zstd]]    [instance [CompressionCodec Zstd] ...]     — zstd (RFC 8878)
                      [type [Identity]] — no-op passthrough; compress/decompress are identity
                      CompressedBytes: [type [data: Bytes  label: CompressionCodec]]
                        — LabeledBytes@CompressionCodec; used for raw HTTP response bodies
                          before decompression, or for compressed file/stream data
    wireguard.llt   — WireGuard user-mode protocol (Noise_IKpsk2) using x25519-dh,
                      chacha20-poly1305, blake2s/blake2s-mac, hkdf-extract;
                      WireguardConnection tinct record: [underlying: Handle
                        tx-key/rx-key: [Bytes 32]  tx-nonce: [Channel Int]];
                      [instance [ByteStream WireguardConnection] ...];
                      wireguard-accept, wireguard-layer: [fn@[bind: [h]  return: WireguardConnection  constraint: [h: ByteStream]] ...]
    noise.llt       — Generic Noise pattern combinator (XX, IK, NK, …);
                      NoiseConnection tinct record: [underlying: Handle
                        send-key/recv-key: [Bytes 32]  send-n: [Channel Int]];
                      [instance [ByteStream NoiseConnection] ...];
                      noise-accept, noise-layer: [fn@[bind: [h]  return: NoiseConnection  constraint: [h: ByteStream]] ...]
    websocket.llt   — WebSocket upgrade (sha1), frame framing/deframing,
                      masking (crypto-random + bytes-xor);
                      WsFrame: [union Text Binary Ping Pong Close];
                      WebSocketConnection tinct record: [underlying: Handle  server-side: Bool];
                      [instance [MessageStream WebSocketConnection WsFrame] ...];
                      ws-accept, ws-connect: [fn@[bind: [h]  return: WebSocketConnection  constraint: [h: ByteStream]] ...]
    icmp.llt        — ICMP framing on top of connect cap Icmp Handle;
                      EchoRequest/EchoReply types, read-icmp, send-icmp
    socks5.llt      — SOCKS5 proxy protocol
    grpc.llt        — gRPC framing over h2.llt
    mqtt.llt        — MQTT frame parsing
```

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
  [Ipv4 [Bytes 4]]     # [Ipv4 @[Bytes 4]: [192 168 1 1]]
  [Ipv6 [Bytes 16]]]   # [Ipv6 @[Bytes 16]: [32 1 13 184 0 0 0 0 0 0 0 0 0 0 0 1]]

Port:      [type Int@[is: [between 1 65535]  repr: u16]]
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

### Restructure `Value::NetCap` — hostname entries + cap-std Pool

`Value::NetCap` currently holds `Rc<Vec<NetCapEntry>>` and is checked by a custom `check_net_cap_allowlist` function in `builtins_io.rs`. This design does not use cap-std's networking capability primitives.

Replace with a hybrid backed by cap-std's `Pool`:

```rust
struct NetCapInner {
    // Hostname-level allowlist: checked by resolve-host/dns-resolve in tinct
    // before DNS resolution. Supports glob patterns that Pool cannot express.
    hostname_entries: Vec<NetCapEntry>,   // Hostname, HostPort, HostnameGlob, Any

    // IP-level allowlist: CIDR ranges and exact addresses, backed by cap-std.
    // tcp-bind, tcp-accept, udp-socket, tcp-connect all go through pool.
    pool: cap_std::net::Pool,
}
```

`tcp-connect: [Fn [cap@NetCap addr@SocketAddress] Handle]` takes a pre-resolved `SocketAddress` — it calls `pool.connect(SocketAddress)` directly. The pool check and the socket syscall are one indivisible operation; there is no window between the capability check and the actual connect. This eliminates the TOCTOU hazard present in the current design (hostname check → DNS resolution → connect to potentially different IP).

`resolve-host: [Fn [cap@NetCap host@String] IpAddress]` in `stdlib/net.llt` checks `hostname_entries` against the hostname before resolving, then validates the resolved `IpAddress` against the pool's CIDR entries as a defence-in-depth step (the pool check at `tcp-connect` is the authoritative gate, but catching the mismatch earlier produces a clearer error). The validated `IpAddress` is composed with a port into a `SocketAddress` and passed to `tcp-connect`.

`--cap-net` parsing splits entries by type: glob/hostname entries populate `hostname_entries`; CIDR and IP-literal entries populate the pool via `Pool::insert_ip_net` and `Pool::insert`.

`tcp-bind` and `udp-socket` similarly go through `pool.bind(SocketAddress)` rather than the custom allowlist check.

### `tcp-connect` takes an IP address, not a hostname

The Rust primitive `tcp-connect` takes a resolved `SocketAddress`. DNS resolution is a higher-layer tinct concern handled by `%dns` and `stdlib/net.llt`. TLS SNI is the hostname, not the IP — passed explicitly in `tls-config` as `sni: host`.

### Inject `%dns`

`%dns` is a `DnsConfig` record injected at process start. The `%` prefix means it is injected from the system environment (like `%libdir`), not that it is a security capability. The security boundary for DNS is `%net-cap` — it gates the UDP/TCP packets sent to the nameserver. `%dns` is resolver configuration: where to send queries, how to expand short names, how long to wait. A program that has `%net-cap` but no `%dns` can still do `tcp-connect` with a pre-resolved `SocketAddress`; it just cannot do hostname resolution.

Because tinct reads `/etc/resolv.conf` directly rather than delegating to glibc, it takes responsibility for implementing everything glibc would have handled automatically.

```tinct
# stdlib/net.llt

Nameserver: [union
  [UdpNameserver  addr@SocketAddress]                          # UDP/53 — standard; from /etc/resolv.conf
  [DotNameserver  addr@SocketAddress  sni@String]              # DNS-over-TLS (port 853)
  [DohNameserver  addr@SocketAddress  sni@String  path@String] # DNS-over-HTTPS
  [DoqNameserver  addr@SocketAddress  sni@String]]             # DNS-over-QUIC (port 853)

DnsConfig: [type {
  nameservers: [Seq Nameserver]  # pre-resolved SocketAddresses — no circular DNS dependency
  search:      [Seq String]      # search domain list: "search corp.example.com example.com"
  ndots:       Int               # dot threshold for search-first vs absolute-first; default 1
  timeout:     Duration          # per-query timeout; default [seconds 5]
  attempts:    Int               # retries per nameserver before moving to next; default 2
  rotate:      Bool              # round-robin across nameservers; default false
  no-aaaa:     Bool              # suppress AAAA queries; default false
  edns0:       Bool              # EDNS0 extended responses; default true
}]
```

All `Nameserver` variants take a pre-resolved `SocketAddress` — no DNS needed to reach the nameserver itself. `/etc/resolv.conf` has no protocol-selection syntax (only `options use-vc` for TCP/53, not TLS), so parsing always produces `UdpNameserver` entries. On modern Linux with systemd-resolved, `/etc/resolv.conf` typically lists `127.0.0.53` — the local stub. DoT/DoH configuration in `/etc/systemd/resolved.conf` affects the stub invisibly; tinct sees `UdpNameserver { addr: 127.0.0.53:53 }` and the stub handles upstream protocol. Users who want tinct itself to speak DoT/DoH to a remote resolver set `--nameservers` explicitly.

**CLI:**
```
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
