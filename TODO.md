# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Research

### `builtin-privacy`: Research hiding `builtin-*` stable aliases from non-prelude code

`builtin-*` names (`builtin-if`, `builtin-lt`, `builtin-eq`, `builtin-reduce`, etc.) are stable aliases that bypass shadowable prelude wrappers so stdlib code can always reach the Rust implementation. They live in `standard_builtins()` and are therefore globally available, but they are an internal implementation detail — user code and non-prelude stdlib should use `if`, `<`, `=`, `reduce`, etc. instead.

The problem: there is currently no enforcement. Any tinct file can call `builtin-lt` directly. Sample code (`samples/versions.llt`) was found using them; this has been fixed, but the underlying access remains open.

Research questions:
- Can `builtin-*` be removed from `standard_builtins()` and injected only into the prelude's evaluation context, so they are invisible to user code?
- Would this break any corpus tests or stdlib files that currently call `builtin-*` directly (the prelude itself uses them extensively)?
- Is there a mechanism to have a "prelude-only" env layer that sits between `standard_builtins()` and the user-visible environment?
- Alternative: emit a type-checker warning (T-code) when user code references a `builtin-*` name? This preserves current behavior while making the smell visible.
- Alternative: rename `builtin-*` to something less guessable (e.g., `__builtin-*`) to discourage use while keeping them accessible?

- [x] Survey all `builtin-*` call sites in `stdlib/` to verify the prelude uses them and identify any non-prelude stdlib usage (`stdlib/`) — prelude.llt: extensive (correct); macros.llt, path.llt, toml-lite.llt: all use builtin-* directly (should not)
- [x] Design a mechanism to restrict `builtin-*` visibility to `prelude.llt` evaluation only — see `doc/whatif/builtin-privacy.md`

### `error-patterns`: Research consistent error handling conventions for tinct stdlib

There is currently no consistent convention for how functions signal failure. Observed patterns in the wild:

- **`try`/`match` Result dicts**: `[try [fn [] ...]]` → `{ok: val}` or `{err: msg}`, caller dispatches with `match`. Used in `versions.llt`, `has?-impl` in prelude.
- **Propagation (let it crash)**: functions just error at runtime; caller is responsible for wrapping in `try` if they need to recover. Most builtins do this.
- **Sentinel strings**: `versions.llt` used `"ERR:..."` prefix strings to signal failure across a table-rendering boundary where a crash would be worse than a bad cell value.
- **Null / empty dict**: some functions return `[]` on "nothing found" (e.g., `get-or` with a default). Mixed with error-propagation at other sites.
- **Structural ADTs**: `[type [ok: a] [err: String]]` is available in the type system but not used as a stdlib-wide Result convention.

The `net.llt` `fetch` function illustrates the problem: it returns `{status: headers: body:}` for success but has no error arm — a failed connection propagates as an uncaught eval error, which is fine if callers always wrap in `try`, but undocumented.

Research questions:
- Should tinct have a canonical `Result` type alias (`[type [ok: a] [err: String]]`) that stdlib I/O functions return?
- When is `try`/`match` the right boundary, and when should errors propagate (the "let it crash" approach is idiomatic in lazy languages)?
- Should functions that are *expected* to fail sometimes (network, file I/O) always return `Result`, while pure functions always propagate?
- What does the type checker need to enforce this? Can `[fn@Result [...]` annotations provide useful guarantees?
- Survey: which stdlib functions currently crash vs return Result vs return sentinel values? Are the choices consistent?

- [x] Survey error handling patterns across all stdlib files and sample scripts — see `doc/whatif/error-patterns.md`

### `bas-core`: Boolean-Algebraic Subtyping — Type Algebra and Constraint Solver

See `doc/whatif/boolean-algebraic-subtyping.md` and `doc/07-type-extensions.md §Boolean-Algebraic Subtyping`. **Spec chapters:** `doc/07-type-extensions.md §BAS`.

- [x] **RowVar removal step 1 — annotation expansion**: in `typecheck_annot.rs`, change `infer_record_annotation` to emit `Record(fields, RowTail::Empty)` instead of `Record(fields, RowTail::RowVar(...))` — under BAS all structural annotations are open by default via conjunction elimination, so the RowVar is no longer needed to express openness (`src/typecheck_annot.rs`)
- [x] **RowVar removal step 2 — is_subtype width subtyping**: replace the RowVar-based width subtyping arms in `is_subtype` (`src/types.rs:~355-375`) with pure BAS conjunction elimination: `Record(F1, Empty) <: Record(F2, Empty)` iff `F2.keys() ⊆ F1.keys()` and `∀k ∈ F2: F1[k] <: F2[k]`; remove the RowVar-specific open/closed divergence branches (`src/types.rs`)
- [x] **RowVar removal step 3 — unification**: simplify record unification in `type_unify.rs` to remove `unify_remainders` and RowVar binding — field-by-field unification only; if one record has fields the other lacks, the subtyping rule (step 2) handles it; no new bindings in the substitution map for row variables (`src/type_unify.rs`)
- [x] **RowVar removal step 4 — cleanup**: delete `RowTail` enum entirely; delete `Row.tail` field; remove `row_map` from `Substitution`; remove `row_vars` from `TypeScheme`; remove `alias_row_map` from `typecheck.rs`/`typecheck_dict.rs`/`typecheck_annot.rs`; remove dead `row_map` merge block from `typecheck_dict.rs`; remove `collect_row_vars()` method; update all match sites across 8 source files (`src/types.rs`, `src/type_unify.rs`, `src/type_env.rs`, `src/typecheck.rs`, `src/typecheck_dict.rs`, `src/typecheck_annot.rs`, `src/eval.rs`, `src/coverage.rs`)
- [x] **RowVar removal step 5 — tests**: `just test` passes (2021 lib tests + 30 corpus tests); updated tests that asserted pre-BAS behavior; added `tests/corpus/eval/typecheck/bas_width_subtyping.llt-eval` (confirms BAS open-record semantics: `[fn [r@[name: String]] r.name]` called with `[name: "Alice" age: 30]` succeeds via width subtyping)
- [x] Add `Type::Negation(Box<Type>)` variant to `src/types.rs`; updated all match sites (types.rs, type_unify.rs, type_env.rs, eval.rs)
- [x] Add `Type::Never` as explicit bottom type variant; `Type::Top` already existed; updated is_subtype (S-NEVER rule), is_consistent, Display, value_matches_type
- [x] Implement S-RcdTop (disjoint single-field records union = Top) and S-ClsBot (disjoint intersections = Never) in `is_subtype`; `simplify_type` added for basic RDNF groundwork (`src/types.rs`)
- [x] C-Var1/2 constraint rewriting: conservative approximation — unify(concrete, Union([..., TypeVar, ...])) binds TypeVar to concrete (`src/type_unify.rs`)
- [x] **RDNF step 1 — call sites**: `infer_match` collects arm result types and unions+simplifies; `infer_if` uses normalize_union+simplify_type (removed least_upper_bound); type_map insertion simplifies before storing for LSP hover; annotation-declared union types NOT simplified (ADT aliases preserved) (`src/typecheck.rs`, `src/typecheck_annot.rs`)
- [x] **RDNF step 2 — recursive simplification**: `simplify_children` bottom-up pass for Union/Intersection/Negation/Record/Seq/Map/Function; subsumption elimination (drop A when A <: non-Negation B); literal promotion (≥2 IntLiterals→Int, ≥2 StringLiterals→Str) (`src/types.rs`)
- [x] **RDNF step 3 — tests**: corpus `rdnf_match_union_simplification.llt-eval`, `rdnf_negation_known_type.llt-eval`; 10 unit tests for recursive simplification, subsumption elimination, literal promotion (`tests/corpus/eval/typecheck/`, `src/types.rs`)
- [x] Multi-field annotation → intersection: `@[x: Int  y: String]` → `Intersection([{x:Int,...ρ1}, {y:String,...ρ2}])`; Record↔Intersection unify + eval dispatch updated (`src/typecheck_annot.rs`, `src/type_unify.rs`, `src/eval.rs`, `src/eval_materialize.rs`)
- [x] Add `@[[all A B]]` (intersection) and `@[[without A]]` (negation) annotation syntax (`src/typecheck_annot.rs`)
- [x] False-branch narrowing: `apply_negation_narrowings()` in if-false branch, type predicates narrow to Negation type (`src/typecheck.rs`)
- [x] I-Case3 in infer_match: remaining_scrutinee accumulates negations across arms for precise type narrowing (`src/typecheck.rs`)
- [x] BAS corpus tests: negation narrowing, S-RcdTop, str narrowing, I-Case3 three-arm, S-ClsBot variant match (`tests/corpus/eval/typecheck/bas_*.llt-eval`)
- [x] Fix `@[[all A B]]` syntax: added Call form dispatch in resolve_type_expr and resolve_annotation; conservative Negation subtype/unify arms added (`src/typecheck_annot.rs`, `src/types.rs`, `src/type_unify.rs`)

