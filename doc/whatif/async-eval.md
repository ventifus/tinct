# What If: Async and Parallel Evaluator for tinct

**State:** Superseded — 2026-05-19

- Core async runtime (eval async, Rc→Arc, OnceLock thunk, task/await/channel/select, par, event sources, context/cancellation) → [`runtime-v2.md`](runtime-v2.md)
- Serve/connect layer design, networking primitives, DNS and ICMP worked examples, stdlib networking modules → [`lib-net-v3.md`](lib-net-v3.md)

What would it take to make tinct's evaluator fully non-blocking and parallel — enabling cooperative concurrency, multi-core evaluation, first-class async primitives, and event-driven programs in a single implementation pass?

## Current State

tinct's evaluator is synchronous. Every builtin that performs I/O calls `block_on` from `src/async_rt.rs` — a thread-local Tokio `current_thread` runtime — to drive async Rust code to completion before returning. The thread blocks for the full duration of the I/O operation.

```tinct
# Today: each http-get blocks its thread until the response arrives.
# No other tinct evaluation can proceed on this thread during the wait.
[
  a: [fetch cap "https://api1.example.com/data"]
  b: [fetch cap "https://api2.example.com/data"]
  combined: [merge a b]
]
# a and b are fetched sequentially even though they're independent
```

There is no way for a tinct program to express "do these things concurrently" or "wait for whichever event arrives first." The only concurrency mechanism is OS-level (running multiple tinct processes).

### What's Missing

1. No way to run independent tinct computations concurrently within one program.
2. No way for I/O operations to yield while waiting, allowing other work to proceed.
3. No `task`/`await` primitives for programmer-controlled concurrency.
4. No `channel`/`select` for event-driven programs (signal handling, timers, HTTP listeners).
5. No composable event source model — signals, timers, and HTTP requests are all ad-hoc.

## Why an Async Evaluator Matters for tinct

tinct's lazy evaluation is already a deferred-computation model: a `Thunk` is work that hasn't run yet. Making the evaluator async extends this from "deferred" to "concurrent" — independent thunks can be in flight simultaneously, yielding to each other at I/O boundaries.

Concretely:

- **Parallel I/O.** Two `fetch` calls with no data dependency between them run concurrently. `[await-all [task [fetch ...]] [task [fetch ...]]]` completes in `max(t1, t2)` not `t1 + t2`.
- **Event-driven programs.** A tinct program can listen for HTTP requests, OS signals, and timer ticks simultaneously, dispatching to the appropriate handler via `select`. Web servers, daemons, and reactive pipelines become expressible.
- **Non-blocking file I/O.** Large file reads don't freeze other evaluations.
- **Foundation for parallel evaluation.** Once `eval` is `async fn`, multi-thread Tokio can run independent thunks on separate cores with only the representation change described in `par-dist-eval.md`.

## Design

### The Rust/tinct Boundary

tinct is a language, not a Rust application. Rust owns the irreducible OS interface; everything above it is tinct. A function belongs in Rust only if it meets one of:

1. **Binary protocol or encoding** — TLS crypto, QUIC, HTTP/3 QPACK, binary frame formats.
2. **Security-critical** — cryptographic primitives, TLS certificate verification.
3. **Performance cliff** — `from-json`/`emit` (JSON parsed on every eval startup), core arithmetic.
4. **OS system call** — file I/O, socket bind, signal delivery, clock, process control.
5. **Bootstrap** — the parser, evaluator, and type checker themselves.

Everything else belongs in tinct. The test: *could this be written clearly in tinct in under 200 lines?* If yes, it is stdlib.

This principle has one concrete consequence for dependencies: `hyper` and `reqwest` are removed. HTTP/1.1 is a text protocol handled by `stdlib/http1.llt`. HTTP/3 framing (QPACK) is the one protocol layer that stays in Rust via the `h3` crate (Huffman tables and binary encoding are not interesting logic to express in tinct). The full Rust dep change: add `h3`, `tokio-util`; remove `hyper`, `reqwest`.

### The Execution Model

This proposal has two inseparable layers implemented in a single pass:

**Layer 1 — Async:** `eval` and `materialize` become `async fn`. Every blocking I/O operation yields to the scheduler instead of blocking the thread. The Tokio runtime interleaves independent evaluations cooperatively.

**Layer 2 — Parallel:** `Rc<T>` → `Arc<T>` throughout; `RefCell<T>` → `RwLock<T>` or `Mutex<T>`; `ThunkState` replaced by an `OnceLock`-based pair. The multi-thread Tokio runtime with work-stealing distributes independent thunks across all available cores automatically. Automatic parallel dict evaluation, `par` hint, `par-map`.

These are one refactor, not two. Every file is touched for the `async fn` contagion anyway; the `Rc`→`Arc` migration is mechanical on top of that. Separating them would require the same files to be opened twice.

Distributed evaluation (cluster, `remote-task`, content-addressed cache) is a separate proposal: `dist-eval.md`.

### The `Rc` → `Arc` Migration

The complete representation change that unlocks multi-thread execution:

| Before | After | Notes |
|--------|-------|-------|
| `Rc<Thunk>` | `Arc<Thunk>` | Thunks safely cross thread boundaries |
| `Rc<RefCell<Environment>>` | `Arc<RwLock<Environment>>` | Write-rarely, read-often |
| `Rc<RefCell<ThunkState>>` | Replaced by `OnceLock` pair — see below | |
| `Rc<EvalConfig>` | `Arc<EvalConfig>` | Already immutable; trivial |
| `Rc<RefCell<EvalState>>` | `Arc<Mutex<EvalState>>` | Include cache; infrequent access |
| `Rc<RefCell<TaskState>>` | `Arc<Mutex<TaskState>>` | Task join-handle polling |
| `Rc<ChannelInner>` | `Arc<ChannelInner>` | `tokio::sync::mpsc` already `Send` |

`Arc` clone costs ~10–50ns vs ~1ns for `Rc`. Thunk evaluation costs microseconds to milliseconds. The overhead is negligible.

### The `OnceLock` Thunk

The current `ThunkState` enum (`Unevaluated`, `InProgress`, `PendingBuiltin`, `PendingCall`, `Materialized`, `Failed`) encodes a mutable state machine inside a `RefCell`. With multiple threads and async yield points, `InProgress` is no longer a safe sentinel — a thread could yield while holding `InProgress`, and another thread or task demanding the same thunk would see it and spuriously raise a cycle error.

The `OnceLock` model replaces this with a write-once primitive:

```rust
pub struct Thunk {
    // Taken by the task that wins the evaluation race; None afterwards
    unevaluated: Mutex<Option<UnevaluatedState>>,
    // Set exactly once by the winning task; all waiters unblock automatically
    result: tokio::sync::OnceCell<Result<Value, Arc<EvalError>>>,
    pub span: Span,
}

enum UnevaluatedState {
    Expr    { expr: Spanned<Expr>, env: Arc<Environment>, ctx: Arc<EvalContext> },
    Builtin { func: BuiltinFn, args: Vec<Arc<Thunk>>, named: IndexMap<String, Arc<Thunk>>, depth: usize, call_span: Span, ctx: Arc<EvalContext> },
    Call    { func: Arc<Thunk>, args: Vec<Arc<Thunk>>, call_span: Span, ctx: Arc<EvalContext> },
}
```

**Forcing protocol:**

```
materialize(thunk):
  1. result.get() → Some(v): return v.clone()      [lock-free after init; the hot path]
  2. lock unevaluated mutex; take Option
     - Some(state): this task won. Release lock. Evaluate → value_or_err.
       result.set(value_or_err).ok()               [waiters unblock automatically]
     - None: another task is evaluating. Release lock.
       result.get_or_init(|| unreachable!()).await  [suspends until winner sets the cell]
  3. return result.get().unwrap().clone()
```

Every thunk evaluates exactly once regardless of how many tasks demand it simultaneously. The hot path (already materialized) is fully lock-free.

