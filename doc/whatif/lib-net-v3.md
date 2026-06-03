# What If: Network Serve and Connect Layers (lib-net-v3)

TODO: make sure we have robust error reporting and handling capabilities. Specific nominal Error types matching TLS errors / DNS errors / etc?

**State:** Draft — 2026-05-21

**Depends on:** [`runtime-v2.md`](runtime-v2.md) ✓ complete — `task`/`await`/`channel`/`recv`/`send`/`select-once`/`loop-select`, `Arc`-based thunks, async Tokio runtime. All async utilities are in `stdlib/prelude.llt` (merged from the now-deleted `stdlib/async.llt`). All concurrency primitives are used here without redefinition. `tcp-connect` is the only Rust primitive here — `tls`, `quic`, `h2`, `h3`, and `connect` are tinct stdlib functions defined in this whatif.

**Depends on:** [`builtin-privacy.md`](builtin-privacy.md) ✓ complete — `--- uses: ["net"]` headers, `builtin_module("net")` registry, `eval-programs`, `eval-program`, `module` and related bootstrap machinery are all in place. Net stdlib files declare `--- uses: ["net"]` to access their Rust builtins. `select-once` returns `[Ok v]` | `[Closed]` (B-192); `loop-select` is pure tinct. `recv` also returns `[Ok v]` | `[Closed]` for uniform channel semantics.

**Uri/Url:** `Uri` is currently an opaque Rust object. Making it a tinct-native data structure is confirmed scope for this whatif — the `builtin_uri` function already returns a `Value::Dict` of URI components rather than a `Value::Uri`, so the decomposition is implemented; what remains is formalising the type in `stdlib/net.llt`.

**Extends:** [`completed/lib-tls.md`](completed/lib-tls.md), [`completed/lib-net-v2.md`](completed/lib-net-v2.md) — replaces the opaque-Rust-type model for TLS, HTTP/2, HTTP/3, QUIC, WebSocket, WireGuard, and Noise with tinct stdlib implementations; adds the compositional serve/connect layer model; introduces `[Bytes N]` fixed-size byte types and a `cap-std::Pool`-backed `NetCap`.

**Open question for this whatif to answer:** Binary integer serialization semantics for `stdlib/numeric.llt`'s `to-bytes`. Protocol encoding (DNS packet fields, TLS record headers, HTTP/2 frame lengths) requires converting integers to fixed-width big-endian bytes. This whatif must specify: the `to-bytes: Int × Int → Bytes` primitive (value + byte-width, big-endian, two's complement), its interaction with `[Bytes N]` fixed-size types, and where it lives (`stdlib/numeric.llt` or `stdlib/encoding.llt`).

---

## Problem

After runtime-v2 provides the async foundation (`task`, `await`, `channel`, `select-once`, `loop-select`), building a network server still requires boilerplate: accept a connection, hand it off to a task, loop. Every protocol layer adds the same pattern. There is no compositional model for stacking protocol layers, no separation between transport (how bytes move) and application protocol (what the bytes mean), and no typed representation for fixed-size byte sequences like IP addresses and cryptographic keys.

---

## Design Principle: Subject Last

Every function in this API places the **subject** — the thing being transformed, wrapped, or consumed — as the **last parameter**. This is the same convention already used throughout tinct's prelude (`map f seq`, `filter pred seq`, `reduce f init seq`).

The consequence: any pipeline stage works directly with `|` and composes via partial application:

```tinct
[connect %net Tcp [HostPort [Hostname "api.example.com"] 443]]
  | tls                      # pending TLS, sni inferred from hostname
  | h2                       # fires TLS with alpn: ["h2"], then H2 handshake
  | [http-request "GET" "/data"]
```

There are no `with-*` wrappers, no `via` / `layer` adapters, no argument-flipping combinators. If a function takes `(config, subject)`, it is a valid pipeline stage. This is the universal rule for all layer functions, codec stages, and middleware in this API.

**Layer naming convention:** Protocol layer functions are named after the protocol itself — `tls`, `quic`, `h2`, `h3`, `ws`, `wireguard`, `noise`. Types are PascalCase (`TlsConnection`, `QuicConnection`); there is no ambiguity. Server-side channel-map wrappers keep their `-serve` suffix (`tls-serve`, `h2-serve`) since they are channel transformers with a different shape.

---

## The Proposal

Define the **serve/connect layer model** as composable tinct stdlib functions. OS-level transport primitives (`tcp-bind`/`tcp-accept`, `udp-socket`/`udp-recv`/`udp-send`) are the only Rust boundary — exactly seven functions. Everything above — accept loops, connection multiplexing, protocol handshakes, HTTP/2, QUIC, TLS, WireGuard — is tinct. Client-side follows the same symmetry. Application protocols are codecs — pure marshal/unmarshal — independent of transport. `[Bytes N]` fixed-size byte sequences give the crypto and address types static size guarantees.

Three API levels, all built from the same components:

**Level 1 — One-liner.** `fetch` dispatches via the `Protocol` typeclass:

```tinct
[fetch %nc [Url "https://api.example.com/data"]]
[fetch %nc cloudflare-dot [dns-query "IN" "A" "example.com"]]
```

**Level 2 — Connection reuse or protocol selection.** Explicit transport + request:

```tinct
[c:    [connect %nc Http [HostPort [Hostname "api.example.com"] 443]]
 resp: [http-request c "GET" "/data"  user-agent: "tinct/0.1"]]
```

**Level 3 — Raw composition.** Full pipeline control for custom stacks:

```tinct
[[connect %nc Tcp [HostPort [Hostname "api.example.com"] 443]] | tls | h2 | [http-request req]]
[[connect %nc Tcp [HostPort [Hostname "redis.internal"]  6379]] | [codec RedisCodec]]
[[connect %nc Udp [HostPort [Hostname "vpn.example.com"] 51820]] | [wireguard cfg]]
```

`fetch` is not a special case — it uses `connect Http` which wires `connect Tcp | tls | h2` (or `quic | h3` if SVCB indicates). No hidden implementations. Users who define a new `Transport` instance and `Protocol` instance get Level 1 for free.

---

## The Symmetry

```text
                  Server (receive)                Client (initiate)
                  ──────────────────────          ──────────────────────
Transport     [listen/bind] → Channel@A       connect %net Proto [HostPort h p] → A
1:1 layer     [layer cfg] Channel@A→Channel@B [layer] A → B   (same name, dispatch by type)
data exchange Channel@conn | recv → Channel@req   conn | [send req] → Task@resp
```

Protocol layer functions are **bidirectional**: the same verb dispatches to client or server instance based on input type. `Channel@X` → server instance; single `X` → client instance:

```tinct
# Client: starts from connect
[connect %net Tcp [HostPort [Hostname "api.example.com"] 443]]
  | tls | h2 | [send req]                    # TcpHandle → TlsHandle → Http2Connection

[connect %net Udp [HostPort [Hostname "api.example.com"] 443]]
  | quic | h3 | [send req]                   # ConnectedUdp → QuicHandle → Http3Connection

# Server: starts from a listener
[tcp-listen cap 443] | [tls cert] | h2 | recv      # Channel@h → Channel@TlsConnection → Channel@Http2Connection
[udp-listen cap 443] | quic | h3 | recv             # UdpSocket → Channel@QuicConnection → Channel@Http3Connection

# Arbitrary stacking — same verbs, direction implicit from source
[udp-listen cap 443] | quic | h3 | substreams | [wireguard wg-cfg]
```

`HostPort` bundles address + port as a single subject — `[connect %net Tcp]` is a partial application waiting for an endpoint. On the client side, `connect` is always the bottom of the stack:

