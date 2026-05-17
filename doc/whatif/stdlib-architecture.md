# What If: Stdlib Architecture — The Rust/tinct Boundary

**State:** Proposal

What is the principled boundary between Rust builtins and tinct stdlib, and what does the full stdlib module map look like when that principle is applied consistently?

## The Principle

tinct is a language, not a Rust application. Rust is the OS interface layer. Everything above that is tinct.

**Stay in Rust if any of these are true:**

1. **Binary protocol or encoding.** TLS crypto, QUIC transport, HTTP/3 QPACK framing, Huffman tables, binary frame formats. These require bit-level manipulation with well-specified constants; there is no interesting logic to express in tinct.
2. **Security-critical.** Cryptographic primitives, TLS certificate verification. Must be in audited Rust.
3. **Performance cliff.** JSON parsing (`from-json`, `emit`) is exercised on every program that reads config or imports data. A tinct-implemented JSON parser would make startup painful. Similarly, the core arithmetic and comparison operators that every evaluation step exercises.
4. **OS system call.** File I/O (`open`, `read-bytes`, `write-bytes`), socket binding (`tcp-connect`, `tcp-listen`, `quic-listen`), signal delivery, real-time clock, process control.
5. **Bootstrap problem.** The tinct parser, evaluator, and type checker. These must exist before tinct can run.

**Move to tinct if:**

- It is a text protocol (HTTP/1.1, URLs, TOML, path strings, query strings).
- It is higher-order function composition (routing, middleware, pipeline transformation).
- It is data normalization or formatting (header lowercasing, response building, date formatting).
- It is utility logic over primitives already in tinct (retry, backoff, circuit breaker, result chaining).

**The test:** could this be written clearly in tinct, using only existing builtins, in under 200 lines? If yes, it belongs in stdlib.

---

## The Irreducible Rust Set

### Core Evaluation
All operators, type predicates, `if`, `match`, `fn`, `let`, `loop`, `apply`, `eval`, `emit`, `from-json`, `ast-of`, `include`.

### Arithmetic and Comparison
`+`, `-`, `*`, `/`, `mod`, `<`, `=`, `>`, etc. Every evaluation step uses these.

### String Primitives
Byte-level operations only: `byte-at`, `byte-count`, `str-slice`, `str-concat`, `utf8-encode`, `utf8-decode`, `str-to-int`, `str-to-float`, `int-to-str`, `float-to-str`. Higher-level string utilities (`trim`, `split`, `pad-left`, `starts-with?`, `str-contains?`) belong in `stdlib/strings.llt` — they are already composable from these primitives.

### Sequence Core
`head`, `tail`, `cons`, `seq-length`, `make-entry`, `merge`, `get`, `get-in`. The lazy sequence machinery (`iterate`, `unfold`, `range`, `repeat`) stays in Rust because it requires `PendingBuiltin` thunk creation that is tightly coupled to the evaluator.

### I/O Primitives
```
open          DirCap → Str → Flags → Handle
read-bytes    Handle → Int → Bytes
write-bytes   Handle → Bytes → Null
slurp         Handle → Str
lines         Handle → Seq@Str
write-handle  Handle → Str → Handle
close         Handle → Null
flush         Handle → Null
stat          DirCap → Str → Dict
listdir       DirCap → Str → Seq@Str
mkdir         DirCap → Str → Null
rename        DirCap → Str → Str → Null
delete        DirCap → Str → Null
```

### Network Primitives
```
tcp-connect   NetCap → Str → Int → Handle          # outgoing TCP
tcp-listen    NetCap → Int → Channel@Handle         # server TCP, one Handle per client
tls-layer     Handle → Str → Handle                 # TLS over any Handle
quic-connect  NetCap → Str → Int → QuicConn         # outgoing QUIC
quic-listen   NetCap → Int → Channel@QuicConn       # server QUIC
h3-request    QuicConn → RawRequest → RawResponse   # HTTP/3 frame exchange (QPACK)
quic-stream   QuicConn → Handle                     # open a QUIC stream as Handle
```