**Cycle detection:** each async task maintains a task-local `HashSet<*const Thunk>` of thunks currently on its own evaluation stack. Demanding a thunk already in this set is a cycle (same task). Seeing `None` in `unevaluated` while the result isn't set yet means another task is evaluating — wait via `OnceCell`. `EvalError` is now `Arc<EvalError>` (was `Box`) so it clones cheaply across threads.

The `Awaiting(Rc<Notify>)` variant described in earlier drafts of this proposal is superseded by `OnceLock`, which provides the same wait-until-available semantics more cleanly and is thread-safe.

### When `task` Starts

In tinct's lazy model, `[task expr]` passes `expr` as a thunk to the `task` builtin. `spawn_local` fires **when the `task` expression itself is materialized** — not when `await` demands the handle. An undemanded task thunk is never spawned; the work never starts.

```tinct
# task is spawned when 'worker' binding is first demanded — not before
worker: [task [fetch cap "https://api.example.com/"]]

# Spawning happens here, when await demands 'worker'
result: [await worker]

# This task is NEVER spawned — t is never demanded
[
  t: [task [expensive]]
  answer: [+ 1 2]       # only answer is demanded; t is ignored
]
```

This is consistent with tinct's lazy semantics and means `task` is safe to place in dict entries without surprising side effects. For fire-and-forget work where the result is not needed, use `[seq [await [task expr]] next]` to force evaluation.

### Async Primitives

```tinct
# Spawn a concurrent computation — returns immediately with a Task handle
worker: [task [fetch cap "https://api.example.com/data"]]

# Await a single task — suspends caller until worker completes
result: [await worker]

# Await all tasks in parallel — completes when all finish
[a: a-result  b: b-result]: [await-all
  [task [fetch cap "https://api1.example.com/"]]
  [task [fetch cap "https://api2.example.com/"]]]

# Channels — typed queues for communication
ch: [channel 32]       # bounded buffer of 32 values
[send ch "hello"]      # put a value in — suspends if buffer full
msg: [recv ch]         # take a value — suspends until one is available

# Select — wait for whichever channel fires first, dispatch to handler
[select
  [sig-ch  [fn [let _]   [exit 0]]]
  [req-ch  [fn [let req] [task [handle-request req]]]]
  [tick-ch [fn [let _]   [log "heartbeat"]]]]
```

### Automatic Parallel Dict Evaluation

With `Arc` throughout and `tokio::spawn` available on the multi-thread runtime, independent dict entries evaluate in parallel automatically — no programmer annotation required:

```tinct
# a, b, c have no data dependency on each other.
# They evaluate on separate cores simultaneously.
[
  a:      [fetch cap "https://api1.example.com/"]
  b:      [fetch cap "https://api2.example.com/"]
  c:      [db-query db "SELECT * FROM users"]
  result: [merge a [merge b c]]   # waits for all three via OnceLock
]
```

`eval_dict` submits each entry's thunk to the Tokio thread pool via `tokio::spawn`. Data dependencies enforce ordering naturally: `result` demands `a`, `b`, and `c` via `materialize`, which each block on their respective `OnceLock` until complete. No explicit synchronization code.

### `par` and `par-map`

For sequences — where elements are evaluated lazily on demand — `par` is an explicit opt-in to eager parallel evaluation:

```tinct
# Sequential: map forces one element at a time
[map [fn [let x] [expensive x]] big-list]

# Parallel: all elements submitted to the thread pool simultaneously
[par-map [fn [let x] [expensive x]] big-list]

# par as a primitive: start evaluating now, return value when demanded
a: [par [expensive-computation input]]
```

`par expr` calls `tokio::spawn(materialize(thunk))` immediately and returns the same thunk. When the thunk is demanded later, its `OnceLock` result is already set (or nearly so). For cheap computations `par` adds overhead; for expensive ones it converts sequential evaluation into parallel.

`par-map` and `par-filter` are stdlib wrappers that submit all elements simultaneously and collect results in order.

### Event Sources

Event sources are Rust builtins that return a `Channel` written to by a background task. Tinct programs consume them identically regardless of source.

```tinct
# OS signal handling — element type is Signal (string name of the signal)
sig: [signal-channel SIGTERM SIGINT]

# Periodic timer — interval is a Duration, delivers scheduled Timestamp
tick: [timer-channel %clock [seconds 5]]

# Incoming HTTP requests — from stdlib/http.llt, not a Rust builtin
reqs: [http-channel cap 8080]

# File system watch — fires when path changes
changes: [watch-channel dir-cap "/etc/config"]

# Event loop: handle all sources until exit
[loop [fn [let]
  [select
    [sig     [fn [let name] [log [str "signal: " name]] [exit 0]]]
    [reqs    [fn [let req]  [task [handle req]]]]
    [tick    [fn [let _]    [log "tick"]]]
    [changes [fn [let _]    [reload-config]]]]]]
```

**Rust event source builtins** (`signal-channel`, `timer-channel`, `watch-channel`) follow the same pattern: spawn a background task that writes to a channel; the channel is the user-visible value; `ChannelInner` holds an `AbortHandle` and calls `abort()` in `Drop`, so cleanup is automatic when all references to the channel are dropped.

**`signal-channel`** carries `Signal` values (string names: `"SIGTERM"`, `"SIGINT"`, etc.) so handlers know which signal fired. Multiple signals on one channel are multiplexed internally via `tokio::select!`.

**`http-channel` is not a Rust builtin.** It is `stdlib/http.llt`, built on two thin Rust network primitives:

```
tcp-listen:  NetCap → Int → Channel@Handle     # one Handle per accepted TCP connection
quic-listen: NetCap → Int → Channel@QuicConn   # one QuicConn per accepted QUIC connection
```

`tcp-listen` (~20 lines of Rust): `TcpListener::bind`, loop on `accept()`, wrap each stream in `Value::Handle` (bidirectional), send to channel. HTTP/1.1 framing and all protocol logic lives in `stdlib/http1.llt` — pure tinct on top of `Handle`.

`quic-listen` (~20 lines of Rust): quinn `Endpoint::server()`, loop on `accept()`, wrap as `Value::QuicConn`, send to channel. HTTP/3 framing uses the `h3` crate via a thin Rust builtin; everything above that frame layer is tinct.

`http-channel` in stdlib unifies both into a single `Channel@Request`, auto-negotiating HTTP/1.1 (TCP) and HTTP/3 (QUIC) on the same port.

### Serve and Connect Layers

Transport primitives produce a single connection or a channel of connections. Protocol layers transform them. The same compositional model applies to both server (receive) and client (initiate).

#### The Symmetry

```
                  Server (receive)                Client (initiate)
                  ──────────────────────          ──────────────────────
Transport     resource → Channel@A            resource → A
1:1 layer     Channel@A → Config → Channel@B  A → Config → B   (already: *-layer)
1:N layer     Channel@A → Channel@B           A → RequestClient@B
```

A "layer violation" — a protocol that skips, merges, or reorders traditional stack layers — is expressed through its input and output types. QUIC produces `QuicConn`, not `Handle`. HTTP/3 expects `QuicConn`. They compose. `tls-serve` expects `Channel@Handle`; it does not compose with `Channel@QuicConn` (QUIC already has TLS). No special cases: the type system is the rule.

#### Server: `make-serve-layer` and `make-multiplex-serve`

Two generic factories in `stdlib/serve.llt` cover all server-side protocol layers:

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
        [case [let conn: Ok]  [task [conn-fn conn out]]]   # each conn gets its own task
        [case [let _: Err]    null]]]]]
    out]]
```

Concrete serve layers are instances. There are two kinds: **connection-promotion** (1:1, upgrade the connection type) and **message-extraction** (1:N, pull individual protocol messages out of a connection):

```tinct
# Connection-promotion: one connection in, one upgraded connection out
tls-serve:       [make-serve-layer tls-accept]        # Handle   → TlsHandle
wireguard-serve: [make-serve-layer wireguard-accept]  # Handle   → WgHandle
noise-serve:     [make-serve-layer noise-accept]      # Handle   → NoiseHandle
h2-serve:        [make-serve-layer h2-accept]         # Handle   → H2Conn
h3-serve:        [make-serve-layer h3-accept]         # QuicConn → H3Conn
ws-serve:        [make-serve-layer ws-accept]         # Handle   → WsConn (WsConn IS a Handle)