```tinct
# HTTPS/2 — sni and alpn inferred by tls/h2 layers
[connect %net Tcp [HostPort [Hostname "api.example.com"] 443]]
  | tls | h2 | [http-request req]

# HTTP/3 — quic creates QuicHandle; h3 adds alpn: ["h3"] before firing
[connect %net Udp [HostPort [Hostname "api.example.com"] 443]]
  | quic | h3 | [http-request req]

# DNS over TLS — tls fires with no alpn, dns-framed-send writes bytes
[connect %net Tcp [HostPort [Hostname "dns.cloudflare.com"] 853]]
  | tls | [dns-framed-send query]

# WireGuard VPN — noise-based, no TLS layer
[connect %net Udp [HostPort [Hostname "vpn.example.com"] 51820]]
  | [wireguard cfg]
```

`connect` is polymorphic on protocol via the `Transport` typeclass — `Tcp` produces a `Handle` (`ByteStream`); `Udp` produces a `ConnectedUdp` (`Datagram`). For `[Hostname h]`, Happy Eyeballs applies universally: parallel A+AAAA with 50ms IPv6 head start, interleaved addresses, then Tcp races connections (250ms stagger) while Udp picks the first.

A "layer violation" — a protocol that skips or reorders traditional stack layers — is expressed through its input and output types. `quic` expects a `Datagram`; it does not compose with a `Handle` (`ByteStream`). `tls-serve` expects `Channel@ByteStream`; it does not compose with `Channel@QuicConnection` (QUIC carries integrated TLS). No special cases: the type system is the rule.

---

## The `Transport` and `Protocol` Typeclasses

Two typeclasses partition the connection lifecycle. Together they make every protocol — built-in or user-defined — a first-class participant in the same system.

```tinct
# Transport: how to connect. p determines the connection type c.
[class [Transport p] determines: [p → c]
  connect: [Fn@c [NetCap Address Port p]]]

# Protocol: how to exchange. fetch dispatches by protocol type.
[class [Protocol p req resp]
  fetch: [Fn@resp [NetCap p req]]]
```

| Typeclass | Answers | Built-in instances | User-extensible? |
|---|---|---|---|
| `Transport p` | How do I connect? | `Tcp`, `Udp`, `Http` | Yes — define a new `[type [MyProto]]` |
| `Protocol p req resp` | How do I exchange? | `Http` (URL→response), `DoT`, … | Yes — implement for any type |

**`Http` as a Transport** bundles SVCB lookup + H3/H2/H1 negotiation — the same components available to power users, just pre-composed:

```tinct
# Http transport: SVCB-first, then H3 or H2 based on hints
[instance [Transport Http] determines: [Http → HttpConnection]
  connect: [fn [let cap addr port _@Http]
    # Resolve SVCB; if h3 available → connect Udp | quic | h3
    #              otherwise        → connect Tcp | tls | h2 (or h1 fallback)
    [error "TODO: Http transport — SVCB + protocol negotiation"]]]
```

**User-defined protocols** implement both typeclasses and get Level 1 (`fetch`) for free:

```tinct
# DNS over TLS — user-defined
DoT: [type [server: Address  port: Port]]

[instance [Transport DoT] determines: [DoT → TlsHandle]
  connect: [fn [let cap@NetCap _@DoT ep@HostPort]
    [TlsHandle [handle: [connect cap Tcp ep]  sni: null  alpn: []  ech: []  trust-roots: SystemRoots]]]]

[instance [Protocol DoT DnsQuery DnsResponse]
  fetch: [fn [let cap@NetCap proto@DoT req@DnsQuery]
    [th: [connect cap DoT [HostPort proto.server proto.port]]]
    [tls-commit th | [dns-framed-send req]]]]

# One definition — three call sites work:
cloudflare-dot: [DoT [server: [Hostname "dns.cloudflare.com"]  port: 853]]

[fetch %nc cloudflare-dot [dns-query "IN" "A" "example.com"]]                              # Level 1
[c: [connect %nc DoT [HostPort [Hostname "dns.cloudflare.com"] 853]]                       # Level 2
 [dns-framed-send [tls-commit c] query]]
[[connect %nc Tcp [HostPort [Hostname "dns.cloudflare.com"] 853]] | tls | [dns-framed-send q]] # Level 3
```

The same pattern works for any protocol: gRPC (HTTP/2 with ALPN `"h2"`), MQTT, custom binary protocols over TLS. Implement `Transport` + `Protocol`; the rest of the stack composes automatically.

---

## Transparent Handle Design

`Handle` is the unified ByteStream interface from the Rust layer — a single opaque type for any sequential I/O stream (TCP, file, pipe). Tinct wraps it in transparent records that carry visible metadata. Since tinct is lazy, the `stream` field is a thunk — the OS connection happens on first access, memoized:

```tinct
Handle:           # opaque Rust — any ByteStream (TCP, file, pipe); base ByteStream instance
TcpHandle:  [type [addr: Address  port: Port  stream: Handle]]
FileHandle: [type [path: String   mode: Symbol  file:   Handle]]
```

`connect Tcp HostPort` returns `TcpHandle`. `addr` is always visible — `tls` reads it to infer SNI from `[Hostname h]` without the hostname ever being lost. All handle types in the stack carry their construction parameters:

```tinct
TcpHandle:    [type [addr: Address  port: Port  stream: Handle]]          # connect Tcp HostPort
TlsHandle:    [type [handle: TcpHandle  sni: [or String Null]
                     alpn: [Seq String]  ech: Bytes
                     trust-roots: [or [Seq Bytes] Symbol]]]               # | tls
TlsConnection:[type [h] [underlying: h  cipher: CipherSuite  ...]]        # tls-commit (inside h2)
Http2Connection:[type [...]]                                               # | h2
```

**ALPN is owned by the layer that knows the protocol**, not by `tls` or `quic`:

```tinct
tls:         TcpHandle       → TlsHandle    (sni open, ALPN open)
tls-commit:  TlsHandle→ TlsConnection       (fires handshake with accumulated config)
h2:          TlsHandle→ Http2Connection     (with-alpn ["h2"] + tls-commit)
quic:        ConnectedUdp    → QuicHandle   (sni open, ALPN open)
quic-commit: QuicHandle→QuicConnection      (fires QUIC+TLS handshake)
h3:          QuicHandle→Http3Connection     (with-alpn ["h3"] + quic-commit)
```

**SNI is never specified twice.** `tls-commit` reads `handle.addr` → extracts `[Hostname h]` → SNI. Explicit override available when connecting to a direct IP with certificate validation.

```tinct
[connect %net Tcp [HostPort [Hostname "api.example.com"] 443]]   # TcpHandle
  | tls      # TlsHandle — sni open (inferred from addr.addr on commit), alpn open
  | h2       # with-alpn ["h2"] + tls-commit + H2 preface → Http2Connection
  | [http-request "GET" "/data"]
```

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

`Handle` is the opaque Rust I/O handle — the unified type for any sequential stream (TCP, file, pipe). It is the base `ByteStream` instance backed by `builtin-read-bytes`/`builtin-write-bytes`. `TcpHandle` and `FileHandle` are transparent tinct records that wrap `Handle` with visible metadata (`addr`, `path`, etc.). Every protocol layer above produces a new `ByteStream` instance — a tinct record wrapping the layer below:

```tinct
# stdlib/net.llt
[instance [ByteStream Handle]
  read-bytes:  builtin-read-bytes    # Rust: tokio AsyncRead
  write-bytes: builtin-write-bytes]  # Rust: tokio AsyncWrite

# stdlib/protocols/tls13.llt
TlsConnection: [type [h]   # h must satisfy ByteStream
  [underlying: h
   write-key:  [Bytes 32]
   read-key:   [Bytes 32]
   write-iv:   [Bytes 12]
   read-iv:    [Bytes 12]
   cipher:     CipherSuite        # Aes128GcmSha256 | Aes256GcmSha384 | Chacha20Poly1305Sha256
   write-seq:  [Channel Int]     # TLS sequence number (XOR'd with IV per RFC 8446 §5.3)
   read-seq:   [Channel Int]
   sni:        [or String Null]]]
[instance@[bind: [h]] [ByteStream [TlsConnection h]]
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
h2-accept:        [fn@[bind: [t]  return: Http2Connection      constraint: [t: ByteStream]] [let h@t]                            ...]
tls-accept:       [fn@[bind: [t]  return: TlsConnection        constraint: [t: ByteStream]] [let cfg@TlsServerConfig h@t]       ...]
wireguard-accept: [fn@[bind: [t]  return: WireguardConnection  constraint: [t: ByteStream]] [let cfg@WireguardConfig h@t]       ...]
```

`wireguard-serve` produces `Channel@WireguardConnection`; `tls-serve` accepts `Channel@[ByteStream h]` — so `[raw-ch | [wireguard-serve wg-cfg] | [tls-serve server-cert]]` type-checks without any collapse to `Handle`.

### `Datagram`

An unordered, unreliable packet socket. Each send and receive is a discrete unit with its own source/destination address. UDP, ICMP, the QUIC packet layer, and WireGuard's UDP transport are all `Datagram`.

```tinct
[class [Datagram d]
  send: [Fn [d@d addr@SocketAddress data@Bytes] Null]
  recv: [Fn [d@d] UdpDatagram]]
```

`UdpSocket` (from `udp-socket`) is the base instance:

```tinct
# stdlib/net.llt
[instance [Datagram UdpSocket]
  send: builtin-udp-send   # Rust: UdpSocket::send_to
  recv: builtin-udp-recv]  # Rust: UdpSocket::recv_from
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

**Every `MessageStream` is a repluggable handle.** `rebind` is a universal operation that swaps the underlying routing implementation without the senders knowing. This is the same model as Unix file descriptors: always a handle, always rebindable via `dup2`. There is no distinction between "proxy" and "non-proxy" MessageStreams — repluggability is simply how `MessageStream` works.

```tinct
[class [MessageStream s t]
  # FD: s determines t (each MessageStream type has exactly one message type)
  send: [Fn [s@s t] Null]        # serialize t → underlying transport
  recv: [Fn [s@s] [or [Ok t] [Closed]]]]  # deserialize → one complete t, or Closed on EOF

# rebind works on any MessageStream handle — swaps the routing implementation
rebind: [fn [let ms new-impl]
  [send ms.impl new-impl]]

# Constructor: create a repluggable MessageStream handle from an initial implementation
message-stream: [fn [let initial-impl]
  [MessageStreamHandle impl: [watch-channel initial-impl]]]
```

The method names are `send` and `recv` — the same names used everywhere for channel operations. There are no separate `send-message`/`recv-message` names; those were redundant with `send`/`recv`.

`Channel T` is the base instance — it already delivers typed values with no framing:

```tinct
[instance [MessageStream [Channel t] t]
  send: builtin-send
  recv: builtin-recv]   # returns [Ok v] | [Closed] (B-192)
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
  send: ws-send-frame    # encodes as WS frame, applies mask (client), writes to underlying
  recv: ws-recv-frame]   # reads from underlying, strips WS framing + mask, returns frame
```

`ws-serve` produces `Channel@WebSocketConnection`; the application calls `recv wsconn` to get `[Ok WsFrame]` or `[Closed]`, and calls `send wsconn reply`. gRPC bidirectional substreams, MQTT connections, and custom application protocols all implement `MessageStream T` for their respective message types.

### `%emit` as a `MessageStream` — the protocol stack model applied to CLI I/O

`%emit` is always a `MessageStream@Any` handle. It is created by `eval-programs` in `loader.llt` when data-streaming is accepted. The output program rebinds it via `rebind` — applying the codec layer in the same compositional model as TLS-over-TCP — without the user program's `[send %emit v]` calls knowing anything has changed.

```text
User code
  [send %emit v]                    ← MessageStream.send (unchanged after rebind)
      ↓
%emit: MessageStream@Any            ← repluggable handle; output formatter rebinds it
      ↓  codec (to-tinct, to-json, yaml, ...)
%stdout: ByteStream                 ← Handle backed by tokio AsyncWrite
```

This is structurally identical to a protocol stack:

```text
TLS:       MessageStream@TlsRecord → ByteStream (TCP Handle)
WebSocket: MessageStream@WsFrame   → ByteStream (Handle)
%emit:     MessageStream@Any       → Codec → ByteStream (%stdout)
```

`emit: [fn [let v] [send %emit v]]` is prelude sugar — it hides the `%emit` MessageStream name from user code. The output formatter (`-o stream`, `-o json`, etc.) implements the codec layer via `rebind`: it rewires `%emit` to a write-through pipeline before the user program runs, so every `[send %emit v]` call from then on goes through the codec without any change at the call site.

**Concrete example — `stdlib/cli/out/json.llt` before and after this design:**

```tinct
# BEFORE (pre-data-streaming contract):
# Returns a JSON String; the CLI owns materialization and printing.
[json: [include %libdir "codecs/json.llt"]]
[call $json.to-json %]
```

```tinct
# AFTER (lib-net-v3 MessageStream + Codec + rebind design):
#
# Declare the codec stack once as a function. Both %emit routing and the %
# return value use the same stack — no duplication of codec configuration.
#
# Codec stages compose with | — argument order is (config, source) so that
# | pipelines read left to right: source | [codec A] | [codec B] | [sink S]
#
# Stage 1 — JsonCodec:    MessageStream@Any    → MessageStream@String  (serialize)
# Stage 2 — NdjsonFramer: MessageStream@String → ByteStream             (frame + encode)

[json: [include %libdir "codecs/json.llt"]]

# Declare the codec stack once (source comes last — works with | pipelines)
[out: [_ | [codec JsonCodec] | [codec NdjsonFramer] | [sink %stdout]]]

# Wire %emit through it — [send %emit v] now writes JSON+\n to %stdout
[rebind %emit [out %emit]]

# Write % through the same stack. By the time % is forced, %emit is fully drained.
# to-messages converts Seq or scalar to a MessageStream; out writes each element.
[[to-messages %] | out]
```

The formatter declares its codec stack once and uses it for both purposes. Swapping JSON for CBOR, adding gzip, or routing to a file instead of `%stdout` all happen in one place. Every `-o FORMAT` formatter follows the same four-line pattern with different codec stages.

**Runtime replugging — the syslog example:**

`rebind` is not limited to formatter startup. Any `MessageStream` handle can be rebound at any point during execution:

```tinct
# Initial: syslog routes to UDP /dev/log
syslog: [message-stream udp-log-sink]

# Runtime rebind: reroute over HTTPS — senders [send syslog event] unchanged
[rebind syslog https-syslog-endpoint]
```

All callers that have already captured `syslog` and are calling `[send syslog event]` transparently switch to the new implementation. No callers need to be updated or restarted.

**Log file rollover** — a common logging pattern is rotating to a new file when the current one reaches a size limit or at a scheduled time. With `rebind`, the logging stream continues uninterrupted:

```tinct
# Logger task — runs forever, never knows about rotation
[log: [message-stream [file-codec [open log-cap "app.1.log" Writable]]]]