`tls-layer` wraps any `Handle` with TLS (rustls), so the same primitive works for TCP, Unix sockets, and any future transport.

### System
```
time-now       ClockCap → Int              # Unix timestamp ms
sleep          Int → Null                 # milliseconds
env-var        Str → Result@Str           # read environment variable
args           → Seq@Str                  # process arguments
spawn-process  DirCap → Str → Seq@Str → Dict  # child process, returns {stdin out err}
exit           Int → Null                 # graceful (drain tasks)
exit-now       Int → Null                 # immediate process::exit
```

### Concurrency (from async-eval.md)
`task`, `await`, `await-all`, `await-any`, `channel`, `send`, `recv`, `select-once`, `context`, `with-cancel`, `with-timeout`, `with-deadline`, `cancelled?`, `timeout`, `par`, `par-map`, `par-filter`.

### Event Sources (Rust — OS interface)
`signal-channel`, `timer-channel`, `watch-channel`, `tcp-listen`, `quic-listen`.

`http-channel` is **not** in this list — it is `stdlib/http.llt`.

---

## What This Displaces

### hyper — removed

hyper's value proposition is HTTP/1.1 and HTTP/2 framing. HTTP/1.1 is a text protocol; `stdlib/http1.llt` handles it. HTTP/2 (HPACK) is the only gap: it requires binary compression tables. HTTP/2 is deferred — HTTP/1.1 and HTTP/3 cover virtually all practical use cases, and HTTP/2 can be added later via a targeted dep if needed.

### reqwest — removed as direct dep

reqwest is a high-level HTTP client built on hyper. With HTTP/1.1 in tinct and HTTP/3 via quinn, tinct programs use `[fetch cap url]` from `stdlib/http.llt`, which is implemented in tinct on top of `tcp-connect` + `tls-layer` + `stdlib/http1.llt`. reqwest may remain as a transitive dep through other crates but is no longer owned.

### High-level string builtins — migrated

`trim`, `pad-left`, `pad-right`, `starts-with?`, `ends-with?`, `str-contains?`, `str-replace` belong in `stdlib/strings.llt`. They are composable from byte-level primitives. Moving them out of Rust reduces `builtins.rs` and makes them overridable.

---

## The Stdlib Module Map

### `stdlib/prelude.llt` — already exists, trimmed

Keep: higher-order functions that require lazy evaluation semantics (`map`, `filter`, `reduce`, `and-then`, `or-else`), result combinators, core type utilities. Remove: anything that belongs in a focused module below.

### `stdlib/strings.llt`

Higher-level string utilities built on byte primitives:

```tinct
trim:         Str → Str
trim-left:    Str → Str
trim-right:   Str → Str
pad-left:     Str → Int → Str → Str
pad-right:    Str → Int → Str → Str
starts-with?: Str → Str → Bool
ends-with?:   Str → Str → Bool
str-contains?: Str → Str → Bool
str-replace:  Str → Str → Str → Str
str-split-lines: Str → Seq@Str
str-repeat:   Str → Int → Str
words:        Str → Seq@Str
unwords:      Seq@Str → Str
```

### `stdlib/seq.llt`

Higher-order sequence utilities not requiring lazy machinery:

```tinct
zip-with:    Fn@A@B@C → Seq@A → Seq@B → Seq@C
enumerate:   Seq@A → Seq@[Int A]
chunk:       Int → Seq@A → Seq@Seq@A
partition:   Fn@A@Bool → Seq@A → [Seq@A Seq@A]
group-by:    Fn@A@Str → Seq@A → Map@[Str: Seq@A]
sort-by:     Fn@A@B → Seq@A → Seq@A           # B must be Comparable
uniq-by:     Fn@A@B → Seq@A → Seq@A
flat-map:    Fn@A@Seq@B → Seq@A → Seq@B
scan:        Fn@B@A@B → B → Seq@A → Seq@B     # running accumulator
window:      Int → Seq@A → Seq@Seq@A           # sliding window
interleave:  Seq@A → Seq@A → Seq@A
tee:         Fn@Seq@A@B → Seq@A → [B Seq@A]   # fork without consuming
```