### `result-nominal`: Nominal Result Type and Stdlib Retrofit

See `doc/whatif/error-patterns.md` and `doc/07-type-extensions.md §Nominal Result Type`. **Spec chapters:** `doc/07-type-extensions.md §Nominal Result Type`.

- [x] Update `builtin_try` to return nominal `Value::Variant { tag: "Ok"/"Err" }` (`src/builtins_meta.rs`)
- [x] Declare `[Result: [type [Ok a] [Err String]]]` + Ok/Err re-exports in prelude (`stdlib/prelude.llt`)
- [x] Add `and-then`, `result-map`, `result-or`, `result-ok` Result combinators (`stdlib/prelude.llt`)
- [x] Add `result` monad dict `[bind: and-then  pure: result-ok]` (`stdlib/prelude.llt`)
- [x] Migrate `has?-impl`, `try-or-impl`, `find-deep-try-check` to nominal `[Ok _]`/`[Err _]` patterns (`stdlib/prelude.llt`)
- [x] Implement `[do]` macro stub in `stdlib/macros.llt` (registered as defmacro)
- [x] Retrofit `stdlib/net.llt`: I/O functions return `Ok[...] | Err[...]` via `[try ...]`
- [x] Retrofit `stdlib/io.llt`: `read-file`, `read-lines` return Result
- [x] Retrofit `stdlib/toml-lite.llt`: `parse-toml-lite` returns Result
- [x] Tests: corpus tests updated for nominal Ok/Err format (14 corpus files + 6 unit tests)
- [ ] **Fix `Ok: Ok` circular dependency in prelude**: `Ok: Ok` and `Err: Err` in the public dict create self-referential letrec thunks — when `[Ok value]` is called in user code, the evaluator forces the prelude's `Ok` thunk which tries to force `Ok` again in the same letrec context, causing E070 circular dependency. Fix: remove the re-export thunks and instead expose the constructors registered by `[type ...]` directly (they are already in scope after the intermediate dict is evaluated); alternatively bind to lambda wrappers that don't recurse: `Ok: [fn [v] [Result.Ok v]]` using an internal name (`stdlib/prelude.llt`)
- [ ] **Update TypeEnv for `try` to reflect nominal return type**: `try` is typed in TypeEnv as returning a structural `{ok: T} | {err: String}` dict, but the runtime now returns nominal `Ok(T) | Err(String)` variants. This mismatch causes T004 "non-exhaustive match" when user code matches on `[Ok v]` / `[Err msg]`, and cascading T002 "undefined variable" errors for bindings that depend on the failing match. Fix: update the `try` type signature in `TypeEnv::with_builtins()` to return `Result[T]` (or `Ok[T] | Err[String]`) so the type checker accepts nominal match patterns (`src/type_env.rs`)
- [ ] **Swap `and-then` argument order from `[and-then f result]` to `[and-then result f]`**: current order (function first, result second) matches Elm but is backwards for tinct's data-flow idiom — pipelines read "take this result, then do this" which means result first. Rust's `.and_then(f)` and Haskell's `result >>= f` both put the result first. Update `and-then` signature and all callers including `result` monad dict (`bind: and-then`), `result-map`, `try-or-impl`, `find-deep-try-check`, and `[do]` macro desugaring which calls `monad.bind result f` (`stdlib/prelude.llt`, `stdlib/macros.llt`)
- [ ] **Remove `https-get` from `samples/versions.llt` entirely**: the script has its own `https-get` only because `net.llt`'s `fetch`/`http-get` were historically broken or unavailable; once `stdlib-protocols` net.llt rewrite lands, replace the local `https-get` with `[include %libdir "net.llt"]` + `[fetch %nc [url "https://..."]]`; until then, if a local function is needed it must take `cap@NetCap url@Url` (not host/port/path separately, and not close over `%nc`) to match `net.llt`'s conventions (`samples/versions.llt`)
**Depends on:** `bas-core` (for exhaustiveness checking; runtime behavior works without BAS but type checking requires it)

### `record-map-split`: Parameterized Map Type and Dict Union

See `doc/whatif/parameterized-dict.md` and `doc/07-type-extensions.md §Record/Map Split`. **Spec chapters:** `doc/07-type-extensions.md §Record/Map Split and Dict`.

- [x] Add `Type::Map(Box<Type>, Box<Type>)` + is_subtype/unify/apply/Display (`src/types.rs`, `src/type_unify.rs`, `src/type_env.rs`)
- [x] Register `Map` in `TypeEnv::with_builtins()` as `Map[Any Any]` (`src/type_env.rs`)
- [x] Add `get?` builtin: returns value or Null on missing key (`src/builtins_dict.rs`)
- [x] Add `record?` and `map?` predicates (`src/builtins_meta.rs`)
- [x] Implement structural dict equality: order-insensitive key comparison with cycle detection (`src/builtins_math.rs`)
- [x] `check_get`: Map[K V] → val_ty; get? → Union([val_ty, Null]); Record narrowing; get? registered in TypeEnv (`src/typecheck.rs`, `src/type_env.rs`)
- [x] Update `doc/03-data-model.md` §Equality: order-insensitive structural equality, cycle detection, extensional semantics (`doc/03-data-model.md`)
- [x] Corpus tests: dict_structural_equality, get_optional, record_map_predicates (`tests/corpus/eval/builtins/`)
**Depends on:** `bas-core`

### `hkt-monads`: Research higher-kinded types and generic monadic `[do]` for tinct

The `error-patterns` proposal adopts `[do monad ...]` with explicit monad-dict dispatch as a HKT-free path to monadic composition. The door is left open: when HKT is available, the explicit monad argument becomes optional (inferred from the return type of the first expression), and `[do]` dispatches through a `Monad` typeclass instead of a runtime field access.