[task [let step [fn []
  [match [recv event-ch]
    [Closed]: []
    [Ok e]:   [[send log e] [step]]]]]
  [step]]

# Rotation task — triggered by size limit or schedule
[task [let step [fn []
  [match [recv rotation-trigger]
    [Closed]: []
    [Ok _]:
      [new-file: [open log-cap [next-log-filename] Writable]]
      [rebind log [file-codec new-file]]
      [step]]]]
  [step]]
```

The logger task calls `[send log e]` continuously. When the rotation task calls `rebind`, all subsequent sends go to the new file — no gap, no lost messages, no restart of the logger. The `rebind` is atomic from the logger's perspective: one send goes to the old file, the very next goes to the new one.

**`%emit` and HTTP client — the same primitive:**

Both are a single `send` to a pre-configured `MessageStream`. The codec is pre-wired and invisible at the call site:

```tinct
# HTTP client sending JSON to an HTTPS endpoint (codec pre-wired in http2-client)
[send http2-client [method: "POST"  path: "/data"  body: [json.to-json data]]]

# %emit writing JSON to %stdout (codec pre-wired via rebind in json.llt)
[send %emit data]  # identical form — both are MessageStream.send
```

The output formatter and the HTTP client are the same abstraction: a `MessageStream` handle whose codec layer is configured once (via `rebind` or at construction) and then used uniformly.

### `Codec`

`Codec` is the bridge between IO shapes. A compression layer maps `Bytes` to `Bytes`; a serialization layer maps structured values to `Bytes`; an H2 framing layer maps `H2Frame` to `Bytes`; a DTLS layer maps `Datagram` to `Datagram`. All of these are `Codec` instances. The typeclass does not mandate what types are consumed or produced — those are determined by each concrete instance:

```tinct
[class [Codec c input output]
  encode: [Fn [c@c input] output]
  decode: [Fn [c@c output] input]]
```

The `Codec` typeclass declaration lives in `stdlib/net.llt` alongside `ByteStream`, `Datagram`, and `MessageStream`.

**Concrete instances:**

```tinct
# ByteStream ↔ MessageStream (Any → Bytes)
# stdlib/codecs/json.llt
[instance [Codec NdjsonCodec Any Bytes]
  encode: [fn [let _ v] [bytes [str [json.to-json v] "\n"]]]
  decode: [fn [let _ b] [json.from-json [str-from-bytes b]]]]

# ByteStream ↔ ByteStream (Bytes → Bytes)
# stdlib/compress.llt
[instance [Codec GzipCodec Bytes Bytes]
  encode: builtin-gzip-compress
  decode: builtin-gzip-decompress]

# TLS record framing (Bytes → Bytes)
# stdlib/protocols/tls13.llt
[instance [Codec TlsRecordCodec Bytes Bytes]
  encode: tls-encrypt-record   # plaintext bytes → TLS record bytes
  decode: tls-decrypt-record]  # TLS record bytes → plaintext bytes

# MessageStream ↔ ByteStream (H2Frame → Bytes)
# stdlib/protocols/http2.llt
[instance [Codec HpackCodec Headers Bytes]
  encode: hpack-encode
  decode: hpack-decode]

[instance [Codec H2FrameCodec H2Frame Bytes]
  encode: h2-frame-serialize
  decode: h2-frame-parse]

# Datagram ↔ Datagram
# stdlib/protocols/quic.llt
[instance [Codec DtlsCodec Datagram Datagram]
  encode: dtls-protect
  decode: dtls-unprotect]
```

**Codec is the bridge between IO shapes** — the `input` and `output` type parameters tell you which shapes are connected:

| Instance | input | output | Bridge |
|---|---|---|---|
| `NdjsonCodec` | `Any` | `Bytes` | MessageStream → ByteStream |
| `GzipCodec` | `Bytes` | `Bytes` | ByteStream → ByteStream |
| `TlsRecordCodec` | `Bytes` | `Bytes` | ByteStream → ByteStream |
| `HpackCodec` | `Headers` | `Bytes` | MessageStream → ByteStream |
| `H2FrameCodec` | `H2Frame` | `Bytes` | MessageStream → ByteStream |
| `DtlsCodec` | `Datagram` | `Datagram` | Datagram → Datagram |

The existing protocol layers (`tls`, `h2`, `ws`, etc.) are `Codec` implementations — they just happen to compose with `ByteStream` instances via `encode`/`decode`. A `Codec` instance for compression sits between two `ByteStream` layers; one for NDJSON sits between a `MessageStream` and a `ByteStream` sink.

**Codec layers compose by type** — the `output` type of one codec is the `input` type of the next. This enables separating framing from serialization into distinct composable stages. The `%emit → JSON → %stdout` path for example can be decomposed into two explicit codec stages:

```tinct
# Stage 1 — serialization: Any → String (JSON text)
[instance [Codec JsonCodec Any String]
  encode: json.to-json
  decode: json.from-json]

# Stage 2 — framing: String → Bytes (NDJSON: add \n, encode as bytes)
[instance [Codec NdjsonFramer String Bytes]
  encode: [fn [let _ s] [bytes [str s "\n"]]]
  decode: [fn [let _ b] [str-from-bytes [str-trim-end b "\n"]]]]

# Pipe pipeline — stages compose with | and read left to right:
[rebind %emit
  [%emit | [codec JsonCodec] | [codec NdjsonFramer] | [sink %stdout]]]
```

The two-stage decomposition enables independent swapping: replace `JsonCodec` with `CborCodec` for binary encoding while keeping NDJSON framing, or replace `NdjsonFramer` with `LengthPrefixFramer` for length-prefixed framing while keeping JSON. `NdjsonCodec` (the combined `Any → Bytes` codec) is a convenience that fuses both stages for the common case.

Adding gzip is one more `| [codec ...]` stage:

```tinct
[rebind %emit
  [%emit
    | [codec JsonCodec]
    | [codec NdjsonFramer]
    | [codec GzipCodec]
    | [sink %stdout]]]