### `stdlib/path.llt`

Path manipulation as string operations:

```tinct
path-join:    Str → Str → Str
path-dirname: Str → Str
path-basename:Str → Str
path-ext:     Str → Str
path-stem:    Str → Str
path-split:   Str → Seq@Str
path-abs?:    Str → Bool
path-normalize: Str → Str          # remove . and .. components
```

### `stdlib/http1.llt` — new

HTTP/1.1 client and server framing in pure tinct, on top of `Handle`:

```tinct
# Server side — on top of tcp-listen
parse-request:  Handle → Result@RawRequest
write-response: Handle → RawResponse → Null
serve-conn:     Handle → Fn@RawRequest@RawResponse → Null  # one request/response cycle
serve-keepalive: Handle → Fn@RawRequest@RawResponse → Null # keep-alive loop

# Client side — on top of tcp-connect / tls-layer
build-request:  Str → Str → Str → Seq@[Str Str] → Str → Str  # method path host headers body
send-request:   Handle → Str → Result@RawResponse              # write + read response
parse-response: Str → Result@RawResponse

# Shared types (plain tinct dicts)
# RawRequest:  { method: Str, path: Str, version: Str, headers: Seq@[Str Str], body: Str }
# RawResponse: { status: Int, headers: Seq@[Str Str], body: Str }
```

HTTP/1.1 chunked transfer encoding, keep-alive, pipelining are all handled here in tinct.

### `stdlib/http3.llt` — new

Thin wrapper around the `h3-request` Rust builtin, delivering the same `RawRequest`/`RawResponse` shape:

```tinct
# Server side — on top of quic-listen
serve-h3-conn:  QuicConn → Fn@RawRequest@RawResponse → Null

# Client side
fetch-h3:       NetCap → Str → Str → Seq@[Str Str] → Str → Result@RawResponse
```

### `stdlib/http.llt` — new, the unified HTTP interface

```tinct
# Server: unified channel over HTTP/1.1 + HTTP/3
http-channel: [fn [cap port]
  [let [tcp-ch:  [tcp-listen cap port]     # TCP socket (HTTP/1.1)
        quic-ch: [quic-listen cap port]]   # UDP socket (HTTP/3), same port
    [let [reqs: [channel 1000]]
      [task [pump-http1 tcp-ch reqs]]
      [task [pump-http3 quic-ch reqs]]
      reqs]]]
# Request dict delivered to channel:
# { method: Str, path: Str, version: Str, headers: Seq@[Str Str], body: Str, respond: Fn }
# Headers are raw — normalization is the caller's job

# Client: fetch auto-negotiates HTTP/1.1 or HTTP/3
fetch:        [fn [cap url] ...]               # auto-negotiate based on ALPN / Alt-Svc
fetch-h1:     [fn [cap url] ...]               # explicit HTTP/1.1
fetch-h3:     [fn [cap url] ...]               # explicit HTTP/3

# Routing
router:       [fn [routes req] ...]            # match on method + path pattern
not-found:    [fn [req] [req.respond [status: 404 headers: [] body: "not found"]]]

# Header utilities
headers-map:  [fn [headers] ...]               # Seq@[Str Str] → Map@[Str: Seq@Str] (lowercase, multi-value)
get-header:   [fn [req name] ...]              # case-insensitive single-value lookup
parse-query:  [fn [path] ...]                  # "/users?page=2" → {path: "/users" params: {page: "2"}}

# Response builders
respond:      [fn [req status headers body] [req.respond [status: status headers: headers body: body]]]
ok:           [fn [req body]  [respond req 200 [] body]]
json-ok:      [fn [req data]  [respond req 200 [["content-type" "application/json"]] [emit data]]]
redirect:     [fn [req loc]   [respond req 302 [["location" loc]] ""]]
not-found-r:  [fn [req]       [respond req 404 [] "not found"]]
server-error: [fn [req]       [respond req 500 [] "internal server error"]]

# Middleware composition (handler = Fn@Request@Null)
with-logging: [fn [handler] [fn [req] [log [str req.method " " req.path]] [handler req]]]
with-cors:    [fn [origins handler] [fn [req] ...]]
with-auth:    [fn [verify handler] [fn [req] ...]]
with-timeout: [fn [ms handler] [fn [req] [timeout ms [task [handler req]]]]]
```