# Message-extraction: one connection in, many protocol messages out
http1-serve:     [make-multiplex-serve http1-conn]    # Handle → RawRequest*
http2-requests:  [make-multiplex-serve http2-req-conn]# H2Conn → RawRequest*
http3-requests:  [make-multiplex-serve http3-req-conn]# H3Conn → RawRequest*
```

`ws-serve` is connection-promotion even though WebSocket is bidirectional — `WsConn` satisfies the `Handle` byte-stream interface, so `wireguard-serve` or any other `*-serve` can be stacked directly on top of it without an adapter.

`h2-serve` and `h3-serve` produce `H2Conn`/`H3Conn` (not `Channel@RawRequest`) because HTTP/2 and HTTP/3 are bidirectional — server push means both sides can initiate. The `http2-requests`/`http3-requests` extraction layers sit on top when you want the request-response view.

A complete stack for the contrived example (HTTP over TLS over WireGuard over Unix socket):

```tinct
raw:  [unix-listen dir-cap "/var/run/app.sock"]
wg:   [wireguard-serve raw wg-config]
tls:  [tls-serve wg server-cert]
reqs: [http1-serve tls]
```

WireGuard as an entry-point transport (rather than a layer):

```tinct
tunnel: [wireguard-bind net-cap wg-config]   # WgTunnel — virtual IP network
conns:  [tcp-listen-on tunnel 443]            # Channel@Handle — TCP over the tunnel
tls:    [tls-serve conns server-cert]
reqs:   [http1-serve tls]
```

#### Client: `*-layer` and Request Clients

The client-side 1:1 layers (`*-layer`) are already compositional and already exist. They are exactly the dual of `make-serve-layer` applied to a single connection rather than a channel:

```tinct
raw:    [tcp-connect cap "host" 443]
tls:    [tls-layer raw tls-config]          # Handle → TlsHandle
wg:     [wireguard-layer tls wg-config]     # TlsHandle → WgHandle  (unusual but typesafe)
inner:  [tls-layer wg inner-tls-config]
resp:   [http1-request inner [method: "GET" path: "/"]]
```

For 1:N multiplexing on the client — HTTP/2 and HTTP/3 — the dual is a **request client**: an opaque handle backed by an internal task that manages stream multiplexing. `http-request` sends a request and immediately returns `Task@RawResponse`, never blocking:

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
r1:     [http-request client [method: "GET" path: "/api/users"]]
```

`http2-client` and `http3-client` are Rust builtins — stream-ID correlation and flow control require tight integration with the h2/h3 protocol implementations. There is no `make-request-client` stdlib factory analogous to `make-multiplex-serve` for the same reason there is no `make-serve-layer` for the 1:N case: the protocol-specific framing cannot be abstracted without knowing the protocol.

WireGuard as a client-initiated transport:

```tinct
tunnel: [wireguard-dial cap peer-config]     # WgTunnel — client-initiated VPN
conn:   [tcp-connect-on tunnel "host" 80]    # Handle — TCP through the tunnel
tls:    [tls-layer conn tls-config]
resp:   [http1-request tls ...]
```

#### Bidirectional Connections: Symmetric Types

For protocols where either side can initiate — HTTP/2 server push, HTTP/3, WebSocket, gRPC streaming — the connection type is the same on both sides. `h2-serve` and `h2-connect` both produce `H2Conn`; `ws-serve` and `ws-connect` both produce `WsConn`. The difference is only in how the connection was established.

`H2Conn` and `H3Conn` expose stream-level access for byte-level tunneling:

```
h2-open-stream:    H2Conn   → Handle    # raw H2 stream as byte pipe
h3-open-stream:    H3Conn   → Handle    # raw H3/QUIC stream as byte pipe
quic-open-stream:  QuicConn → Handle    # already exists
```

For server-side stacking from a multiplexed connection back to `Channel@Handle`:

```tinct
# Adapter functions: each connection → one stream per connection
quic-stream-ch:  Channel@QuicConn → Channel@Handle
h2-stream-ch:    Channel@H2Conn   → Channel@Handle
h3-stream-ch:    Channel@H3Conn   → Channel@Handle
```

These are stdlib functions (instances of `make-multiplex-serve`) that open one stream per incoming connection. They bridge back to `Channel@Handle`, restoring composability with all `*-serve` layers:

```tinct
# WireGuard over H3 streams
h3-ch:  [h3-serve [quic-listen cap 443]]   # Channel@H3Conn
s-ch:   [h3-stream-ch h3-ch]               # Channel@Handle — one stream per conn
wg-ch:  [wireguard-serve s-ch config]      # WireGuard on top

# Server push: H2Conn is the same type on both sides
conns: [h2-serve tls-ch]                   # Channel@H2Conn
[loop [fn [let]
  [conn: [recv conns]]
  [task [handle-h2-conn conn]]]]

handle-h2-conn: [fn [let conn]
  [reqs: [h2-incoming conn]]              # Channel@H2Message — incoming requests
  [loop [fn [let]
    [req: [recv reqs]]
    [h2-push conn push-promise pushed-response]    # server initiates push (result discarded)
    [h2-respond conn req response]]]]              # then responds
```

#### Transport-Agnostic Application Protocols

The serve/connect layer handles *transport* — how bytes and connections move. Application protocols (DNS, REST APIs, gRPC, MQTT, custom wire formats) are *codecs* — pure marshal/unmarshal between transport messages and domain objects. These two concerns are orthogonal and should not be mixed.

The pattern that makes them work together is the **`respond` closure**: each message in a `Channel@AppMessage` carries a `respond` function that closes over the transport-specific response mechanism. The application calls `[q.respond answer]` without knowing whether the transport is UDP, TLS, HTTP/2, or QUIC.

**DNS as the worked example:**

The DNS client and server are transport-agnostic. On the client side, a **resolver** is any function `DnsQuery → Task@DnsResponse`:

```tinct
# stdlib/dns.llt

dns-resolve: [fn [let resolver name type]
  [await [resolver [name: name type: type id: [random-id]]]]]

# All identical from dns-resolve's perspective:
[dns-resolve udp-resolver  "example.com" A]
[dns-resolve doh-resolver  "example.com" A]
[dns-resolve dot-resolver  "example.com" A]
[dns-resolve doq-resolver  "example.com" A]

# Resolver factories — each returns a function DnsQuery → Task@DnsResponse
dns-udp-resolver: [fn [let cap host port]
  [fn [let q] [task [decode-dns-wire [udp-request cap host port [encode-dns-wire q]]]]]]

dns-tls-resolver: [fn [let cap host port]
  [fn [let q] [task [dns-tls-send cap host port q]]]]

dns-tls-send: [fn [let cap host port q]
  [conn: [tls-layer [tcp-connect cap host port] tls-config]]
  [write-handle conn [encode-dns-length-prefixed q]]
  [decode-dns-wire [read-dns-length-prefixed conn]]]

dns-https-resolver: [fn [let cap url]
  [client: [http-client cap url]]
  [fn [let q] [task [dns-https-send client q]]]]

dns-https-send: [fn [let client q]
  [resp: [await [http-request client
                  [method:  "POST"
                   path:    "/dns-query"
                   headers: [["content-type" "application/dns-message"]]
                   body:    [encode-dns-wire q]]]]]
  [decode-dns-wire resp.body]]

dns-quic-resolver: [fn [let cap host port]
  [fn [let q] [task [dns-quic-send cap host port q]]]]

dns-quic-send: [fn [let cap host port q]
  [stream: [quic-open-stream [quic-connect cap host port]]]
  [write-handle stream [encode-dns-length-prefixed q]]
  [decode-dns-wire [read-dns-length-prefixed stream]]]
```

On the server side, a **`Channel@DnsQuery`** is the transport-agnostic interface. Transport adapters produce this channel; the DNS server loop consumes it identically regardless of transport:

```tinct
dns-server-loop: [fn [let query-ch]
  [loop [fn [let]
    [q: [recv query-ch]]
    [task [q.respond [dns-lookup q.name q.type]]]]]]

# Plug in any transport — the loop never changes
dns-server-loop [dns-udp-server net-cap 53]
dns-server-loop [dns-tls-server [tls-serve [tcp-listen net-cap 853] cert]]
dns-server-loop [dns-https-server [http3-requests [h3-serve [quic-listen net-cap 443]]]]
dns-server-loop [dns-quic-server  [quic-listen net-cap 853]]
```

Each adapter wraps its transport's response mechanism in the `respond` closure:

```tinct
# DoH adapter: Channel@RawRequest → Channel@DnsQuery
# The respond closure encodes the DNS answer as an HTTP response
dns-https-server: [fn [let http-ch]
  [filter-map [fn [let req]
    [if [= [get-header req "content-type"] "application/dns-message"]
      [Ok [name:    [decode-dns-name req.body]
           type:    [decode-dns-type req.body]
           respond: [fn [let answer]       # wraps req.respond
                      [req.respond [status:  200
                                    headers: [["content-type" "application/dns-message"]]
                                    body:    [encode-dns-wire answer]]]]]]
      null]]
  http-ch]]
```

**The general pattern for any custom protocol:**

```tinct
# 1. Define the domain message type with a respond closure
AppMessage: { ...fields...  respond: Fn@Null [AppResponse] }

# 2. Write a codec adapter: Channel@TransportMsg → Channel@AppMessage
my-server: [fn [let transport-ch]
  [map [fn [let msg]
    [...fields decoded from msg...
     respond: [fn [let reply] [msg.respond [encode reply]]]]]
  transport-ch]]

# 3. Write resolver factories for the client: resource → Fn@[Task AppResponse] [AppQuery]
my-client: [fn [let cap endpoint]
  [fn [let query] [task [...connect, encode, send, receive, decode...]]]]

# 4. Application uses either without knowing the transport
[app-loop [my-server [http3-requests [h3-serve [quic-listen cap 443]]]]]
[app-result [my-client cap "host:443"] query]
```

DoH, REST APIs, gRPC, MQTT, and any other application-layer protocol are instances of this pattern. They are codecs, not serve layers.

#### New Rust Primitives for Serve/Connect

| Primitive | Signature | Description |
|-----------|-----------|-------------|
| `unix-listen` | `DirCap → Str → Channel@Handle` | Incoming Unix socket connections. |
| `tcp-listen-on` | `WgTunnel → Int → Channel@Handle` | TCP over a WireGuard virtual network. |
| `wireguard-bind` | `NetCap → WgConfig → WgTunnel` | Start a WireGuard server; returns virtual network. |
| `wireguard-dial` | `NetCap → WgConfig → WgTunnel` | Connect as a WireGuard peer; returns virtual network. |
| `tcp-connect-on` | `WgTunnel → Str → Int → Handle` | TCP through a WireGuard tunnel. |
| `tls-accept` | `Handle → TlsServerConfig → TlsHandle` | Server-side TLS handshake. |
| `wireguard-accept` | `Handle → WgServerConfig → WgHandle` | Server-side WireGuard handshake. |
| `noise-accept` | `Handle → NoiseConfig → NoiseHandle` | Server-side Noise Protocol handshake. |
| `h2-accept` | `Handle → H2ServerConfig → H2Conn` | Server-side HTTP/2 handshake. Used by `h2-serve`. |
| `h3-accept` | `QuicConn → H3ServerConfig → H3Conn` | Server-side HTTP/3 handshake. Used by `h3-serve`. |
| `ws-accept` | `Handle → WsServerConfig → WsConn` | Server-side WebSocket handshake. `WsConn` satisfies `Handle`. |
| `h2-connect` | `TlsHandle → H2ClientConfig → H2Conn` | Client-side HTTP/2. Same `H2Conn` type as server. |
| `h3-connect` | `QuicConn → H3ClientConfig → H3Conn` | Client-side HTTP/3. Same `H3Conn` type as server. |
| `ws-connect` | `Handle → WsClientConfig → WsConn` | Client-side WebSocket. Same `WsConn` type as server. |
| `h2-open-stream` | `H2Conn → Handle` | Raw H2 stream as a byte pipe. Enables byte-level tunneling over H2. |
| `h3-open-stream` | `H3Conn → Handle` | Raw H3/QUIC stream as a byte pipe. |
| `http-request` | `H2Conn\|H3Conn → RawRequest → Task@RawResponse` | Send HTTP request; returns immediately. Correlated by stream ID internally. |
| `h2-push` | `H2Conn → PushPromise → RawResponse → Null` | HTTP/2 server push. Protocol-enforced: clients receive an error. |
| `h2-incoming` | `H2Conn → Channel@H2Message` | Incoming requests (server) or server pushes (client). |
| `ws-send` | `WsConn → WsFrame → Null` | Send a WebSocket frame, either side. |
| `ws-incoming` | `WsConn → Channel@WsFrame` | Incoming frames, either side. |

### The `select` Form

`select` takes a list of `[channel handler]` pairs. It suspends until any channel has a value, then calls that channel's handler with the received value. `select` is itself a tail call in the loop body — the pattern above is an explicit recursive loop, but `[loop-select sources]` in stdlib wraps it:

```tinct
# stdlib/async.llt
loop-select: [fn [let sources]
  [select-once sources]        # handle one event (result discarded)
  [loop-select sources]]       # tail-recursive loop
```

`select-once` is the primitive that suspends until one channel fires; `select` in user code is the `loop-select` wrapper.

**Fairness:** `select-once` polls channels in pseudo-random order (matching Tokio `select!` and Go `select` semantics). No channel is permanently starved when multiple are ready simultaneously.

**Closed channels:** when a channel's sender is dropped (all `Channel` references gone, the background task aborted), `select-once` treats it as a permanently-unavailable source and removes it from consideration. If all sources in the list are closed, `select-once` returns an error. `loop-select` in stdlib propagates this as a normal program exit.

### Cancellation and Contexts

Every blocking operation in an async tinct program needs a bound: a way to say "if this takes too long, or if the program is shutting down, stop waiting." Go solved this with `context.Context` — a value threaded through every function call that carries a cancellation signal. tinct adopts the same model.

A `Context` is a first-class tinct value backed by `tokio_util::sync::CancellationToken`. Contexts form a hierarchy: cancelling a parent cancels all its children. The runtime creates a **root context** for every program run. All tasks, channels, and blocking operations inherit from this root by default.

```tinct
# Get the current evaluation's context
ctx: [context]

# Derived contexts
[child-ctx: child   cancel: cancel-fn]: [with-cancel ctx]   # cancel-fn cancels child-ctx
timed-ctx: [with-timeout ctx 5000]    # auto-cancels after 5000ms
dead-ctx:  [with-deadline ctx ts]     # auto-cancels at absolute Timestamp

# Test if a context is done (for cooperative cancellation in loops)
[if [cancelled? ctx] [cleanup] [continue]]

# Cancel explicitly
[cancel-fn]
```

All blocking builtins (`await`, `recv`, `send`, `select-once`) respect the current EvalContext's cancellation token. If the context is cancelled while a builtin is blocked, the builtin returns immediately with `Err("cancelled")`. Internally this is a `tokio::select!`:

```rust
tokio::select! {
    result = the_operation.await  => Ok(result),
    _      = ctx.cancelled().await => Err(EvalError::Cancelled("context cancelled")),
}
```

#### `timeout` — bounded await

`timeout` wraps a task with a deadline. It is a Rust builtin (not a stdlib wrapper) using `tokio::time::timeout` directly:

```tinct
# Returns [Ok result] on success, [Err "timeout"] if the duration elapses
result: [timeout [seconds 5] [task [slow-fetch cap url]]]

# With context propagation: the task's context is a child of the caller's
result: [timeout-with ctx [seconds 5] [task [fetch-with-ctx ctx url]]]
```