```

The type chain: `MessageStream@Any → MessageStream@String → ByteStream → ByteStream → %stdout`. Each stage is independently swappable and the pipeline reads the same as the data flow.

### Summary: the four IO shapes

| Typeclass | Direction | Framing | Example instances |
|---|---|---|---|
| `ByteStream` | `n` raw bytes | caller's job | `Handle` |
| `Datagram` | packet + address | per-packet | `UdpSocket` |
| `MessageStream T` | one typed `T` | protocol's job | `Channel T` |
| `Codec input output` | `input → output` | codec's job | `NdjsonCodec`, `GzipCodec`, `HpackCodec` |

### What Changes in the Implementation

- **New**: `ByteStream`, `Datagram`, `MessageStream`, and `Codec` class declarations in `stdlib/net.llt`
- **New**: `[instance [ByteStream Handle] ...]`, `[instance [Datagram UdpSocket] ...]`, and `[instance [MessageStream [Channel t] t] ...]` in `stdlib/net.llt`
- **New**: `MessageStreamHandle` — the repluggable handle type; holds a `ReactiveCell` (backed by `builtin-reactive-cell` / `tokio::sync::watch`) carrying the current routing implementation. `send`/`recv` on a handle transparently delegate to the current impl; `rebind` replaces it atomically. All `MessageStream` handles are `MessageStreamHandle` instances.
- **New**: `rebind` — universal operation that swaps the routing implementation on any `MessageStream` handle without the senders knowing. Defined in `stdlib/serve.llt`.
- **New**: `message-stream` — constructor that wraps an initial implementation in a `MessageStreamHandle`. Defined in `stdlib/serve.llt`.
- **New**: `codec-stream source codec sink → MessageStream` — wraps a source `MessageStream` with a `Codec` instance and a `ByteStream` sink. Returns a write-through `MessageStream` that calls `encode` on each value via the codec, then routes the output to the sink. The codec instance's `output` type determines which route is taken (write-bytes for ByteStream, send for MessageStream, send for Datagram). Defined in `stdlib/serve.llt`.
- **New**: `codec-sink source codec sink → Task` — drains a source `MessageStream`, applies codec to each value, writes to sink. Returns a Task (await it when done). `drain-emit codec sink` is sugar for `codec-sink %emit codec sink`. Defined in `stdlib/serve.llt`.
- **New**: `codec C source` — wraps source through a Codec stage; argument order (C, source) enables `|` pipelines. `sink S source` — terminal stage, drains source to ByteStream S. Both in `stdlib/serve.llt`.
- **New**: `to-messages v` — adapter from any tinct value to a `MessageStream`. Bridges arbitrary shapes into the MessageStream world: Seq → yields each element, scalar → yields one element. Follows the `to-json`/`to-tinct` naming convention. Enables `[[to-messages %] | out]` and, once `out` is made polymorphic, `[% | out]`. Defined in `stdlib/serve.llt`.
- **Removed**: `make-encrypted-handle` Rust primitive — superseded by tinct `ByteStream` instances
- **Updated**: `read-bytes`, `write-bytes` Rust primitives become `Handle`'s `ByteStream` instance methods; `udp-send`, `udp-recv` become `UdpSocket`'s `Datagram` instance methods. They remain in the primitives table as the Rust backing but are exposed through typeclass dispatch.
- **Updated**: All `*-accept` signatures use `constraint: [h: ByteStream]`; protocol layers are typed records; `WebSocketConnection` implements `MessageStream WsFrame` rather than `ByteStream`

---

## Server Layers

### `codec-stream`, `codec-sink`, and `drain-emit`

Three codec utilities in `stdlib/serve.llt` complete the `MessageStream` replugging model. All three take a `Codec` instance (not a raw `Fn` lambda) — the codec handles both serialization and framing. The caller does not need to know about `\n` separators, length prefixes, or any other framing detail; that is the codec's job.

- **`codec-stream source codec sink`** — wraps a source `MessageStream` with a `Codec` instance and a `ByteStream` sink. Each `send` to the returned `MessageStream` calls `encode` on the `Codec`, then dispatches the output to `sink` based on the codec's `output` type (`write-bytes` for `ByteStream`, `send` for `MessageStream`, `send` for `Datagram`). Used by output formatters to `rebind %emit` to a write-through pipeline.

```tinct
codec-stream: [fn@[bind: [c input output]]
    [let source@[MessageStream input]  codec@[Codec c input output]  sink]
  [task [loop-select [context]
    [[source [fn [let v] [route-encoded sink [encode codec v]]]]]
    identity]]]
```

Where `route-encoded` dispatches based on the encoded output type: `write-bytes` for `ByteStream`, `send` for `MessageStream`, `send` for `Datagram`. The concrete codec instance's type signature determines which branch is taken.

- **`codec-sink source codec sink`** — drains a source `MessageStream` in a background task, applies `encode` on the `Codec` for each value, and routes the output to `sink`. Returns a `Task`; await it when the drain must complete before the program exits.
- **`drain-emit codec sink`** — sugar for `codec-sink %emit codec sink`. The common case: drain `%emit` through a `Codec` instance into a `ByteStream` sink.

### `channel-map` and `channel-flat-map`

These are general concurrent channel operators defined in `stdlib/prelude.llt` (merged from the deleted `stdlib/async.llt` in the builtin-privacy-stdlib sprint) — not networking-specific. `channel-map` applies a function to each element concurrently (1:1); `channel-flat-map` applies a function that emits multiple outputs per element (1:N). The networking serve layers are just applications of these.

```tinct
# stdlib/prelude.llt (channel-map and channel-flat-map moved here from stdlib/async.llt)

# 1:1 — apply f to each element concurrently, collect results.
# buf: output channel buffer size; tune to match downstream consumption rate.
# (TLS handshake, WireGuard upgrade, JSON parsing, format conversion, ...)
channel-map: [fn@[bind: [element result]] [let f@[Fn [element] result]]
  [fn@[return: [Channel result]] [let in-ch@[Channel element]  buffer-size@[type: Int  default: 64]]
    [out: [channel buffer-size]]
    [task
      [let step [fn []
        [match [recv in-ch]
          [Closed]: []
          [Ok x]:   [[task [send out [f x]]] [step]]]]]
      [step]]
    out]]

# 1:N — apply f to each element; f emits multiple items to the shared out channel.
# buf: output buffer; size to absorb bursts when concurrent f calls complete together.
# (HTTP/1.1 keep-alive requests, HTTP/2 substreams, log lines from files, ...)
channel-flat-map: [fn@[bind: [element item]] [let f@[Fn [element [Channel item]] Null]]
  [fn@[return: [Channel item]] [let in-ch@[Channel element]  buffer-size@[type: Int  default: 256]]
    [out: [channel buffer-size]]
    [task
      [let step [fn []
        [match [recv in-ch]
          [Closed]: []
          [Ok x]:   [[task [f x out]] [step]]]]]
      [step]]
    out]]
```

Both loops are fire-and-forget tasks that terminate in two ways: (1) **natural termination** — the input channel closes (`[Closed]` from `recv`), the `step` function returns `[]`, and the task ends; (2) **external shutdown** — context cancellation via `cancel-task`/`exit`/`drain` raises a cancellation exception that propagates out of the task. `recv` returns `[Ok v]` | `[Closed]` (B-192); cancellation is a separate exception path. For infinite server accept loops (e.g., `[tcp-listen cap port]`), the channel never closes — shutdown happens via context cancellation only.

The inner tasks (`[task [send out [f x]]]`) are also fire-and-forget and are **not** cancelled when the outer loop exits — they run to completion or until they error. This is correct: aborting a half-completed TLS handshake would leave the client in a broken state. For clean server shutdown that waits for in-flight handshakes to complete, call `drain` from `stdlib/prelude.llt` at the top level before `exit`.

Concrete serve layers:

```tinct
# Config-bearing serve layers live alongside their protocol, not in serve.llt.
# Each protocol file defines its own *-serve using channel-map:

# stdlib/protocols/tls13.llt
tls-serve:       [fn [let cfg@TlsServerConfig in-ch]
                   [channel-map [fn [let h] [tls-accept cfg h]] in-ch]]

# stdlib/protocols/wireguard.llt
wireguard-serve: [fn [let cfg@WireguardConfig in-ch]
                   [channel-map [fn [let h] [wireguard-accept cfg h]] in-ch]]

# stdlib/protocols/noise.llt
noise-serve:     [fn [let cfg@NoiseConfig in-ch]
                   [channel-map [fn [let h] [noise-accept cfg h]] in-ch]]

# Config-free serve layers live alongside their protocol accept function:
# stdlib/protocols/http2.llt:
h2-serve:        [_ | [channel-map h2-accept]]
# stdlib/protocols/http3.llt:
h3-serve:        [_ | [channel-map h3-accept]]
# stdlib/protocols/websocket.llt:
ws-serve:        [_ | [channel-map ws-accept]]