### `stdlib/net.llt` — already exists, expanded

URL parsing, DNS helpers, higher-level connection utilities. The existing `parse-url`, `http-get`, `fetch` functions are already here; expand with:

```tinct
url-encode:    Str → Str
url-decode:    Str → Str
form-encode:   Map@[Str:Str] → Str
form-decode:   Str → Map@[Str:Str]
resolve-host:  NetCap → Str → Seq@Str    # DNS lookup → IP list
```

### `stdlib/datetime.llt` — already proposed, unchanged

Timestamp, Duration, formatting/parsing on top of `time-now`.

### `stdlib/toml.llt` — expand from toml-lite

Complete TOML 1.0 parser in tinct. The existing `toml-lite.llt` handles the common case; extend to handle arrays of tables, inline tables, dotted keys.

### `stdlib/async.llt` — new

Concurrency utilities built on core async primitives:

```tinct
loop-select:       Seq@[Channel handler] → Null    # recurring event loop
retry:             Int → Fn@[]@Result → Result      # N retries with backoff
retry-with:        Dict → Fn@[]@Result → Result     # configurable: attempts, delay-ms, backoff
throttle:          Int → Channel@A → Channel@A      # rate-limit a channel
debounce:          Int → Channel@A → Channel@A      # coalesce rapid events
merge-channels:    Seq@Channel@A → Channel@A        # fan-in
broadcast:         Channel@A → Seq@Channel@A → Null # fan-out
pipeline:          Seq@Fn → Channel@A → Channel@B   # channel transformation chain
```

### `stdlib/result.llt` — extract from prelude

```tinct
and-then:   Result@A → Fn@A@Result@B → Result@B
map-ok:     Result@A → Fn@A@B → Result@B
map-err:    Result@A → Fn@Str@Str → Result@A
unwrap-or:  Result@A → A → A
unwrap:     Result@A → A      # error on Err
ok?:        Result@A → Bool
err?:       Result@A → Bool
collect-results: Seq@Result@A → Result@Seq@A   # fail-fast over a sequence
```

### `stdlib/cap.llt` — new

Capability utilities:

```tinct
narrow:       DirCap → Str → DirCap          # already exists, ensure it's here
readable?:    DirCap → Str → Bool            # can we read this path?
writable?:    DirCap → Str → Bool
with-temp:    DirCap → Fn@DirCap@A → A      # temp directory, cleaned up after fn
distributable?: any → Bool                  # from dist-eval.md — no cap values
```

---

## What Would Change

### `src/builtins.rs`

**Remove** (migrate to stdlib): `trim`, `pad-left`, `pad-right`, `starts-with?`, `ends-with?`, `str-contains?`, `str-replace` — these move to `stdlib/strings.llt`. Each is a few lines of Rust that can be expressed as tinct string operations.

**Add**: `tcp-listen`, `quic-listen`, `h3-request`, `byte-at`, `byte-count`, `str-slice` (byte-level string primitives to support the stdlib modules).

**Net effect:** `builtins.rs` shrinks. The Rust code that remains is genuinely irreducible.

### `stdlib/prelude.llt`

Reduced in scope. Functions that have natural homes in focused modules move there. The prelude becomes the set of things that should be in scope for every tinct program: `map`, `filter`, `reduce`, `and-then`, `ok?`, `err?`, plus re-exports from the modules above.

### `Cargo.toml`

**Remove direct deps:** `hyper`, `reqwest`.

**Add:** `h3` (HTTP/3 framing, from the quinn team).