`timeout` on a task that is already done returns immediately. The losing task is `abort()`ed.

#### Shutdown primitives

Three Rust builtins compose the shutdown story:

```
cancel-root:  → Null      # cancel the root CancellationToken — signals all tasks to stop
drain:        → Null      # await until all in-flight spawned tasks have finished
exit-now:     Int → Null  # process::exit(code) immediately; no drain, no cleanup
```

`cancel-root` is not capability-gated. The security model is OS-level process isolation — `tinct run` is the security boundary, and untrusted code never runs in the same process as trusted code. Any tinct code that can call `cancel-root` is already trusted.

`drain` waits on the runtime's `JoinSet`, which includes all tasks spawned via `task` and all `cluster-local` workers (Tokio tasks on the same runtime). Remote workers (`connect-cluster`) are external processes and are not included in `drain`.

`stdlib/async.llt` composes these into the common shutdown patterns:

```tinct
# stdlib/async.llt

# Graceful: cancel all tasks, wait indefinitely for them to finish, then exit.
exit: [fn [let code]
  [cancel-root]
  [drain]
  [exit-now code]]

# Graceful with an explicit drain window — caller decides how long to wait.
graceful-exit: [fn [let drain-timeout@Duration code@Int]
  [cancel-root]
  [timeout drain-timeout [task [drain]]]    # wait up to drain-timeout; result discarded
  [exit-now code]]
```

There is no built-in timeout on `exit`. If tasks do not finish, `exit` waits indefinitely — it is the programmer's responsibility to bound this explicitly:

```tinct
# 10-second drain window, then force-exit
[on-signal SIGTERM [fn [let _] [graceful-exit [seconds 10] 0]]]

# Wait as long as it takes
[on-signal SIGTERM [fn [let _] [exit 0]]]

# Immediate — no cleanup at all
[on-signal SIGKILL [fn [let _] [exit-now 0]]]
```

Tasks that want to perform cleanup when cancelled use `[finally cleanup body]` or check `[cancelled? [context]]` at yield points.

#### `finally` — cleanup on any exit

`[finally cleanup body]` is a stdlib macro in `stdlib/async.llt`. It runs `cleanup` regardless of whether `body` succeeds, errors, or is cancelled. Cleanup executes in a non-cancellable context so it cannot be interrupted by the same cancellation that stopped `body`.

```tinct
# Close a connection even if work is cancelled
[finally
  [close-conn conn]
  [do-work-with conn]]

# Cleanup can await — it runs in a fresh non-cancellable context
[finally
  [do [flush-logs log-handle] [notify-shutdown service]]
  [serve-requests reqs]]
```

Expansion (in `stdlib/async.llt`):

```tinct
finally: [fn [let cleanup body]
  [result: body]                                          # evaluate body; bind result
  [with-context [non-cancellable] [fn [let] cleanup]]     # run cleanup in fresh context
  result]                                                 # return body's result
```

`non-cancellable` creates a fresh root `CancellationToken` that nothing will ever cancel. `with-context` runs `cleanup` under that token, so any `await`, `recv`, or `send` inside cleanup proceeds normally even if the surrounding context was cancelled.

**What `finally` does not cover:** Rust-level `EvalError` exceptions (type errors, cycle detection) propagate through Rust's call stack and bypass `finally`. This is inherent to the architecture. For ordinary tinct errors — `Result`-style `Err` values and cancellation — `finally` is reliable.

**Timeout in cleanup:** if cleanup can hang, bound it explicitly. There is no default:

```tinct
[finally
  [timeout 5000 [task [flush-logs log-handle]]]
  [serve-requests reqs]]
```

#### REPL and Ctrl-C

The REPL runs each user expression under a **session context** — a child of the root context, created fresh per expression. The root context is **not** cancelled by a single Ctrl-C; background tasks from previous expressions continue running.

Ctrl-C behaviour:

- **During an eval:** cancels the session context. Any `await`, `recv`, or `send` blocked inside unblocks with `Err("cancelled")`; the REPL prints the error and returns to the prompt.
- **At the prompt (no active eval):** prints `^C` and a hint. No action taken. Matches Python's interactive mode.
- **Second Ctrl-C within ~2 seconds** (whether during eval or at prompt): calls `[exit 0]` — graceful drain then exit. The 2-second window is tracked in the REPL's Rust input loop, not in tinct.
- **Ctrl-D:** calls `[exit 0]`.

```
> [await [task [sleep 10000]]]
^C
Error: cancelled
> _                          ← prompt; background tasks still running
> ^C
(Ctrl-D or [exit 0] to quit)
> ^C                         ← second within 2s: graceful exit
```

#### `EvalContext` additions

```rust
pub struct EvalContext {
    pub config: Arc<EvalConfig>,
    pub state:  Arc<Mutex<EvalState>>,
    pub cancel: tokio_util::sync::CancellationToken,  // NEW — current scope's token
}

impl EvalContext {
    pub fn with_cancel(&self) -> (EvalContext, CancellationToken) {
        let child = self.cancel.child_token();
        (EvalContext { cancel: child.clone(), ..self.clone() }, child)
    }
    pub fn with_timeout(&self, ms: u64) -> EvalContext {
        let child = self.cancel.child_token();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            child.cancel();
        });
        EvalContext { cancel: child, ..self.clone() }
    }
}
```

#### Context inheritance and task lifetimes

A task inherits its creator's `CancellationToken` by default. If the parent context is cancelled or times out, the task is cancelled too — even if `await` has not been called on the task handle yet.

```tinct
timed: [with-timeout [context] 5000]

# This task WILL be cancelled when the 5s timer fires.
# It shares timed's token; cancellation propagates immediately.
worker: [with-context timed [task [long-running-thing]]]

# This task will NOT be cancelled by timed's timer.
# It has its own independent token.
[child: independent  cancel: _]: [with-cancel [context]]
worker: [with-context independent [task [long-running-thing]]]
```

This is the same model as Go's `context.Context`: parent cancels → subtasks cancel, unless the subtask is given an independent context. The default is the safe choice — a budget timeout propagates to all work within that budget. Opt-out is explicit.

`with-cancel` creates an explicit child for tasks that should be cancellable independently of their creator.

### Type System

`Task` and `Channel` are parameterized types:

```tinct
task-a@[Task Int]:  [task [+ 1 2]]
ch@[Channel Str]:   [channel 10]
result@Int:         [await task-a]
msg@Str:            [recv ch]

# await-all: homogeneous — [Seq [Task T]] → Seq@T, results in submission order
results@[Seq Int]:  [await-all [task [+ 1 2]] [task [* 3 4]]]

# For heterogeneous concurrent work, use separate awaits or a sum type:
[
  n: [task [+ 1 2]]
  s: [task [str "hello"]]
  both: [[await n] [await s]]   # evaluated sequentially; use task for concurrency
]
```

`await-all` is intentionally homogeneous: `[Seq [Task T]] → [Seq T]`. This matches F# `Async<'T>` and Haskell `Async a`. For mixed types, separate `await` calls or wrapping results in a nominal sum type are the correct pattern.

`select-once` typing: each `[channel handler]` pair must have a handler whose argument type matches the channel's element type. The return type of `select-once` is the union of all handler return types.

**`channel` capacity:** `[channel n]` requires `n ≥ 1`. `[channel 0]` is a runtime error. There is no unbounded channel primitive; callers that want very large buffers use a large capacity. There is no rendezvous (synchronous handoff) channel — `[channel 1]` is the closest approximation.

### Implementation: Making `eval` Async

The core change is making `eval` and `materialize` `async fn`. Every caller must also become async or use `block_on` at the program boundary (CLI entry point, LSP analysis).

**New `BuiltinFn` type:**

```rust
// Before
pub type BuiltinFn =
    fn(&[Rc<Thunk>], &IndexMap<String, Rc<Thunk>>, usize, Span) -> Result<Value, Box<EvalError>>;

// After — future may borrow args for lifetime 'a
pub type BuiltinFn = for<'a> fn(
    &'a [Rc<Thunk>],
    &'a IndexMap<String, Rc<Thunk>>,
    usize,
    Span,
    Rc<EvalContext>,
) -> Pin<Box<dyn Future<Output = Result<Value, Box<EvalError>>> + 'a>>;
```