# Message-extraction (alongside their protocol request parser):
# stdlib/protocols/http1.llt:
http1-serve:     [_ | [channel-flat-map http1-conn]]
# stdlib/protocols/http2.llt:
http2-requests:  [_ | [channel-flat-map http2-req-conn]]
# stdlib/protocols/http3.llt:
http3-requests:  [_ | [channel-flat-map http3-req-conn]]
```

The serve layers that need configuration (`tls-serve`, `wireguard-serve`, `noise-serve`) place config first and the input channel last, so they work as pipeline stages: `raw-ch | [tls-serve cert]`. They pass a 1-arg closure to `channel-map` — satisfying `channel-map`'s `f: [Fn [element] result]` type. Config-free layers (`h2-serve`, `ws-serve`, etc.) pass `*-accept` directly since those functions take only the connection handle.

Constraint propagation: the closure `[fn [let h] [tls-accept cfg h]]` has type `[Fn [t] TlsConnection constraint: [t: ByteStream]]`. When `channel-map` unifies this with `f: [Fn [element] result]`, the `ByteStream` constraint on `element` propagates to `in-ch: [Channel element]` — passing `Channel@QuicConnection` is caught at the call site as a compile-time type error.

WireGuard is a user-mode protocol: Noise_IKpsk2 handshake delegated to `noise.llt` (IKpsk2 + Blake2s + ChaChaPoly + X25519), with WireGuard-specific message framing (type bytes, sender/receiver index fields, mac1/mac2 fields) layered on top. Data plane over `udp-socket`; no kernel TUN/TAP.

`h2-serve` and `h3-serve` call tinct-implemented `h2-accept`/`h3-accept` from `stdlib/protocols/http2.llt` and `stdlib/protocols/http3.llt`. `Http2Connection` and `Http3Connection` are tinct records holding frame-parsing state, HPACK/QPACK tables, and stream channels — not opaque Rust types. They do **not** implement `ByteStream` — they are stream multiplexers, not byte pipes. Individual substreams are accessed via `h2-open-stream`/`h3-open-stream` which return `Handle` (a `ByteStream`). The extraction layers (`http2-requests`, `http3-requests`) pull request substreams from those records using tinct loops and channels.

A complete stack (HTTP over TLS over WireGuard over Unix socket):

```tinct
[unix-listen dir-cap "/var/run/app.sock"]
  | [wireguard-serve wg-config]
  | [tls-serve server-cert]
  | http1-serve
```

---

## Client Layers

Client-side layers are named after their protocol. `connect` is always the bottom; layers accumulate upward. Each layer either accumulates config (returning a handle type with open fields) or fires a handshake (returning a committed connection). The pattern is uniform across protocols.

```tinct
# Tcp path: TcpHandle → TlsHandle → Http2Connection
[connect %net Tcp [HostPort [Hostname "api.example.com"] 443]]
  | tls   # TlsHandle — sni inferred from addr, ALPN open
  | h2    # with-alpn ["h2"] + tls-commit + H2 preface → Http2Connection

# Udp path: ConnectedUdp → QuicHandle → Http3Connection
[connect %net Udp [HostPort [Hostname "api.example.com"] 443]]
  | quic  # QuicHandle — sni inferred from peer addr, ALPN open
  | h3    # with-alpn ["h3"] + quic-commit + H3 setup → Http3Connection

# WireGuard (Noise-based — wireguard fires immediately on ConnectedUdp)
[connect %net Udp [HostPort [Hostname "vpn.example.com"] 51820]]
  | [wireguard cfg]   # Noise IKpsk2 handshake → WireguardConnection

# DoT (DNS over TLS — tls-commit fires when dns-framed-send writes first byte)
[connect %net Tcp [HostPort [Hostname "dns.cloudflare.com"] 853]]
  | tls | [dns-framed-send query]
```

**Address resolution** happens inside `connect` for `[Hostname h]` via `resolve-address` in `dns.llt`. The `Transport Tcp` instance inlines Happy Eyeballs connection racing (250ms stagger, first success wins). The `Transport Udp` instance picks the first from the interleaved A+AAAA list. Both respect `%dns.single-request`.

For 1:N multiplexing — HTTP/2 and HTTP/3 — stream multiplexing is managed internally by the connection type. `http-request` returns `Task@RawResponse` immediately:

```tinct
# Connection reuse: two concurrent requests on one H2 connection
c:      [[connect %nc [Hostname "api.example.com"] Tcp 443] | tls | h2]
r1:     [http-request c "GET" "/api/users"]
r2:     [http-request c "GET" "/api/posts"]
[u p]:  [await-all r1 r2]

# Or via Http transport (SVCB-aware, picks H3 or H2 automatically)
c:      [connect %nc Http [HostPort [Hostname "api.example.com"] 443]]
resp:   [http-request c "GET" "/data"  accept: "application/json"]
```

`http-request` accepts trailing named arguments as HTTP headers (collected into `[Map String String]`). The connection (subject) is always the first argument; headers are keyword arguments after method and path.

---

## Bidirectional Connections

For protocols where either side can initiate — HTTP/2 server push, HTTP/3, WebSocket — the connection type is the same on both sides. `h2-serve` and `h2` both produce `Http2Connection`; `h3-serve` and `h3` both produce `Http3Connection`. These are tinct records, so stream access and channel adapters are tinct stdlib functions in `http2.llt`/`http3.llt`/`quic.llt`:

```tinct
# Stream access for byte-level tunneling (stdlib/protocols/http2.llt, http3.llt)
h2-open-stream:  [Fn [c@Http2Connection] Handle]     # opens a new H2 stream as a raw Handle
h3-open-stream:  [Fn [c@Http3Connection] Handle]     # opens a new H3/QUIC stream

# Adapters: one-stream-per-connection flattening (stdlib/protocols/)
quic-stream-ch:  [Fn [ch@[Channel QuicConnection]] [Channel Handle]]
h2-stream-ch:    [Fn [ch@[Channel Http2Connection]]   [Channel Handle]]
h3-stream-ch:    [Fn [ch@[Channel Http3Connection]]   [Channel Handle]]

# WireGuard tunnelled over H3 substreams — all tinct
wg-ch: [quic-listen cap 443]
         | h3-serve
         | h3-stream-ch
         | [wireguard-serve config]
```

`quic-stream-ch`, `h2-stream-ch`, and `h3-stream-ch` open exactly one stream per incoming connection. Use `h2-open-stream`/`h3-open-stream` directly when a single `Http2Connection`/`Http3Connection` needs multiple substreams.

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
[app-loop [[quic-listen cap 443] | h3-serve | http3-requests | my-codec]]
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
    [send sock addr [encode-dns-wire q]]
    [decode-dns-wire [recv sock].data]]]]

dns-tls-resolver: [fn [let cap addr@SocketAddress sni@String]
  [fn [let q] [task
    # tcp-connect gives a raw Handle; wrap in TcpHandle for tls, set sni explicitly
    [th: [TcpHandle [addr: [Ipv4 addr.addr]  port: addr.port  stream: [tcp-connect cap addr]]]]
    [conn: [tls-commit [TlsHandle [handle: th  sni: sni  alpn: []  ech: []  trust-roots: SystemRoots]]]]
    [dns-framed-send conn q]]]]

dns-https-resolver: [fn [let cap addr@SocketAddress sni@String path@String]
  [ep: [HostPort [Ipv4 addr.addr] addr.port]]
  [client: [h3-client [[connect cap Udp ep] | [quic-with-sni sni]]]]
  [fn [let q] [task [dns-https-send client q path]]]]

# Server — loop is transport-agnostic
dns-server-loop: [fn [let query-ch]
  [let step [fn []
    [match [recv query-ch]
      [Closed]: []
      [Ok q]:   [[task [q.respond [dns-lookup q.name q.type]]] [step]]]]]
  [step]]

# Plug in any transport
dns-server-loop [dns-udp-server net-cap 53]
dns-server-loop [dns-tls-server [[tcp-listen net-cap 853] | [tls-serve cert]]]
dns-server-loop [dns-https-server [[quic-listen net-cap 443] | h3-serve | http3-requests]]
```