Research questions:
- What is the right HKT model for tinct? Full System F-omega? Rank-1 kind polymorphism? Defunctionalization (as in Elm's `elm-program-test` or Oleg Kiselyov's tagless-final)?
- How does HKT interact with tinct's row polymorphism and BAS? Row variables are already kind `Row`; adding type constructors of kind `* → *` extends the kind system non-trivially.
- Can `[do]` remain backward-compatible — monad dict explicit when no Monad instance exists, inferred when one does?
- What is the minimal HKT extension needed to express `Monad m`, `Functor f`, and `Foldable t` without requiring full System F-omega? (Rank-1 kind polymorphism may suffice.)
- Survey: how do ML-family languages (OCaml 5 with effects, F#, SML) and recent functional languages (Koka, Frank, Unison) handle HKT and monadic abstraction?

- [x] Research HKT and generic monadic do-notation — see `doc/whatif/hkt-monads.md`

### `net-layers`: Research composable networking layer model for tinct

The current `connect`/`tls-connect`/`http-connect` design is a flat stack: raw transport, TLS wrapper, HTTP client. This is insufficient for modern networking where protocols compose arbitrarily:

- **QUIC**: session-oriented reliability on top of UDP (RFC 9000)
- **DTLS**: TLS on top of datagrams (RFC 6347)
- **HTTP/3**: HTTP on top of QUIC
- **SOCKS5**: TCP tunneling proxy (RFC 1928)
- **HTTP CONNECT**: HTTP-level tunneling
- **Wireguard**: encrypted transport (IP-level)
- **RFC 9297**: datagram extensions inside HTTP/3
- **STARTTLS**: TLS upgrade mid-connection (SMTP, IMAP, PostgreSQL)

The desired layering model allows expressions like:
`[http3 [quic [connect cap Udp host port]]]` or
`[http [socks5 [connect cap Tcp proxy port] target-host target-port]]`

The current `connect Udp` stub and `tls-connect` Handle form are both blocked pending this design.

Research questions:
- What is the right abstraction for a "transport" vs "session" vs "application" layer in tinct's capability model?
- How does each layer advertise its capabilities via the Handle cap row? (e.g., `Handle[Readable Writable Binary Stream]` vs `Handle[Readable Writable Binary Stream Tls Datagram]`)
- Can the composable layer model be expressed entirely in pure tinct over a small set of Rust transport primitives (TCP, UDP, Unix socket)?
- How do capabilities narrow at each layer? A SOCKS5 Handle over a NetCap should not grant broader access than the NetCap permits.
- Survey: Rust's `tower` (service layers), Haskell's `conduit`/`pipes`, Node.js Transform streams, gRPC interceptors — which compositional model fits tinct's lazy, capability-based design?
- What is the minimum Rust builtin surface needed? (Probably: raw TCP, raw UDP, Unix socket, and `tls-upgrade handle sni opts` — everything else in pure tinct or via reqwest)

- [x] Research composable networking layer model — see `doc/whatif/lib-net-v2.md`

### `connect-v2`: Transport-generic connect, Handle refactor, tls-layer, Unix sockets

See `doc/whatif/lib-net-v2.md` §Connector Protocol, §Layer Protocol, §Handle Refactor. **Spec chapters:** `doc/03-data-model.md §Network Handles`, `doc/03-data-model.md §Layers`.

- [x] Add `raw_tcp` + `creation_span` fields to `Value::Handle` (`src/value.rs`)
- [x] Refactor `builtin_connect` to dispatch on Transport variant tag (`src/builtins_io.rs`)
- [x] Implement `connect cap UnixStream path` (Linux-only via /proc/self/fd) (`src/builtins_io.rs`)
- [x] Implement `connect cap Udp` — Value::DatagramHandle with UdpSocket + send-datagram/recv-datagram builtins (189 total)
- [x] Implement `connect cap UnixDatagram` — DatagramSocket enum (Udp | UnixDgram), Linux autobind via UnixDatagram::bind("") + connect(path) (`src/value.rs`, `src/builtins_io.rs`)
- [x] `connect cap NamedPipe` — stub with platform error (Windows-only, not implementable on Linux)
- [x] `icmp-ping cap host timeout-ms` → `{ok: {latency-ms: Int}} | {err: String}`; SOCK_DGRAM+IPPROTO_ICMP (unprivileged Linux 3.11+); NetCap allowlist enforced before socket creation; RFC 792 checksum (`src/builtins_io.rs`)
- [x] Implement `tls-layer handle sni opts` → Handle with Tls cap; consumes raw_tcp (`src/builtins_io.rs`)
- [x] Remove `tls-connect` entirely — replaced by `tls-layer` (`src/builtins.rs`, `src/builtins_io.rs`, `src/type_env.rs`)
- [x] Register transport variants (UnixStream, UnixDatagram, NamedPipe, Icmp) + Url type alias (`src/builtins.rs`, `src/type_env.rs`)
- [x] Document Connector capability policy in `doc/11a-builtins.md` §Network
- [x] Update `check_net_cap_allowlist` for ICMP (host-only, no port) (`src/builtins_io.rs`)
- [x] Corpus tests: connect arity, UDP stub, UnixStream arity, tls-layer no raw_tcp, NamedPipe stub (`tests/corpus/eval/builtins/`)
- [x] Update `doc/11a-builtins.md` §Network: transport-generic connect, tls-layer, removed Proxy Tunnels

### `http-sessions`: QUIC, HTTP/2, HTTP/3 Sessions

See `doc/whatif/lib-net-v2.md` §Session Protocol. **Spec chapters:** `doc/03-data-model.md §Sessions`.

- [x] **quinn step 1 — deps**: quinn 0.11 (rustls+ring), h3 0.0.8, h3-quinn 0.0.10, tokio 1 full (`Cargo.toml`)
- [x] **quinn step 2 — shared tokio runtime**: thread_local current_thread runtime + `block_on()` helper (`src/async_rt.rs`, `src/lib.rs`)
- [ ] **quinn step 3 — quic-session builtin**: implement `builtin_quic_session` — parse cap/host/port/opts; build `rustls::ClientConfig` from opts (reuse `build_tls_config` from tls-layer); create `quinn::Endpoint`; call `async_rt::block_on(endpoint.connect(addr, host)?.await)`; store `quinn::Connection` in `Value::QuicSession` (replace `Rc<()>` placeholder) (`src/builtins_io.rs`)
- [ ] **quinn step 4 — stream builtins**: implement `builtin_quic_open_stream` — `block_on(conn.open_bi())` returning `(SendStream, RecvStream)`; wrap in `Value::Handle` with Binary RW Stream caps using `AsyncRead`/`AsyncWrite` adapters; implement `builtin_quic_open_datagram` — `conn.send_datagram(bytes)` / `block_on(conn.read_datagram())` via `Value::DatagramHandle` (`src/builtins_io.rs`)
- [ ] **quinn step 5 — http3-session**: implement `builtin_http3_session` — take `Value::QuicSession`; call `h3::client::builder().build(quic_conn).await`; store `h3::client::SendRequest` in `Value::Http3Session` (replace `Rc<()>`) (`src/builtins_io.rs`)
- [ ] **quinn step 6 — http-request for HTTP/3**: implement HTTP/3 dispatch in `builtin_http_request` — when `Value::Http3Session`: construct h3 request, `block_on(send_request.send_request(req).await)`, collect response body bytes; return `Ok[{status headers body}] | Err[String]` using nominal Result; the HTTP/2 dispatch branch (reqwest-based) can be implemented similarly (`src/builtins_io.rs`)
- [ ] **quinn step 7 — tests**: corpus tests for quic-session type errors (wrong arg count, wrong cap type); these are the only CI-testable paths since QUIC requires a live server
- [x] Stub builtins registered: quic-session, quic-open-stream, quic-open-datagram, http2-session, http3-session, http-request, icmp-ping — all return clear "not yet implemented" errors (187 total builtins) (`src/builtins_io.rs`, `src/builtins.rs`, `src/type_env.rs`)
- [x] Add `Value::QuicSession`, `Value::Http2Session`, `Value::Http3Session` as `Rc<()>` placeholders (`src/value.rs`)
- [x] Add `Type::QuicSession`, `Type::Http2Session`, `Type::Http3Session` + TypeEnv + value_matches_type (`src/types.rs`, `src/type_env.rs`, `src/eval.rs`)
- [x] Tokio runtime strategy documented in `doc/11a-builtins.md`
- [x] Remove `http-connect` entirely (`src/builtins.rs`, `src/builtins_io.rs`, `src/type_env.rs`)
- [x] Corpus tests: quic_session_stub, http2_session_stub, http_request_stub (`tests/corpus/eval/builtins/`)
**Depends on:** `connect-v2`

### `stdlib-protocols`: net.llt rewrite + protocols/ subdirectory

See `doc/whatif/lib-net-v2.md` §Protocol Library, §Stdlib Layout, §fetch. **Spec chapters:** `doc/03-data-model.md §Sessions`, `doc/11-stdlib.md`.

- [x] Create `stdlib/protocols/` directory
- [x] Write `stdlib/protocols/socks5.llt` — SOCKS5 pure helpers: build-socks5-greeting, build-socks5-connect, parse-socks5-response; 15 corpus tests
- [x] Write `stdlib/protocols/dns.llt` — DNS query helpers: encode-dns-name, build-dns-query, QTYPE constants; 8 corpus tests
- [x] Write `stdlib/protocols/grpc.llt` — gRPC frame encoding: build-grpc-frame, parse-grpc-frame-header; 8 corpus tests
- [x] Write `stdlib/protocols/websocket.llt` — WebSocket frame encoding/decoding + HTTP upgrade handshake; 15 corpus tests
- [ ] Rewrite `stdlib/net.llt` full rewrite (blocked: needs http-sessions for HTTP/2 ALPN negotiation)
- [x] Update `doc/11-stdlib.md` with `protocols/` subdirectory layout and function listings (`doc/11-stdlib.md`)
- [x] Tests: pure-helper corpus tests for build-socks5-*, build-ws-frame, parse-ws-frame-header, ws-handshake (15 tests in `tests/corpus/eval/stdlib/protocols/`)
- [x] Add Rust unit test for `check_net_cap_allowlist` denial path with a restricted allowlist — the allowlist is the primary security enforcement and has zero corpus coverage; add to `src/builtins_io.rs` `#[cfg(test)]` section
**Depends on:** `http-sessions`

## Known Bugs

### ~~`iife-parse`: `[[fn ...] args]` parsed as Dict, not Call~~ (BY DESIGN)

`[[fn [x] body] arg]` is parsed as a 2-element data array, not a function call.
This is intentional: a bracket expression in head position produces data (Priority 7 fallback),
preserving the `[[condition-result] value]` pair pattern used by `cond` and similar constructs
(30+ occurrences in stdlib). Changing the fallback to "call" would break all of these.

IIFEs are largely unnecessary in tinct: fn-body Sequential and document-level Sequential
both provide local bindings natively. For rare inline cases, use `[call [fn [x] body] arg]`.

Documented in `doc/02-syntax.md` §3.3.1a (head-position rule) and §3.3.1b (IIFE patterns).

### ~~`sequential-lazy`: Sequential fn-body bindings are lazy, not eager~~ (FIXED)

Fixed in `sequential-strict` sprint: Sequential named bindings are now forced to WHNF at binding time (strict `let*` semantics). See `doc/09-documents.md` §[SEQ-SCOPE].

### ~~`depth-limit-toml`: `parse-toml-lite` exceeds depth on large TOML files~~ (RESOLVED)

Resolved by removing `MAX_EVAL_DEPTH` in the `sequential-strict` sprint. The recursive
tinct parser in `stdlib/toml-lite.llt` required ~900 depth levels for a 60-line TOML file;
without a depth limit this is no longer a concern.

---



## Known Bugs (Runtime)

### `runtime-bugs`: Runtime, evaluator, and parser correctness fixes

Fixes for runtime stubs, evaluator bugs, parser gaps, disabled tests, and CLI issues.

**stub-builtins** — Registered builtins that always error at runtime:

- [x] `socks5-connect` (`src/builtins_io.rs:3590-3592`) — **remove from registry** (no use case currently; re-add when there is one)
- [x] `proxy-connect` (`src/builtins_io.rs:3597-3599`) — **remove from registry** (same)
- [x] `open` write and append modes — **implement `Writable`/`Appendable` flags and remove legacy string-flag API**: implement `[open cap path Writable]` and `[open cap path Appendable]` per `doc/whatif/completed/lib-supplemental.md` §Streaming File I/O; delete the backward-compat string-mode branch entirely (`src/builtins_io.rs:167-226`); audit all corpus tests and stdlib files for `open ... "r"` calls and migrate to `[open cap path Readable]` (`src/builtins_io.rs`, `stdlib/`, `tests/`)
- [x] `tls-connect` — removed in connect-v2 sprint; replaced by `tls-layer`
- [x] `connect` UDP transport — stub added in connect-v2 sprint ("UDP not yet supported"); full impl requires datagram infrastructure
- [x] `--cap-net` CIDR range entries — `NetCapEntry::Cidr(ipnet::IpNet)`, DNS rebinding mitigation, combined hostname+CIDR validation (`src/main.rs`, `src/value.rs`, `src/builtins_io.rs`, `Cargo.toml`)

**spki-pinning-wrong** — `compute_spki_hash` hashes full DER cert instead of SPKI field; `tls-peer-cert` returns placeholder strings:

- [x] Fix `compute_spki_hash` to extract SPKI field via `x509-parser` before hashing (`src/builtins_io.rs`)
- [x] Implement `tls-peer-cert` subject/issuer/SANs/validity parsing via `x509-parser` (`src/builtins_io.rs`)

**variant-payload-eq** — `=` returns false for Variant values with payloads (`builtins_math.rs:199`):

- [x] Implement recursive payload equality for `Variant` values in the `=` builtin (`src/builtins_math.rs`)

**guard-default-missing** — `default:` annotation not applied on guard failures (`eval.rs:1634`):

- [x] In `ThunkState::Guarded` and the guard-failure path: check if the annotation carries a `default:` value; if so, return that value instead of propagating the error (`src/eval.rs`, `src/eval_materialize.rs`)

**pin-patterns** — `$name` in match arms binds new variable instead of pinning (`parser.rs:3915`):

- [x] Distinguish `$name` (pin — match against variable value) from bare `name` (bind — introduce new variable) in `Pattern::VarRef` via `escaped: bool` field on `Expr::VarRef`; `Pattern::Pin` variant added (`src/parser.rs`, `src/ast.rs`)

**ignored-tests** — Disabled tests masking real gaps:

- [x] `test_typecheck_corpus` re-enabled — reorganized corpus files (moved warning/error tests to appropriate dirs) and removed `#[ignore]`
- [x] `test_tco_tail_recursive_function` — removed `#[ignore]`; updated syntax to modern bare-word (`[if [= n 0] acc [count-down ...]]`); bumped iterations from 10 to 100_000; passes with no depth limit (`src/eval.rs`)

**typecheck-named-arg-gaps** — Remaining named-arg type checking gaps found in typecheck-bugs panel review:

- [x] `Type::Function` `PartialEq` now ignores param names — compares types only (`src/types.rs`)
- [x] Named arg arity overlap check (C-NO-OVERLAP) — positional param indices excluded from named-arg search (`src/typecheck.rs`)
- [x] CALL-MONO named-arg error accumulation — `infer_expr` failures accumulated instead of `?` short-circuit (`src/typecheck.rs`)
- [x] `expand_macros` depth guard changed from `panic!` to `Err(EvalError::resource_limit_exceeded(...))` (`src/expand.rs`)
- [x] Corpus tests: moved `named_arg_wrong_type.llt-eval` and `named_arg_unknown_name.llt-eval` to `eval/errors/`
- [x] Corpus coverage: added `call_poly_named_args.llt-eval` for CALL-POLY path (`tests/corpus/eval/typecheck/`)
- [x] `collect_pattern_bindings` `Or` and `Constructor` unit tests added (`src/typecheck.rs`)

**int-float-precision** — Int→Float promotion silently loses precision for integers > 2^53 (`builtins_math.rs:191`):

- [x] Decide: **error always** when implicit Int→Float promotion would lose precision (|n| > 2^53); consistent with CUE/Nickel/Dhall which require explicit conversion rather than silent precision loss. Provide `[float n]` as the explicit escape hatch — analogous to Rust's `as f64` — that makes the lossy conversion intentional.
- [x] In arithmetic builtins: check `|int_val| > 2^53` before promotion; error with suggestion to use `[float n]` (`src/builtins_math.rs`)
- [x] Add `float` builtin: unconditional Int→Float cast (`src/builtins_math.rs`, `src/builtins.rs`)
- [x] Update `doc/03-data-model.md` §Numeric Types: precision-safe promotion + `[float n]` documented

**e-flag-ordering** — `-e` expressions don't interleave with file arguments (`main.rs:750`):

- [x] Track relative order of file and `-e` arguments via `interleave_files_and_exprs()` helper (`src/main.rs`)

**net-llt-parse-http-response** — `parse-http-response` returns wrong dict for any real HTTP response (one with headers):

- [x] Fix `parse-http-response` Sequential binding bug — extracted into `parse-header-body` helper (`stdlib/net.llt`)
- [x] Replace `builtin-eq`/`builtin-add` with `=`/`+` in net.llt (`stdlib/net.llt`)
- [x] Corpus test for parse-http-response (`tests/corpus/eval/stdlib/net_parse_http_response.llt-eval`)

~~**http-connect-untested**~~ — `http-connect` is **removed** in the `http-sessions` sprint; the reqwest container bug is moot. No investigation needed.

**stale-corpus-warns** — Multiple corpus test files have `=== warn` or `=== error` sections with stale expected strings that no longer match the type checker's output. Grouped by cause:

*Span shifted or message text changed — update the expected substring to match current output:*
- [x] `tests/corpus/eval/builtins/apply.llt-eval` — warn: `cannot unify Fn@? [?] with Fn@Number [? ?]` at wrong span
- [x] `tests/corpus/eval/builtins/seq_empty.llt-eval` — warn: `cannot unify Seq[?] with []` span changed from `1:1-1:13` to `1:10-1:12`
- [x] `tests/corpus/eval/builtins/seq_collect_empty_input.llt-eval` — same span shift as seq_empty
- [x] `tests/corpus/eval/stdlib/slice_float_coercion.llt-eval` — warn: `cannot unify Int with Float` span changed
- [x] `tests/corpus/eval/stdlib/rest_seq.llt-eval` — warn: `cannot unify [] with Seq[?]` span changed
- [x] `tests/corpus/eval/stdlib/cons_seq.llt-eval` — warn: `cannot unify [] with Seq[?]` span changed
- [x] `tests/corpus/eval/laziness/deep_chain_50.llt-eval` — last expected warn line `field 'result' not found in []` no longer produced
- [x] `tests/corpus/eval/functions/fn_kotlin_apply_mixed_keys.llt-eval` (and `_named`, `_positional`, `_reorder`, `_sparse_keys`) — warn: `cannot unify Fn@? [?] with _t0` vs old arity mismatch text
- [x] `tests/corpus/eval/errors/flatten_seq_error.llt-eval` — warn: `cannot unify [] with Seq[?]` span changed
- [x] `tests/corpus/eval/errors/seq_collect_non_seq.llt-eval` — warn: span changed
- [x] `tests/corpus/eval/errors/slice_on_non_dict.llt-eval` — warn: `cannot unify [] with 42` (was `[...]`)
- [x] `tests/corpus/eval/errors/sort_seq_error.llt-eval` — warn: span changed
- [x] `tests/corpus/eval/errors/proxy_access_error_has_context.llt-eval` — warn: `cannot unify Proxy with [test: _t2]` (was `[test: _t2 ...]` with open-row marker)
- [x] `tests/corpus/eval/errors/proxy_handler_error.llt-eval` — same open-row marker removal
- [x] `tests/corpus/eval/errors/input_validation_mismatch.llt-eval` — warn: `expects contract []` (was `expects contract [...]`)
- [x] `tests/corpus/eval/errors/apply_builtin_named_arg.llt-eval` — warn text changed

*Type checker improved — warning no longer produced, remove the `=== warn` section entirely:*
- [x] `tests/corpus/eval/errors/bracket_access_missing_key.llt-eval` — type checker now resolves field access; `field 'b' not found` warning is gone
- [x] `tests/corpus/eval/errors/dot_access_missing_key.llt-eval` — same; `field 'missing' not found` warning gone
- [x] `tests/corpus/eval/errors/no_match_shows_keys.llt-eval` — same; `field 'xyz' not found` warning gone
- [x] `tests/corpus/eval/errors/typo_suggestion.llt-eval` — same; `field 'usernme' not found` warning gone

*Eval behavior changed — update expected output/error:*
- [x] `tests/corpus/eval/errors/closed_record_rejects_extra.llt-eval` — under BAS width subtyping, `@[name: Str]` now accepts dicts with extra fields; TypeAssert no longer rejects them; this test belongs in a "width-subtyping" success test, not errors/
- [x] `tests/corpus/eval/type_assertions/guard_default_extra_field.llt-eval` — expected `Dict({"x": Int(5)})` but BAS now preserves extra fields; output is `Dict({"x": Int(1), "y": Int(2)})`
- [x] `tests/corpus/eval/typecheck/bas_width_subtyping.llt-eval` — output changed after RowTail removal; update expected output to match new format

*Test bugs — fix the corpus test itself:*
- [ ] `tests/corpus/eval/stdlib/get_or_missing.llt-eval` — uses bare `b` (undefined variable) instead of `"b"` (string key); fix to `[get-or [a: 1] "b" 99]` and remove `=== warn` section
- [ ] `tests/corpus/eval/stdlib/encoding_scoping.llt-eval` (and `path_scoping`, `re_scoping`, `strings_scoping`, `toml_scoping`, `math_scoping`) — test intentionally calls undefined functions inside `[try ...]` to verify they're not in scope without `include`; the `=== warn` section is technically expected but these tests belong in `tests/corpus/eval/stdlib/errors/` not the clean stdlib directory; reorganize

*Tinct type checker bugs — causing spurious warnings on correct code:*
- See `typeenv-wrapper-pattern` sprint below for the root cause and fix.
- [ ] ~30 tests in `builtins/` test error conditions (arity, wrong types, stubs) and have `=== error` / `=== warn` sections; these are intentional and correct behavior, but for clarity should be reorganized into `tests/corpus/eval/builtins/errors/` to match the convention that success-path tests have no error/warn sections (`tests/corpus/eval/builtins/`)

**bas-get-seq-unknown** — BAS regression: `[get N seq]` returns `_` (Unknown) instead of the Seq element type. When `seq: Seq[String]`, `[get 0 seq]` should return `String`, but the type checker infers `_`. This causes false T003 errors in functions annotated with a concrete return type that use `get` on a Seq inside an `if` expression: the `if` body becomes e.g. `"" | _` instead of `String`. Observed in `samples/versions.llt:15` (http-body, `cannot unify String with "" | _`) and `samples/versions.llt:57` (dep-name-append, `cannot unify [] with [] | _`). Root cause: either `split` is typed as `Seq` (no element type) in TypeEnv, or `check_get` doesn't propagate the element type for integer-indexed Seq access. Investigate `TypeEnv::with_builtins()` entry for `split` and the `check_get` Seq arm in `src/typecheck.rs`.

- [ ] Diagnose: add a minimal test `[parts: [split "x" "a x b"]]  [result: [get 0 parts]]` and check whether `result` infers `String` or `_` (`src/typecheck.rs`)
- [ ] Fix `split` TypeEnv entry to return `Seq[String]` (if it currently returns bare `Seq`) OR fix `check_get` to return element type `T` when called on `Seq[T]` with an integer key (`src/type_env.rs`, `src/typecheck.rs`)
- [ ] Verify `samples/versions.llt` T003 errors at lines 15 and 57 disappear after fix

**bas-narrowing-union-regression** — BAS RDNF step 1 removed `least_upper_bound` from `infer_if`, replacing it with `normalize_union + simplify_type`. This regresses 5 unit tests: the `if` expression now produces literal union types (`42 | 0`, `"hello" | "world"`) where narrowed concrete types (`Int`, `Str`) were previously inferred. Specifically: `test_narrowing_equality_literal_int`, `test_narrowing_equality_literal_reversed_operands`, `test_narrowing_equality_literal_string`, `test_narrowing_no_false_branch_narrowing`, `test_false_branch_narrowing_int_predicate` (`src/typecheck.rs:8625`, `8637`, `8648`, `8728`, `9777`).

- [ ] Determine whether `simplify_type` should promote `n | m` (two IntLiterals) to `Int` as the RDNF literal-promotion rule intends — if so, the rule may not be firing on if-expression results (`src/types.rs`)
- [ ] Fix the 5 failing narrowing unit tests to pass (`src/typecheck.rs`)

### `typeenv-wrapper-pattern`: Apply private builtin-NAME + public prelude wrapper consistently

**Problem:** `TypeEnv::with_builtins()` is a manual second source of truth for function types. It drifts from reality (e.g. `concat` registered as 1-arg but is 2-arg, `cycle`/`join` typed as `Seq[?]` but callers pass Dict literals). The correct pattern — already used for `map`, `filter`, `reduce`, `take`, `drop`, `+`, `-`, `=`, `<` — is: Rust registers `builtin-NAME`, prelude exports `NAME` with proper type annotations, TypeEnv only has `builtin-NAME` (for prelude's internal use). See `doc/07-type-extensions.md §Boolean-Algebraic Subtyping` for rationale.

**Functions that need Rust rename to `builtin-NAME` + prelude wrapper added:**

Collection/Sequence operations (these are the root cause of Seq vs Dict literal type mismatches):
- [ ] `join` → `builtin-join`; add `join@[doc: "Join elements into string"]: [fn@String [sep@String xs@[Dict Seq]] [builtin-join sep xs]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `concat` → `builtin-concat`; add `concat@[doc: "Concatenate two sequences or dicts"]: [fn [xs ys] [builtin-concat xs ys]]` with correct 2-arg signature — **this is the root cause of the `concat` TypeEnv arity bug** (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `cycle` → `builtin-cycle`; add `cycle@[doc: "Repeat xs infinitely"]: [fn@Seq [xs] [builtin-cycle xs]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `range` → `builtin-range`; add `range@[doc: "Integer range [start end)"]: [fn@Seq [start@Int end@Int] [builtin-range start end]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `repeat` → `builtin-repeat`; add `repeat@[doc: "Repeat value infinitely"]: [fn@Seq [x] [builtin-repeat x]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `iterate` → `builtin-iterate`; add `iterate@[doc: "f(x), f(f(x)), ..."]: [fn@Seq [f@Fn x] [builtin-iterate f x]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `unfold` → `builtin-unfold`; add `unfold@[doc: "Generate sequence from seed"]: [fn@Seq [f@Fn seed] [builtin-unfold f seed]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `collect` → `builtin-collect`; add `collect@[doc: "Materialize lazy Seq to Dict"]: [fn@Dict [xs@Seq] [builtin-collect xs]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `seq` → `builtin-seq`; add `seq@[doc: "Construct a Seq from head and tail"]: [fn@Seq [head tail@Seq] [builtin-seq head tail]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `head` → `builtin-head`; add `head@[doc: "First element of a Seq"]: [fn [xs@Seq] [builtin-head xs]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `tail` → `builtin-tail`; add `tail@[doc: "All but first element of Seq"]: [fn@Seq [xs@Seq] [builtin-tail xs]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `sort` → `builtin-sort`; add `sort@[doc: "Sort a dict by keys"]: [fn@Dict [xs@Dict] [builtin-sort xs]]`; note: prelude already has `sorted`, `sort-by`, `sorted-by` — this is the raw underlying builtin (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `reverse` → `builtin-reverse`; add `reverse@[doc: "Reverse a dict or Seq"]: [fn [xs] [builtin-reverse xs]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `first` → `builtin-first`; add `first@[doc: "First element"]: [fn [xs] [builtin-first xs]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `last` → `builtin-last`; add `last@[doc: "Last element"]: [fn [xs] [builtin-last xs]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `rest` → `builtin-rest`; add `rest@[doc: "All but first element"]: [fn [xs] [builtin-rest xs]]` (`src/builtins.rs`, `stdlib/prelude.llt`)
- [ ] `cons` → `builtin-cons`; add `cons@[doc: "Prepend element"]: [fn [x xs] [builtin-cons x xs]]` (`src/builtins.rs`, `stdlib/prelude.llt`)

**TypeEnv entries to REMOVE from `TypeEnv::with_builtins()` after wrappers are added** (type now comes from prelude annotation):
- [ ] Remove `join`, `concat`, `cycle`, `range`, `repeat`, `iterate`, `unfold`, `collect`, `seq`, `head`, `tail`, `sort`, `reverse`, `first`, `last`, `rest`, `cons` from TypeEnv::with_builtins() — prelude wrappers will drive the type (`src/type_env.rs`)

**TypeEnv entries to KEEP** (prelude uses these internally during its own type-checking, or they are truly primitive):
- `builtin-*` aliases — used by prelude wrappers
- `+`, `-`, `*`, `/`, `=`, `<`, `if` — operators used throughout prelude
- `keys`, `length`, `merge`, `append`, `builtin-get` — dict primitives
- `str`, `split`, `replace`, `upper`, `lower`, `trim`, `str-length`, `str-slice`, `str-contains?`, `starts-with?`, `ends-with?` — string ops used in prelude
- `char-code`, `chr`, `str-chars`, `str-bytes`, `bytes-str`, `bytes*` — char/bytes primitives
- All math: `floor`, `round`, `sqrt`, trig, `pow`, `atan2`, `nan?`, `inf?`, `finite?`
- All bitwise: `band`, `bor`, `bxor`, `shl`, `shr`
- All type predicates: `int?`, `float?`, `str?`, `bool?`, `dict?`, `fn?`, `null?`, `num?`, `bytes?`, `seq?`, `record?`, `map?`
- `error`, `try`, `force`, `eval`, `apply`, `until` — evaluation primitives
- `from-json`, `to-int`, `to-float`, `float`, `type-of`, `llt-repr`, `tag-of`, `variant`, `get?` — conversion/introspection
- All I/O and capability functions — too I/O-domain-specific for prelude wrappers
- All date-time functions — domain-specific
- URI/HTTP session functions — domain-specific
- `include`, `env`, `emit`, `proxy`, `gensym`, `validate` — system/meta functions

**Update all call sites** after renaming:
- [ ] Audit `stdlib/prelude.llt`, `stdlib/net.llt`, `stdlib/io.llt`, `stdlib/toml-lite.llt`, `stdlib/regex.llt` for direct calls to `join`, `concat`, `cycle`, `range`, `repeat`, `iterate`, `unfold`, `collect`, `seq`, `head`, `tail`, `sort`, `reverse`, `first`, `last`, `rest`, `cons` — update to use `builtin-NAME` in private helpers and `NAME` (via the wrapper) in public exports (`stdlib/`)
- [ ] Update all corpus test files that call the renamed functions to ensure they still pass (`tests/corpus/`)
- [ ] Remove `=== warn` sections from `cycle.llt-eval`, `join.llt-eval`, `concat*.llt-eval`, `rest_seq.llt-eval`, `cons_seq.llt-eval` and other stdlib tests once TypeEnv drift is fixed (`tests/corpus/eval/stdlib/`)

## Standard Library

### `stdlib-doc-annotations`: Add `@[doc: "..."]` to all exported stdlib functions

The `doc-annotations` sprint wired up the full infrastructure (DocMap extraction, `:describe` in REPL, LSP hover, `tinct describe` CLI) and seeded 8 functions in `prelude.llt` as examples. The remaining exported functions across all stdlib files have no doc annotations. Only the **last dict** in each multi-expression file is exported — internal helpers in earlier dicts should not be annotated.

Annotation format: `fn-name@[doc: "One-line description"]: [fn ...]` for public entries; param docs go on the `fn` annotation: `[fn@ReturnType [param@[type: T doc: "Description"]] ...]`.

- [x] `stdlib/prelude.llt` — 101 exported functions annotated
- [x] `stdlib/strings.llt` — pad-left, pad-right, str-find, str-reverse
- [x] `stdlib/math.llt` — pi, e, phi, hypot, deg->rad, rad->deg, log-base
- [x] `stdlib/encoding.llt` — hex-encode, hex-decode, base64-encode, base64-decode, mask-apply
- [x] `stdlib/numeric.llt` — UInt8/16/32, Int8/16/32, to-bytes
- [x] `stdlib/path.llt` — basename, dirname, path-join, extension, path-parts
- [x] `stdlib/io.llt` — read-file, read-lines, println, write-file, write-file-atomic, write-line
- [x] `stdlib/net.llt` — http-get, fetch, parse-url, build-http-request, parse-http-response
- [x] `stdlib/datetime.llt` — days-between, timestamp-in-range?
- [x] `stdlib/regex.llt` — re-compile, re-match, re-find, re-findall, re-replace, re-split
- [x] `stdlib/toml-lite.llt` — parse-toml-lite
- [x] `stdlib/macros.llt` — tmpl-transformer
- [x] `stdlib/formatter/compact.llt`, `stdlib/formatter/pretty.llt` — format
- [x] `stdlib/out/*.llt` — json, yaml, csv, toml, env (llt/raw are pipeline-only, no exports)
- [x] Verify `:describe` works — annotations are metadata-only, DocMap extraction infrastructure already tested

### `stdlib-reorganize`: Stdlib File Reorganization

Organizing principles for `stdlib/`:
1. **`prelude.llt`** — universal core, auto-included. General-purpose utilities; no domain-specific functions. Two-dict pattern: private first dict (`-impl`/`-step`/`-check` helpers), public second dict (exported API). Non-prelude modules must never copy prelude exports.
2. **Domain modules** (`strings.llt`, `math.llt`, `encoding.llt`, etc.) — single-topic, explicit `[include %libdir "..."]` required. Depend on prelude (always in scope); two-dict pattern; no prelude duplication.
3. **Pipeline adapters** (`in/`, `out/`, `formatter/`) — thin wrappers for document pipeline stages; not general-purpose libraries.
4. **Protocol libraries** (`protocols/`) — low-level RFC wire format helpers; self-contained; no prelude duplication.

*Document principles:*
- [ ] Add `§Organization` section to `doc/11-stdlib.md` documenting the four principles above and the module taxonomy table (`doc/11-stdlib.md`)

*Move `str-find` to prelude* — toml-lite.llt and strings.llt both define it; consolidate in prelude (same pattern as str-repeat in cycle-196):
- [ ] Move `str-find-impl`, `str-find-check` to `prelude.llt` private dict; add `str-find@[doc: "Find first occurrence of needle in haystack; returns index or -1"]` to public dict; remove from `strings.llt` (`stdlib/prelude.llt`, `stdlib/strings.llt`)

*Export `make-entry` from prelude* — needed by toml-lite.llt and currently inaccessible from the public dict:
- [ ] Add `make-entry@[doc: "Construct a single-entry dict from key and value"]: make-entry` to prelude public dict (re-exports the private helper) (`stdlib/prelude.llt`)

*Clean up `regex.llt`* — single-dict, duplicates prelude, uses `builtin-*` directly:
- [ ] Remove locally-defined `has?`, `has-check`, `>=`, `>` — prelude versions are in scope; replace usages with prelude calls (`stdlib/regex.llt`)
- [ ] Apply two-dict pattern: private first dict with `re-match-impl`, `re-match-try`, `re-match-check`, `re-find-impl`, `re-find-try`, `re-find-check`, `re-findall-impl`, `re-findall-try`, `re-findall-check`, `re-ensure-pattern`; public second dict with `re-compile`, `re-match`, `re-find`, `re-findall`, `re-replace`, `re-split`, `re-escape-replacement` (`stdlib/regex.llt`)
- [ ] Replace `builtin-eq`/`builtin-lt` with `=`/`<` from prelude in regex.llt helpers (`stdlib/regex.llt`)

*Clean up `toml-lite.llt`* — single-dict with 15+ prelude/stdlib duplicates, uses `builtin-*` throughout:
- [ ] Remove locally-defined prelude duplicates: `get-or`, `has?`, `has?-impl`, `not`, `and`, `>=`, `>`, `try-or`, `try-or-impl`, `first`, `all?`, `all?-impl` (12 functions) — use prelude versions (`stdlib/toml-lite.llt`)
- [ ] Remove locally-defined `make-entry` (now exported from prelude) and `str-find`/`str-find-impl`/`str-find-check` (now in prelude) (`stdlib/toml-lite.llt`)
- [ ] Apply two-dict pattern: private first dict for `parse-lines-impl`, `parse-line-dispatch`, `parse-header-body`, `parse-section-name`, `parse-array-table-header`, `parse-kv-split`, `parse-kv-build`, `parse-value-try-int`, `parse-value-try-bool`, `toml-set-at-path-impl`, `toml-set-at-path-final`, `toml-set-at-path-final-check`, `toml-merge-into-last-impl`, `toml-append-array-impl`, `is-array?`, `is-array?-check-keys`, `not-blank-or-comment`; public second dict for `parse-toml-lite`, `parse-toml-lines`, `parse-section-header`, `parse-key-value`, `parse-value`, `toml-set-at-path`, `toml-merge-into-last`, `toml-append-array-table` (`stdlib/toml-lite.llt`)
- [ ] Replace `builtin-if`/`builtin-eq`/`builtin-lt`/`builtin-add`/`builtin-get`/`builtin-reduce` with prelude wrappers `if`/`=`/`<`/`+`/`get`/`reduce` throughout toml-lite.llt (`stdlib/toml-lite.llt`)

*Clean up `path.llt`* — single-dict, internal helpers are accidentally exported, uses `builtin-*`:
- [ ] Apply two-dict pattern: private first dict for `dirname-impl`, `dirname-drop-last`, `extension-impl`; public second dict for `path-parts`, `basename`, `dirname`, `extension`, `path-join` (`stdlib/path.llt`)
- [ ] Replace `builtin-if`/`builtin-eq`/`builtin-sub`/`builtin-add`/`builtin-get` with prelude wrappers `if`/`=`/`-`/`+`/`get` (`stdlib/path.llt`)

*Clean up `macros.llt`* — `tmpl-expand`/`tmpl-emit-call` are in the public dict but are internal helpers:
- [ ] Move `tmpl-expand` and `tmpl-emit-call` from the public second dict to the private first dict; only `tmpl-transformer` and `do-transformer` should be exported (`stdlib/macros.llt`)
- [ ] Replace `builtin-*` calls in `macros.llt` private helpers with prelude wrappers (`if`/`=`/`+`/`get`) where the prelude call is equivalent — the macro expander runs with prelude in scope (`stdlib/macros.llt`)

*Update corpus tests:*
- [ ] Move `tests/corpus/eval/stdlib/encoding_scoping.llt-eval`, `path_scoping.llt-eval`, `re_scoping.llt-eval`, `strings_scoping.llt-eval`, `toml_scoping.llt-eval`, `math_scoping.llt-eval` to `tests/corpus/eval/stdlib/errors/` — scoping-denial tests belong with error tests, not success-path tests (`tests/corpus/eval/stdlib/`)
- [ ] Update `strings_scoping.llt-eval` to remove `str-find` from the "not-in-scope" list once it is in prelude; verify existing `str_find*.llt-eval` tests pass without any changes (`tests/corpus/eval/stdlib/`)
- [ ] `just test` passes after all changes (`tests/`)

## Codebase Health

### cycle-206-findings: Cycle #206 analysis findings

From the 9-agent codebase review on 2026-05-09.

**[Critical] Row occurs check incomplete for Map/Negation** (computer-scientist):
- [x] Row occurs check: added Map/Negation arms to `row_var_occurs_in_type_impl` (`src/type_unify.rs`)
- [x] Dict equality cycle detection: refactored to thread `visited` through recursive value comparisons (`src/builtins_math.rs`)
- [x] Map key variance: fixed unify to use invariance (bidirectional subtype check) matching is_subtype (`src/type_unify.rs`)
- [x] CALL-POLY consumed_params: verified already present from cycle-201-findings
- [x] UnixStream path bypass: added absolute path + `..` traversal validation (`src/builtins_io.rs`)
- [x] Never TypeVar binding: explicit binding in unify (`src/type_unify.rs`)
- [x] Duplicate named args rejection: added detection in all 3 check_call paths (`src/typecheck.rs`)
- [x] doc/15-ast.md TypeAlias: updated to show params field (`doc/15-ast.md`)
- [x] doc/02-syntax.md: removed stale bracket-access whitespace claim (`doc/02-syntax.md`)
- [x] doc/08-evaluation.md: added §Strictness Exceptions subsection (`doc/08-evaluation.md`)

### cycle-201-findings: Cycle #201 analysis findings

From the 9-agent codebase review on 2026-05-09.

**[Major] Duplicate named args bypass Robinson idempotency** (computer-scientist):
- [x] Robinson idempotency: `consumed_params.insert(param_idx)` in CALL-POLY and check_call_with_scheme (`src/typecheck.rs`)
- [x] CALL-POLY substitution threading: merge `state.subst` into local `subst` after `infer_expr` (`src/typecheck.rs`)
- [x] VarRef.escaped audit: field IS used by `expr_to_pattern()` for pin patterns (`Pattern::Pin` vs `Pattern::Variable`); not dead code
- [x] ErrorKind E-code exhaustiveness test added (`src/error.rs::test_error_code_exhaustiveness`)
- [x] doc/15-ast.md: added `escaped: bool` to VarRef entry; `desugared`/`resolved_type` already correct
- [x] Error corpus `[EXXX]` codes: audited and updated (`tests/corpus/eval/errors/`)
- [x] desugar_file: returns `()` (in-place mutation), `#[must_use]` not applicable
- [x] Corpus test count minimums: updated to 37/120/195/123 (`tests/corpus_tests.rs`)

### cycle-196-findings: Cycle #196 analysis findings

From the 9-agent codebase review on 2026-05-09.

- [x] Remove duplicate prelude functions from `stdlib/encoding.llt` (mod, quot, trunc, >=, not, ceil, and, <=)
- [x] Remove `str-repeat` from `stdlib/strings.llt` — keep prelude version only
- [x] Audit `Type::PartialEq` — verified all 26 variants have explicit arms, no issues found
- [x] Verify help_suggestion corpus format — `=== out` + `=== warn` is correct for advisory-warning tests
- [x] ErrorKind constructors — already exist for all new variants
- [x] Substitution maps — already migrated to HashMap in prior sprint
- [x] apply() fast-path — extended with Bytes, Uri, Timestamp, Duration, ClockCap, Timezone, IntLiteral, StringLiteral
- [x] bytes_to_seq — verified no double-wrapping bug exists; current design correct
- [x] doc/11-stdlib.md builtin count — updated from 191 to 178
- [x] Span::origin() frame filter — already implemented in error.rs::should_display_frame

### dep-bumps: Bump direct dependencies to latest

From `just versions` on 2026-05-09. Also: `versions.llt` has a false-positive on Rust toolchain
(compares `"1.95"` != `"1.95.0"` as strings — both are the same release).

- [x] `jiff`: 0.1→0.2 (backward compatible)
- [x] `reqwest`: 0.12→0.13 (feature `rustls-tls`→`rustls`)
- [x] `rustls-native-certs`: 0.7→0.8 (`CertificateResult` struct API change)
- [x] `sha2`: 0.10→0.11 (compatible)
- [x] `sha3`: 0.10→0.11 (compatible)
- [x] `subtle`: 1→2 (compatible)
- [x] `webpki-roots`: 0.26→1.0 (compatible)