The `'a` lifetime allows builtins to borrow from `args` within their future. This is correct for all builtins that complete within a single `materialize` call. A convenience macro hides the boilerplate:

```rust
fn builtin_map(args, named, depth, span, ctx) -> BuiltinFuture<'_> {
    Box::pin(async move {
        let func = materialize(&args[0], span, &ctx, depth).await?;
        let seq  = materialize(&args[1], span, &ctx, depth).await?;
        // ...
    })
}
```

**The `task` builtin is a special case.** `spawn_local` requires a `'static` future — it cannot borrow from the caller's stack. The `task` builtin clones `Rc<Thunk>` and `Rc<EvalContext>` into an owned `async move {}` block:

```rust
fn builtin_task(args, named, depth, span, ctx) -> BuiltinFuture<'_> {
    // Clone into 'static-capable owned values before spawning
    let thunk = Rc::clone(&args[0]);
    let ctx   = Rc::clone(&ctx);
    Box::pin(async move {
        let handle = tokio::task::spawn_local(async move {
            materialize(&thunk, span, &ctx, depth).await
        });
        Ok(Value::Task(Rc::new(RefCell::new(TaskState::Pending(handle)))))
    })
}
```

**`async_rt.rs` becomes the task scheduler:**

The thread-local `block_on` bridge is replaced by a `LocalSet` that hosts the entire evaluation of a tinct program. `task` spawns onto this `LocalSet` via `tokio::task::spawn_local`. The CLI and LSP entry points construct the `LocalSet` and run the top-level evaluation on it.

**`src/async_rt.rs` — new role:**

```rust
// Entry point: multi-thread runtime, work-stealing across all cores
pub fn run_program<F>(fut: F) -> F::Output
where F: Future + Send, F::Output: Send
{
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num_cpus::get())
        .enable_all()
        .build()
        .unwrap()
        .block_on(fut)
}

// task builtin: Arc-based thunks are Send; use tokio::spawn (not spawn_local)
pub fn spawn_task<F>(fut: F) -> TaskHandle
where F: Future<Output = Result<Value, Arc<EvalError>>> + Send + 'static
{
    let join = tokio::task::spawn(fut);
    TaskHandle(Arc::new(Mutex::new(TaskState::Pending(join))))
}
```

### New Value Variants

```rust
pub enum Value {
    // ... existing variants ...
    Task(Rc<RefCell<TaskState>>),
    Channel(Rc<ChannelInner>),
    Context(tokio_util::sync::CancellationToken),  // Clone is cheap (Arc internally)

enum TaskState {
    Pending(tokio::task::JoinHandle<Result<Value, Box<EvalError>>>),
    Done(Value),
}

struct ChannelInner {
    tx: tokio::sync::mpsc::Sender<Value>,
    rx: RefCell<tokio::sync::mpsc::Receiver<Value>>,
    capacity: usize,
    // Held for event-source channels; abort() called on Drop
    background_task: Option<tokio::task::AbortHandle>,
}

impl Drop for ChannelInner {
    fn drop(&mut self) {
        if let Some(handle) = &self.background_task {
            handle.abort();
        }
    }
}
```

`RefCell<Receiver>` is needed because `recv()` requires `&mut self`. Since all channel access is on one thread (LocalSet), `RefCell` is safe. For user-created channels (`[channel n]`), `background_task` is `None`; for event sources, it holds the `AbortHandle` so cleanup is automatic.

### Event Source Builtins

Event sources create channels and internally `spawn_local` a background task that writes to the channel:

```rust
// signal-channel: spawn a tokio::signal::unix::signal listener
fn builtin_signal_channel(signals: &[Value]) -> Value {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    tokio::task::spawn_local(async move {
        let mut stream = tokio::signal::unix::signal(kind)?;
        loop { stream.recv().await; tx.send(Value::Null).await.ok(); }
    });
    Value::Channel(Rc::new(ChannelInner::new(tx, rx)))
}

// timer-channel: spawn a tokio::time::interval task
// watch-channel: spawn a notify::RecommendedWatcher task
// http-channel is stdlib/http.llt — not a Rust builtin
```

All event sources follow the same pattern: the channel is the user-visible value; the spawned task is invisible infrastructure.

### The `block_on` Bridge (Compatibility)

Builtins that previously called `block_on` internally (HTTP, QUIC) replace `block_on(fut)` with `fut.await`. The thread-local runtime in `async_rt.rs` is removed; the `LocalSet` runtime drives everything from the top level.

Existing integration points that can't yet be made async (LSP synchronous callbacks, test harness) retain a thin `block_on` wrapper at the outermost call site only.

## Stdlib Module Map

The complete `stdlib/` layout resulting from this proposal and the Rust/tinct boundary principle.

```
stdlib/
  prelude.llt       — map, filter, reduce, result combinators; trimmed core
  strings.llt       — trim, pad-left/right, starts-with?, ends-with?, str-contains?, str-replace,
                      str-split-lines, words, unwords  (moved from builtins)
  seq.llt           — zip-with, enumerate, chunk, partition, group-by, sort-by, uniq-by,
                      flat-map, scan, window, interleave
  path.llt          — path-join, path-dirname, path-basename, path-ext, path-normalize
  result.llt        — and-then, map-ok, map-err, unwrap-or, unwrap, ok?, err?, collect-results
  cap.llt           — narrow, readable?, writable?, with-temp, distributable?

  net.llt           — Port: [type Int@[is: [between 1 65535]  repr: u16]]
                      parse-url, url-encode/decode, form-encode/decode, resolve-host,
                      higher-level connection utilities  (expanded from existing)
  http1.llt         — HTTP/1.1 framing in pure tinct on top of Handle:
                        parse-request, write-response, serve-conn, serve-keepalive (server)
                        build-request, send-request, parse-response              (client)
  http3.llt         — thin wrapper around h3-request Rust builtin; same RawRequest/RawResponse
                        shape as http1.llt
  serve.llt         — make-serve-layer (connection-promotion), make-multiplex-serve (message-extraction)
                      Connection-promotion: tls-serve, wireguard-serve, noise-serve,
                        h2-serve, h3-serve, ws-serve
                      Message-extraction: http1-serve, http2-requests, http3-requests
                      Stream adapters: quic-stream-ch, h2-stream-ch, h3-stream-ch
  http.llt          — http-channel (unified server: TCP+QUIC, same port)
                      fetch, fetch-h1, fetch-h3 (client)
                      router, headers-map, parse-query
                      ok, json-ok, redirect, not-found, server-error
                      with-logging, with-cors, with-auth, with-timeout (middleware)

  async.llt         — exit, graceful-exit, finally
                      loop-select, retry, retry-with
                      throttle, debounce, merge-channels, broadcast, pipeline

  datetime.llt      — Timestamp, Duration, formatting/parsing  (existing proposal)
  regex.llt         — Thompson NFA regex engine                 (existing proposal)
  toml.llt          — complete TOML 1.0 parser                  (expand toml-lite)
  sql.llt           — lazy SQL data sources                     (existing proposal)
  dist.llt          — dist-map, dist-reduce, dist-filter        (from dist-eval.md)
```

**HTTP/1.1 request lifecycle** — where the Rust/tinct boundary sits:

```
OS TCP accept  → Rust: tcp-listen → Value::Handle
               → tinct: stdlib/http1.llt parse-request (text parsing)
               → tinct: stdlib/serve.llt pump attaches respond fn (oneshot::Sender)
               → tinct: user handler (router, headers-map, json-ok — all tinct)
               → tinct: stdlib/http1.llt write-response serializes to wire format
               → Rust: write-handle sends bytes through Handle.write_inner
OS TCP send
```

## Worked Examples

### ICMP Ping Tunnel over H3 — Bidirectional RTT Measurement

This example combines several design elements in one program:

- **H3 as a byte-level transport** — ICMP echo packets are tunneled through raw H3 streams (not HTTP requests), using `h3-stream-ch` to get `Channel@Handle` from `Channel@H3Conn`
- **Bidirectionality** — the server both handles incoming pings *from* clients and initiates outgoing pings *to* each connected client to measure RTT in both directions
- **Per-connection concurrency** — each H3 connection gets its own task; a shared channel tracks all live connections

```tinct
# icmp-h3-server.llt
#
# An H3 server that:
#   1. Accepts H3 connections and tunnels ICMP echo over raw H3 streams
#   2. Responds to echo requests from clients (standard ping server)
#   3. Periodically sends echo requests TO each client and measures RTT

# One dict: channels, helpers, and all bindings together.
# Letrec semantics mean everything can reference everything else freely.
[
  cap:            %net-cap

  # Port is a stdlib/net.llt type alias — shown here for clarity
  Port:           [type Int@[is: [between 1 65535]  repr: u16]]

  # Configuration
  port:           [@Port 4500]
  probe-interval: [seconds 5]
  client-backlog:          256            # unhandled connection backlog

  # Transport stack: QUIC → H3 connections → raw H3 streams → byte-level ICMP
  quic-ch:   [quic-listen cap port]
  h3-ch:     [h3-serve quic-ch]               # Channel@H3Conn — connection promotion
  stream-ch: [h3-stream-ch h3-ch]             # Channel@Handle — one stream per H3 conn

  # Registry of active client streams for server-initiated pings
  clients:   [channel client-backlog]

  # Timer: server-initiated RTT probes on configured interval
  tick:      [timer-channel %clock probe-interval]

  # Respond to incoming echo requests; handle incoming echo replies (from our own probes)
  # read-icmp decodes the wire format into IcmpMessage nominal variants,
  # defined in stdlib/icmp.llt:
  #   [union IcmpMessage
  #     [EchoRequest [id: Int  seq: Int  data: Str]]
  #     [EchoReply   [id: Int  seq: Int  data: Str]]
  #     [Unreachable [code: Int]]
  #     [TimeExceeded]]
  serve-icmp-stream: [fn@Null [let stream@Handle]
    [loop [fn [let]
      [pkt: [read-icmp stream]]
      [match pkt
        [case [let req: EchoRequest]  [send-icmp stream [EchoReply req.id req.seq req.data]]]
        [case [let rep: EchoReply]    [log [str "RTT reply seq=" rep.seq]]]
        [case [let _]                 null]]]]]

  # Send echo requests to all registered clients and log completion
  probe-all-clients: [fn@Null [let clients-ch@[Channel Handle]  scheduled@Timestamp]
    [lag:    [timestamp-diff [now %clock] scheduled]
     seq:    [random-id]
     probe:  [EchoRequest 0 seq "rtt-probe"]
     active: [collect-channel clients-ch]]
    [par-map [fn [let stream]
      [send-icmp stream probe]
      [send clients-ch stream]]             # put stream back in registry
     active]
    [log [str "lag=" lag "ms  clients=" [length active]]]]
]

# The event loop is the emitted value — demanding it drives evaluation of everything above.
[loop [fn [let]
  [select
    [stream-ch [fn [let stream]
                 [send clients stream]
                 [task [serve-icmp-stream stream]]]]
    [tick      [fn [let scheduled]
                 [probe-all-clients clients scheduled]]]]]]
```

`collect-channel`, `send-icmp`, `read-icmp`, `echo-request`, `echo-reply` are stdlib or user functions operating on `Handle` values — plain byte-stream reads and writes of ICMP wire format. The serve/connect infrastructure is invisible to them: a `Handle` from an H3 stream is identical to a `Handle` from a plain TCP connection.

**What this demonstrates:**

| Concept | Where used |
|---|---|
| H3 as byte transport (not HTTP) | `h3-stream-ch` converts `Channel@H3Conn` → `Channel@Handle` |
| Bidirectional H3Conn | Server both receives from clients and sends to them on same streams |
| Per-connection tasks | Each stream gets `[task [serve-icmp-stream stream]]` |
| Shared mutable registry | `clients` channel acts as a concurrent queue of active streams |
| Timer-driven server-initiated work | `timer-channel %clock [seconds 5]` delivers scheduled `Timestamp`; lag = `timestamp-diff [now %clock] scheduled` |
| Parallel probe fan-out | `par-map` sends to all clients concurrently |
| Transport-agnostic ICMP logic | `serve-icmp-stream` only sees `Handle` — works over any byte transport |

A client connecting to this server would use the symmetric stack:

```tinct
# icmp-h3-client.llt
[
  cap:    %net-cap
  conn:   [quic-connect cap "server.example.com" 4500]
  h3:     [h3-connect conn h3-config]
  stream: [h3-open-stream h3]          # raw H3 stream — same type as the server sees
  t0:     [now %clock]
]
[send-icmp stream [EchoRequest 0 1 "hello"]]
[match [read-icmp stream]
  [case [let rep: EchoReply]  [log [str "RTT: " [timestamp-diff [now %clock] t0]]]]
  [case [let _]               null]]
```

## What Would Change

### Value and Thunk (`src/value.rs`)

**Current:** `Rc<Thunk>`, `Rc<RefCell<Environment>>`, `Rc<RefCell<ThunkState>>` throughout. `ThunkState` is a 7-variant enum inside a `RefCell`.

**Proposed:** All `Rc` → `Arc`; all `RefCell` → `RwLock` or `Mutex`. `ThunkState` enum replaced by `(Mutex<Option<UnevaluatedState>>, tokio::sync::OnceCell<Result<Value, Arc<EvalError>>>)` pair. `EvalError` wrapped in `Arc` (was `Box`) for cheap cross-thread clone. New `Value::Task`, `Value::Channel`, `Value::Context` variants.

**Impact:** Fundamental — every file in `src/` is touched. The `Rc`→`Arc` change is mechanical (search-replace plus fixing resulting type errors). The `ThunkState`→`OnceLock` change restructures `eval_materialize.rs`. The type system enforces every missed case at compile time.

### Evaluator (`src/eval.rs`, `src/eval_materialize.rs`)

**Current:** `eval`, `materialize`, and all helpers are synchronous `fn`. Single-task forcing via `ThunkState`.

**Proposed:** All become `async fn`. Every recursive call site gains `.await`. `eval_dict` fans out independent entries via `tokio::task::JoinSet` — automatic parallel evaluation. `materialize` uses the `OnceLock` forcing protocol. A task-local `HashSet<*const Thunk>` detects true cycles (same task re-demanding a thunk it holds); contention (another task evaluating) waits via `OnceCell`. `EvalContext` gains `cancel: CancellationToken` field.

**Impact:** Major — pervasive `async fn` contagion (~600 lines) plus `eval_dict` rewrite for `JoinSet` fanout and `materialize` rewrite for `OnceLock` protocol.

### Builtins (`src/builtins.rs`, `src/builtins_io.rs`)

**Current:** `BuiltinFn` is a plain function pointer; `block_on` bridges I/O builtins.

**Proposed:** `BuiltinFn` becomes an async function pointer type returning `Pin<Box<dyn Future>>`. All ~180 builtins gain the wrapper. I/O builtins replace `block_on(fut)` with `fut.await`. A `#[tinct_builtin]` proc-macro hides the `Box::pin(async move { ... })` wrapper for the common case.

**Impact:** Major — all builtins are touched. The change is uniform and mechanical; each builtin gains 2–3 lines of async boilerplate.

### New Builtins