---

## Worked Example: ICMP Ping Tunnel over H3

H3 used as byte transport — not HTTP. Each QUIC substream carries raw ICMP echo packets. The pipeline: `udp-listen | quic | h3 | substreams` yields a `Channel@QuicStream`, each stream a bidirectional byte channel (ICMP request/reply on both sides).

```tinct
[
  cap:            %net-cap
  port:           [@Port 4500]
  probe-interval: [seconds 5]

  # udp-listen → QuicLayer server instance → H3Layer server instance → substreams
  # Result: Channel@QuicStream, one stream per incoming H3 connection.
  stream-ch: [udp-listen cap port] | quic | h3 | substreams

  clients:   [channel 256]
  tick:      [timer-channel %clock probe-interval]

  serve-icmp-stream: [fn@Null [let stream@QuicStream]
    [let step [fn []
      [match [read-icmp stream]
        [Closed]: []
        [Ok pkt]:
          [match pkt
            [EchoRequest [id: id  seq: seq  data: data]]:
              [send-icmp stream [EchoReply id seq data]]
            [EchoReply [seq: seq]]:
              [log [str "RTT reply seq=" seq]]
            _: null]
          [step]]]]
    [step]]

  probe-all-clients: [fn@Null [let clients-ch scheduled]
    [lag:    [timestamp-diff [now %clock] scheduled]
     seq:    [random-id]
     probe:  [EchoRequest 0 seq "rtt-probe"]
     active: [collect-channel clients-ch]]
    [par-map [fn [let stream] [send-icmp stream probe] [send clients-ch stream]] active]
    [log [str "lag=" lag "ms  clients=" [length active]]]]
]
[loop-select [context]
  [[stream-ch [fn [let stream]    [send clients stream] [task [serve-icmp-stream stream]]]]
   [tick      [fn [let scheduled] [probe-all-clients clients scheduled]]]]
  identity]
```

**What this demonstrates:** `udp-listen | quic | h3 | substreams` as a fully composed server stack; H3 as byte transport (not HTTP); per-connection tasks; shared client registry via channel; timer-driven server-initiated probes via `par-map`; transport-agnostic ICMP logic; `loop-select` coordinating two event sources.

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
[let step [fn []
  [match [recv requests]
    [Closed]: []
    [Ok req]: [[task [req.respond [handler req]]] [step]]]]]
[step]
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
[let step [fn []
  [match [recv requests]
    [Closed]: []
    [Ok req]: [[task [req.respond [handler req]]] [step]]]]]
[step]
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
      # No SVCB — use connect Http which does Happy Eyeballs + h2/h1 ALPN negotiation
      [connect cap Http [HostPort [Hostname host] port]]]

# Race h3/QUIC and h2/TCP with 250ms stagger in ALPN preference order.
http-protocol-race: [fn [let cap@NetCap host@String port@Int
                          alpn@[Seq String] addrs@[Seq IpAddress] ech@Bytes]
  [result-ch:  [channel 1]
   h3?:        [not [empty? [filter [fn [let p] [= p "h3"]] alpn]]]
   h2?:        [not [empty? [filter [fn [let p] [= p "h2"]] alpn]]]]

  [h3-tasks: [if h3?
    [collect [map [fn [let e]
      [i: e.0  addr: e.1]
      [task
        [recv [timer-channel %clock [millis (* i 250)]]]
        [ep: [HostPort addr port]]
        [match [try [[connect cap Udp ep] | [quic-with-sni-ech host ech] | h3]]
          [Ok h]:  [try [send result-ch h]]
          [Err _]: null]]]
      [entries addrs]]]
    []]]

  [h2-tasks: [if h2?
    [collect [map [fn [let e]
      [i: e.0  addr: e.1]
      [task
        [recv [timer-channel %clock [millis (+ [if h3? 250 0] (* i 250))]]]
        [ep: [HostPort addr port]]
        [match [try [[connect cap Tcp ep] | [tls-with-sni-ech host ech] | h2]]
          [Ok h]:  [try [send result-ch h]]
          [Err _]: null]]]
      [entries addrs]]]
    []]]

  [result: [recv result-ch]]
  [par-map [fn [let t] [cancel-task t]] [append h3-tasks h2-tasks]]
  result]

# Helpers: create TlsHandle/QuicHandle with explicit sni (for SVCB-aware race where
# sni comes from the original hostname, not the resolved IP in TcpHandle.addr).
tls-with-sni-ech:  [fn [let sni@String ech@Bytes th@TcpHandle]
  [TlsHandle [handle: th  sni: sni  alpn: []  ech: ech  trust-roots: SystemRoots]]]