**Retained:** `quinn`, `rustls`, `tokio`, `tokio-util`, `notify`, `serde_json` (for `from-json`/`emit`), `indexmap`.

**Net effect:** smaller dependency surface. HTTP/1.1 and client/server logic are in tinct; binary protocols (TLS, QUIC, HTTP/3) remain in audited Rust.

### `stdlib/` directory structure

```
stdlib/
  prelude.llt        — trimmed core (map, filter, reduce, result combinators)
  strings.llt        — higher-level string utilities (moved from builtins)
  seq.llt            — higher-order sequence utilities (chunk, group-by, sort-by, …)
  path.llt           — path manipulation
  net.llt            — URL parsing, DNS, connection utilities (expanded)
  http1.llt          — HTTP/1.1 framing in tinct (new)
  http3.llt          — HTTP/3 wrapper around h3 builtin (new)
  http.llt           — unified HTTP channel, fetch, router, middleware (new)
  async.llt          — concurrency utilities: retry, throttle, broadcast (new)
  result.llt         — result combinators (extracted from prelude)
  cap.llt            — capability utilities (new)
  datetime.llt       — date/time (existing proposal)
  regex.llt          — regex engine (existing proposal)
  toml.llt           — complete TOML parser (expanded from toml-lite)
  sql.llt            — SQL data sources (existing proposal)
  dist.llt           — distributed map/reduce (from dist-eval.md)
```

---

## The HTTP Stack End-to-End

To make the architecture concrete, here is the full request lifecycle for an HTTP/1.1 request, showing exactly where the Rust/tinct boundary sits:

```
OS: TCP connection arrives on port 8080
  ↓
Rust: tcp-listen accept() → Value::Handle (bidirectional TCP stream)
  ↓
Rust: Channel<Handle> delivers the handle to tinct
  ↓
tinct: stdlib/http1.llt parse-request reads lines from Handle
         "GET /api/users?page=2 HTTP/1.1\r\n..."
         → RawRequest { method: "GET", path: "/api/users?page=2", ... }
  ↓
tinct: stdlib/http.llt pump-http1 attaches respond fn (oneshot tx), sends to reqs channel
  ↓
tinct: user handler receives Request dict
         router routes on method + path
         parse-query extracts page=2
         headers-map normalizes headers
         json-ok builds response
         req.respond called
  ↓
tinct: stdlib/http1.llt write-response serializes response dict to HTTP/1.1 wire format
  ↓
Rust: write-handle sends bytes through the Handle's write_inner
  ↓
OS: bytes sent on TCP socket
```

Every step marked "tinct" is in `stdlib/`. The Rust steps are the irreducible OS interface.

---

## Prerequisites

- `async-eval.md` — the async runtime is required before stdlib HTTP functions can be non-blocking. Without async eval, `http1.llt`'s `parse-request` would block its entire thread.
- `lib-net-v2.md` — `tcp-listen`, `quic-listen`, `tls-layer` build directly on the connection model from this proposal.
- `runtime-reflection.md` — `ast-of` is used by `dist.llt` for thunk serialization.
- `error-patterns.md` — all stdlib I/O functions return `Result`.

## References

- Pike, R. (2012). "Go Concurrency Patterns." Google I/O. — The principle that channel-based event sources unify signals, timers, and network events under one abstraction; directly implemented in `stdlib/async.llt`.
- The Lua Reference Manual. "The C API." *lua.org*. — Lua's model of a minimal C core with the standard library written in Lua wherever possible; the direct inspiration for this proposal's Rust/tinct boundary.
- Nix Reference Manual. "Built-in Functions." *nixos.org*. — Nix keeps ~100 built-in functions and implements all package logic in the Nix language; tinct follows the same discipline.
- Wirfs-Brock, R. & McKinney, A. (2017). "JavaScript: The First 20 Years." *HOPL IV*. — The history of JavaScript's stdlib growth shows the cost of embedding too much logic in the runtime: stdlib functions become frozen at the C level and cannot be patched, overridden, or improved in the language itself.