| Builtin | Signature | Description |
|---------|-----------|-------------|
| `context` | `→ Context` | Returns the current evaluation's cancellation context. |
| `with-cancel` | `Context → [Context Fn]` | Child context + a zero-arg cancel function. Cancelling child does not cancel parent. |
| `with-timeout` | `Context → Duration → Context` | Child context that auto-cancels after the given duration. |
| `with-deadline` | `Context → Timestamp → Context` | Child context that auto-cancels at an absolute time. |
| `cancelled?` | `Context → Bool` | True if the context has been cancelled. |
| `non-cancellable` | `→ Context` | Returns a fresh `Context` backed by a root `CancellationToken` that nothing will ever cancel. Used to run cleanup that must not be interrupted. |
| `with-context` | `Context → [Fn@T []] → T` | Evaluates the zero-arg function in a derived `EvalContext` where the cancel token is replaced by the given `Context`. Required for `[finally ...]`. |
| `timeout` | `Duration → Task@T → Result@T` | Awaits the task; returns `[Ok result]` or `[Err "timeout"]` if the duration elapses. Aborts the task on timeout. |
| `cancel-root` | `→ Null` | Cancel the root `CancellationToken`. Signals all tasks to stop. Not capability-gated (process isolation is the security boundary). |
| `drain` | `→ Null` | Await until all in-flight tasks (including `cluster-local` workers) have finished. Does not include remote workers. |
| `exit-now` | `Int → Null` | Immediate `process::exit`. No drain, no cleanup. |
| `task` | `expr → Task@T` | Spawns evaluation of `expr` via `spawn_local` when the task expression is materialized. Clones `Rc<Thunk>` + `Rc<EvalContext>` for `'static` bound. |
| `await` | `Task@T → T` | Suspends until task completes; propagates its error. |
| `await-all` | `[Seq Task@T] → [Seq T]` | Awaits all tasks; results in submission order. Homogeneous: all tasks must share type `T`. |
| `await-any` | `[Seq Task@T] → T` | Returns first completed result; calls `abort()` on all remaining tasks. Aborted tasks stop at their next yield point; side effects are not rolled back. |
| `channel` | `Int → Channel@T` | Creates a bounded channel. Capacity must be ≥ 1; `[channel 0]` is an error. |
| `send` | `Channel@T → T → Null` | Sends a value; suspends if buffer full. |
| `recv` | `Channel@T → T` | Receives next value; suspends until available. Returns error if channel is closed. |
| `select-once` | `[Seq [Channel@T Fn@T→R]] → R` | Polls channels in pseudo-random order; calls handler of first ready channel. Removes closed channels from consideration; errors if all sources closed. |
| `par` | `expr → T` | Spawns `expr` on the thread pool immediately; returns same value when demanded. No-op if thunk already in flight. |
| `par-map` | `[Fn@B [A]] → [Seq A] → [Seq B]` | Parallel map: all elements submitted to thread pool simultaneously; results in order. |
| `par-filter` | `[Fn@Bool [A]] → [Seq A] → [Seq A]` | Parallel filter. |
| `signal-channel` | `[Seq Signal] → Channel@Signal` | Channel that delivers the signal name (`"SIGTERM"`, `"SIGINT"`) when any listed signal fires. Background task aborted when channel is dropped. |
| `timer-channel` | `ClockCap → Duration → Channel@Timestamp` | Fires on each interval. Delivers the scheduled fire time as a `Timestamp`. Lag = `[timestamp-diff [now clock] scheduled]`. Dropped ticks not queued. Background task aborted when channel is dropped. |
| `watch-channel` | `DirCap → Str → Channel@Null` | Fires when the watched path is modified. Background task aborted when channel is dropped. |
| `tcp-listen` | `NetCap → Int → Channel@Handle` | One bidirectional `Handle` per accepted TCP connection. HTTP/1.1 framing is `stdlib/http1.llt`. |
| `quic-listen` | `NetCap → Int → Channel@QuicConn` | One `QuicConn` per accepted QUIC connection. HTTP/3 framing via `h3` builtin. |

### Type Checker (`src/typecheck.rs`)

**Current:** No `Task`, `Channel`, or `Context` types.

**Proposed:** `Type::Task(Box<Type>)`, `Type::Channel(Box<Type>)`, and `Type::Context` (opaque) — all new. `task` infers the inner type from the body expression. `await` unifies `Task@?T` → `?T`. `send`/`recv` unify channel element type. `select-once` checks handler arity against channel element type. `with-cancel` returns `[Context [Fn@Null []]]`. `timeout` returns `Result@T`.

**Impact:** Moderate — new inference rules for 5 types. Pattern is identical to existing parameterized types (`Seq@T`, `Map@[K:V]`).

### `async_rt.rs`

**Current:** Thread-local `current_thread` Tokio runtime; `block_on` bridge.

**Proposed:** `run_program(fut)` — constructs `LocalSet` + `current_thread` runtime, drives top-level evaluation. `spawn_task(fut)` — `spawn_local` with `TaskHandle` return. `block_on` removed.

**Impact:** Minor — file shrinks; `LocalSet` replaces raw `block_on`.

### Test Suite

**Current:** Tests call `eval`/`materialize` synchronously in `#[test]`.

**Proposed:** Tests become `#[tokio::test(flavor = "current_thread")]`. The test helper `run_eval(source)` wraps evaluation in `run_program(...)`. A blanket rewrite of test helpers; individual tests are unchanged.

**Impact:** Moderate — test infrastructure changes uniformly; test assertions are unchanged.

### LSP (`src/lsp/`)

**Current:** Analysis functions are synchronous; LSP protocol handler runs its own event loop.

**Proposed:** Analysis functions become `async fn`. The LSP handler runs them under a `LocalSet`. Where the LSP protocol requires synchronous responses, `block_on` remains at the LSP layer boundary only.

**Impact:** Moderate — LSP analysis functions gain `async`; protocol layer is unchanged.

### CLI (`src/main.rs`)

**Current:** `run_eval()` calls synchronous `eval_source`.

**Proposed:** `run_eval()` calls `run_program(eval_source_async(...))`. Entry point gains one call to `run_program`.

**Impact:** Minor — two lines changed.

### Dependencies (`Cargo.toml`)

- `tokio` features: `macros`, `rt-multi-thread`, `time`, `signal`, `sync`.
- `tokio-util` — `CancellationToken` for `Context`.
- `notify` — filesystem watch for `watch-channel` (inotify/kqueue/FSEvents are OS-specific; justified Rust dep).
- `num_cpus` — default worker thread count.
- `h3` — HTTP/3 framing (QPACK) on top of quinn, for `quic-listen` and the `h3-request` builtin used by `stdlib/http3.llt`.
- Remove: `hyper` — HTTP/1.1 framing moves to `stdlib/http1.llt`.
- Remove: `reqwest` — HTTP client moves to `stdlib/http.llt` on top of `tcp-connect` + `tls-layer`.
- Remove: `tokio::runtime::Builder::new_current_thread().block_on(...)` — replaced by `run_program`.

## Prerequisites

- `EvalContext` refactor — complete. `ctx` is already threaded through `eval`/`materialize`; migrating to `Arc<EvalContext>` is mechanical.
- `block_on` bridge in `async_rt.rs` — already present; replaced, not added.
- No type system prerequisites — `Task@T`, `Channel@T`, and `Context` are new types with no dependency on existing proposals.
- `dist-eval.md` builds on this proposal for distributed cluster evaluation.

## References

- Marlow, S. et al. (2009). "Runtime Support for Multicore Haskell." *ICFP '09*. — `par`/`seq` sparks and the GHC scheduler; the implicit-parallelism model that `par-dist-eval.md` draws from.
- Syme, D., Petricek, T. & Lomov, D. (2011). "The F# Asynchronous Programming Model." *PADL '11*. — Async workflows as first-class values; `task { }` computation expressions directly analogous to tinct's `[task ...]` builtin.
- Leijen, D., Schulte, W. & Burckhardt, S. (2009). "The Design of a Task Parallel Library." *OOPSLA '09*. — Structured task concurrency; `await-all`/`await-any` semantics.
- Tokio documentation. "tokio::task::LocalSet." — The `!Send` cooperative execution model that this proposal uses.
- Go language specification. "Select statements." *go.dev/ref/spec*. — `select` over channels; tinct's `select-once` is directly analogous.
- Jones, S.P., Gordon, A. & Finne, S. (1996). "Concurrent Haskell." *POPL '96*. — MVars and the original "communicating lazy threads" model; conceptual ancestor of this proposal.