quic-with-sni-ech: [fn [let sni@String ech@Bytes qh@QuicHandle]
  [QuicHandle [pending: qh.pending  sni: sni  alpn: []  ech: ech  trust-roots: SystemRoots]]]
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
| [`async.llt`](lib-net-v3/async.llt) | `stdlib/prelude.llt` (merged — `stdlib/async.llt` deleted by builtin-privacy S-786) | `channel-map`, `channel-flat-map`, `collect-channel` |
| [`net.llt`](lib-net-v3/net.llt) | `stdlib/net.llt` | IO typeclasses (`ByteStream`, `Datagram`, `MessageStream`, `Codec`, `Listener`, `Indexed`); `Transport` typeclass; `Protocol` typeclass (`fetch`); `HttpConnect` typeclass (`http-request`); `HttpStream` typeclass (`send`/`recv` — bidirectional request/response); `Multiplexed` typeclass (`substreams` — Channel of connections → Channel of subsubstreams); `IpAddress`, `Address`, `Port`, `SocketAddress`, `HostPort`; `TcpHandle`, `FileHandle`, `ConnectedUdp`; `tcp-listen`, `udp-bind`, `udp-ephemeral`, `listen-loop`, `ip->string`, `Url`, `parse-url`, `url-decode`. Rust boundary: `Handle` + `UdpSocket`. |
| *(not yet written)* | `stdlib/codecs/json.llt` | `NdjsonCodec` (`Codec Any Bytes`) — NDJSON serialization with `\n` framing; used by `cli/out/json.llt` via `codec-stream` |
| [`dns.llt`](lib-net-v3/dns.llt) | `stdlib/dns.llt` | `DnsQuery`, `DnsRecord`, `DnsResponse`, `Nameserver`, `DnsConfig`; `encode-dns-wire`, `decode-dns-wire`; resolver factories (`dns-udp-resolver`, `dns-tls-resolver`, `dns-https-resolver`, `dns-quic-resolver`); `dns-framed-send`; `resolve-address` (Happy Eyeballs address resolution for `Address`); `Transport Tcp` + `Transport Udp` instances (the `connect` typeclass method — lives here because it needs `dns-resolve`); `dns-resolve-host`, `dns-resolve`, `dns-resolve` (alias), `dns-server-loop` |
| [`tls13.llt`](lib-net-v3/tls13.llt) | `stdlib/protocols/tls13.llt` | `TlsLayer` typeclass (`tls` — bidirectional: client `TcpHandle→TlsHandle`, server `TlsServerConfig→fn`); `TlsHandle`, `TlsServerConfig`, `TlsClientConfig`, `CipherSuite`, `TlsConnection`; `tls-commit`, `with-alpn`, `tls-handshake`; `hkdf-expand-label`, `derive-secret`, `tls13-key-schedule`; `tls-accept`, `tls-serve` |
| [`quic.llt`](lib-net-v3/quic.llt) | `stdlib/protocols/quic.llt` | `QuicLayer` typeclass (`quic` — bidirectional: client `ConnectedUdp→QuicHandle`, server `UdpSocket→Channel@QuicConnection`); `Multiplexed QuicConnection QuicStream` instance (`substreams`); `QuicFrame`, `QuicConnection`, `QuicKeys`, `QuicLossState`, `QuicHandle`; `quic-commit`, `with-alpn`; `quic-connect` (sugar); `quic-open-stream` |
| [`http2.llt`](lib-net-v3/http2.llt) | `stdlib/protocols/http2.llt` | `H2Layer` typeclass (`h2` — bidirectional: client `TlsHandle→Http2Connection`, server `Channel@TlsConnection→Channel@Http2Connection`); `HttpStream Http2Connection` instance (`send`/`recv`); `Multiplexed Http2Connection H2Stream` instance (`substreams`); `H2Frame`, `HpackTable`, `Http2Connection`; `hpack-decode`, `hpack-encode`; `h2-accept`; `http-request`; `HpackCodec`, `H2FrameCodec` |
| [`http3.llt`](lib-net-v3/http3.llt) | `stdlib/protocols/http3.llt` | `H3Layer` typeclass (`h3` — bidirectional: client `QuicHandle→Http3Connection`, server `Channel@QuicConnection→Channel@Http3Connection`); `HttpStream Http3Connection` instance (`send`/`recv`); `Multiplexed Http3Connection QuicStream` instance (`substreams`); `H3Frame`, `Http3Connection`; `h3-accept`; `http-request`; `qpack-decode`, `qpack-encode` |
| [`http1.llt`](lib-net-v3/http1.llt) | `stdlib/protocols/http1.llt` | `RawRequest`, `RawResponse`; `ok`, `json-ok`, `redirect`, `not-found`, `server-error`; `parse-request`, `write-response`, `http1-conn`, `http1-request` |
| [`http.llt`](lib-net-v3/http.llt) | `stdlib/protocols/http.llt` | `Http` transport type + `Transport Http` instance (SVCB-first, selects H3/H2/H1 — the `Http` in `[connect %nc Http [HostPort ...]]`); `HttpConnection` (wraps `Http2Connection` or `Http3Connection`); `fetch` (Level 1: `[fetch %nc url]`); `[instance [Protocol Http Url RawResponse] ...]`; `SvcbRecord`; `http-channel`, `http-channel-tls`, `resolve-svcb`; `router`, `headers-map`; `compression`, `logging`, `timeout`, `cors`, `auth` |
| [`websocket.llt`](lib-net-v3/websocket.llt) | `stdlib/protocols/websocket.llt` | `WsFrame`, `WebSocketConnection`; `ws-accept`, `ws` (client handshake), `ws-recv-frame`, `ws-send-frame`; `ws-serve` |
| [`wireguard.llt`](lib-net-v3/wireguard.llt) | `stdlib/protocols/wireguard.llt` | `WireguardLayer` typeclass (`wireguard` — bidirectional: `[wireguard cfg]` on single handle or channel); `WireguardConfig`, `WireguardConnection`; `wireguard-accept` (TODO — WireGuard framing differs from Noise framing); `wg-read-bytes`, `wg-write-bytes`; framing helpers. Data plane: ChaCha20-Poly1305 with little-endian nonce counter. |
| [`noise.llt`](lib-net-v3/noise.llt) | `stdlib/protocols/noise.llt` | `NoiseLayer` typeclass (`noise` — bidirectional: `[noise cfg]` on single handle or channel); `NoisePattern`, `NoiseDh`, `NoiseCipher`, `NoiseConfig`, `NoiseState`, `NoiseConnection`; `noise-accept`, `noise-read`, `noise-write`; patterns XX, IK, IKpsk2, NK |
| [`crypto.llt`](lib-net-v3/crypto.llt) | `stdlib/crypto.llt` | `HkdfHash` union; `Sha256`, `Sha384`, `Sha512`, `Blake2s` (hash algorithm selectors for HKDF — used by TLS, WireGuard, Noise, etc.); `AsnValue` union; `parse-cert`, `cert-san`, `cert-public-key`, `verify-cert-chain` |
| [`text.llt`](lib-net-v3/text.llt) | `stdlib/text.llt` | `TextCodec` typeclass + instances (UTF-8, UTF-16, Windows-1252, ~38 WHATWG encodings); `TextEncoding` runtime record (renamed from `Codec` to avoid collision with `Codec` typeclass); `TextBytes`; `to-encoding`, `encoding-for-name`, `text-encode`, `text-decode`, `decode-http-body` |
| [`compress.llt`](lib-net-v3/compress.llt) | `stdlib/compress.llt` | `CompressionCodec` typeclass (superclass `[Codec c Bytes Bytes]`, with `codec-name`) + instances (`GzipCodec`, `DeflateCodec`, `BrotliCodec`, `ZstdCodec`, `IdentityCodec`); `CompressedCodec` runtime record; `to-compressed-codec`; `codec-for-encoding`, `negotiate-encoding` (both accept optional `registry@[Map String CompressedCodec]` for user codecs) |
| [`serve.llt`](lib-net-v3/serve.llt) | `stdlib/serve.llt` | Generic codec pipeline only — no protocol imports. `MessageStreamHandle` type + `MessageStream` instance (delegates to `ReactiveCell` impl); `message-stream`, `rebind`; `codec`, `sink`, `to-messages`, `drain-emit`. Protocol-specific serve layers (`h2-serve`, `h3-serve`, etc.) live alongside their `*-accept` functions in their respective protocol files. |
| *(not yet written)* | `stdlib/protocols/icmp.llt` | `EchoRequest`, `EchoReply`; `read-icmp`, `send-icmp` |
| *(not yet written)* | `stdlib/protocols/socks5.llt` | SOCKS5 proxy protocol |
| *(not yet written)* | `stdlib/protocols/grpc.llt` | gRPC framing over `http2.llt` |
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

All primitives in the New Rust Primitives table above, registered in the appropriate module via `builtin_module()` (builtin-privacy architecture): transport and crypto primitives in `builtin_module("net")` (`src/builtins_net.rs`). Stdlib files that use them declare `--- uses: ["net"]` in their document header.

### Add stdlib modules

`stdlib/net.llt`, `stdlib/text.llt`, `stdlib/compress.llt`, `stdlib/crypto.llt`, `stdlib/serve.llt`, `stdlib/dns.llt` (async utilities now in `stdlib/prelude.llt` — `stdlib/async.llt` deleted); `stdlib/protocols/` (tls, quic, h2, h3, http1, http, compress, wireguard, noise, websocket, icmp, socks5, grpc, mqtt).

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
       [append [list absolute] searched]]]]]

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
- `h3` — HTTP/3 framing moves to `stdlib/protocols/http3.llt`
- `rustls` — TLS moves to `stdlib/protocols/tls13.llt`

**Keep**: `tokio` (rt-multi-thread, net, time, signal, sync), `cap-std`, `notify`

(`tokio-util`, `num_cpus` are added by runtime-v2, not here.)

---

## Prerequisites

- [`runtime-v2.md`](runtime-v2.md) complete — `task`, `await`, `channel`, `recv`, `send`, `select-once`, `loop-select`, `context`, `with-timeout`, `finally`, `Arc`-based async runtime, and async utilities (`loop-select`, `retry`, `finally`, etc.) all present in `stdlib/prelude.llt` (`stdlib/async.llt` was merged into prelude in the builtin-privacy-stdlib sprint). (`tcp-connect` is the only Rust primitive here; `tls`, `quic`, `h2`, `h3`, and `connect` are tinct stdlib functions defined in this whatif.)

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
