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

- [ ] Remove `RowTail::RowVar` from `src/types.rs`; replace row variable representation with BAS Boolean lattice; all `Type::Record(Row)` sites that reference `RowTail::RowVar` must migrate to intersection-based representation (`src/types.rs`)
- [x] Add `Type::Negation(Box<Type>)` variant to `src/types.rs`; updated all match sites (types.rs, type_unify.rs, type_env.rs, eval.rs)
- [x] Add `Type::Never` as explicit bottom type variant; `Type::Top` already existed; updated is_subtype (S-NEVER rule), is_consistent, Display, value_matches_type
- [x] Implement S-RcdTop (disjoint single-field records union = Top) and S-ClsBot (disjoint intersections = Never) in `is_subtype`; `simplify_type` added for basic RDNF groundwork (`src/types.rs`)
- [x] C-Var1/2 constraint rewriting: conservative approximation — unify(concrete, Union([..., TypeVar, ...])) binds TypeVar to concrete (`src/type_unify.rs`)
- [ ] Full RDNF normalization (simplify_type groundwork added; needs constraint solver integration) (`src/typecheck.rs`)
- [ ] Implement multi-field record annotation as intersection: `@[x: T  y: U]` → `{x: T} ∧ {y: U}` (BAS-dependent on RowTail removal) (`src/typecheck.rs`, `src/expand.rs`)
- [x] Add `@[[all A B]]` (intersection) and `@[[without A]]` (negation) annotation syntax (`src/typecheck_annot.rs`)
- [x] False-branch narrowing: `apply_negation_narrowings()` in if-false branch, type predicates narrow to Negation type (`src/typecheck.rs`)
- [x] I-Case3 in infer_match: remaining_scrutinee accumulates negations across arms for precise type narrowing (`src/typecheck.rs`)
- [x] BAS corpus tests: negation narrowing, S-RcdTop, str narrowing, I-Case3 three-arm, S-ClsBot variant match (`tests/corpus/eval/typecheck/bas_*.llt-eval`)
- [ ] Fix `@[[all A B]]` annotation syntax: `[all T1 T2]` parses as Expr::Call not Expr::Dict; dispatch in typecheck_annot.rs needs to handle Call form (`src/typecheck_annot.rs`, `src/parser.rs`)

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
**Depends on:** `bas-core` (for exhaustiveness checking; runtime behavior works without BAS but type checking requires it)

### `record-map-split`: Parameterized Map Type and Dict Union

See `doc/whatif/parameterized-dict.md` and `doc/07-type-extensions.md §Record/Map Split`. **Spec chapters:** `doc/07-type-extensions.md §Record/Map Split and Dict`.

- [x] Add `Type::Map(Box<Type>, Box<Type>)` + is_subtype/unify/apply/Display (`src/types.rs`, `src/type_unify.rs`, `src/type_env.rs`)
- [x] Register `Map` in `TypeEnv::with_builtins()` as `Map[Any Any]` (`src/type_env.rs`)
- [x] Add `get?` builtin: returns value or Null on missing key (`src/builtins_dict.rs`)
- [x] Add `record?` and `map?` predicates (`src/builtins_meta.rs`)
- [x] Implement structural dict equality: order-insensitive key comparison with cycle detection (`src/builtins_math.rs`)
- [ ] Update `check_get` in typecheck for Map[K V] → V|Null (BAS-dependent) (`src/typecheck.rs`)
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
- [ ] Implement `connect cap Udp` — stub: requires Datagram handle infrastructure (Read/Write incompatible with recv_from/send_to)
- [ ] Implement `connect cap UnixDatagram` — stub: same Datagram issue as UDP
- [ ] Implement `connect cap NamedPipe` — stub: Windows-only
- [ ] Implement `connect cap Icmp` — stub: requires raw socket support
- [x] Implement `tls-layer handle sni opts` → Handle with Tls cap; consumes raw_tcp (`src/builtins_io.rs`)
- [x] Remove `tls-connect` entirely — replaced by `tls-layer` (`src/builtins.rs`, `src/builtins_io.rs`, `src/type_env.rs`)
- [x] Register transport variants (UnixStream, UnixDatagram, NamedPipe, Icmp) + Url type alias (`src/builtins.rs`, `src/type_env.rs`)
- [x] Document Connector capability policy in `doc/11a-builtins.md` §Network
- [x] Update `check_net_cap_allowlist` for ICMP (host-only, no port) (`src/builtins_io.rs`)
- [x] Corpus tests: connect arity, UDP stub, UnixStream arity, tls-layer no raw_tcp, NamedPipe stub (`tests/corpus/eval/builtins/`)
- [x] Update `doc/11a-builtins.md` §Network: transport-generic connect, tls-layer, removed Proxy Tunnels

### `http-sessions`: QUIC, HTTP/2, HTTP/3 Sessions

See `doc/whatif/lib-net-v2.md` §Session Protocol. **Spec chapters:** `doc/03-data-model.md §Sessions`.

- [ ] Add `quinn` as direct dependency (`Cargo.toml`); implement `quic-session cap host port opts` → `Value::QuicSession`; opts carries TLS config (CA roots, ALPN, client certs, SPKI pins) (`src/builtins_io.rs`, `Cargo.toml`)
- [x] Add `Value::QuicSession`, `Value::Http2Session`, `Value::Http3Session` as `Rc<()>` placeholders (`src/value.rs`)
- [x] Add `Type::QuicSession`, `Type::Http2Session`, `Type::Http3Session` + TypeEnv + value_matches_type (`src/types.rs`, `src/type_env.rs`, `src/eval.rs`)
- [ ] Document tokio runtime strategy for `quic-session`: use `tokio::runtime::Runtime::new_current_thread().block_on(...)` scoped to the builtin call; must not create a second runtime when `http2-session` (reqwest, which already embeds a runtime) and `quic-session` are both used in the same script — use a single shared `Rc<tokio::runtime::Runtime>` in the builtin context or run quinn on the reqwest runtime
- [ ] Implement `quic-open-stream session` → `Handle[Binary Readable Writable Stream]` from a QuicSession (`src/builtins_io.rs`)
- [ ] Implement `quic-open-datagram session` → `Handle[Binary Readable Writable Datagram]` for RFC 9297 datagram extensions (`src/builtins_io.rs`)
- [ ] Implement `http2-session handle [opts]` → `Value::Http2Session` via reqwest/h2; handle must be `Handle[Stream Tls]` with h2 ALPN (`src/builtins_io.rs`)
- [ ] Implement `http3-session quic-session [opts]` → `Value::Http3Session` via h3/reqwest http3 feature (`src/builtins_io.rs`)
- [ ] Implement `http-request session method path headers body` → `{ok: {status headers body}} | {err: String}`; dispatch on Http2Session and Http3Session (`src/builtins_io.rs`)
- [ ] Implement `icmp-ping cap host timeout-ms` → `{ok: {latency-ms: Int}} | {err: String}`; platform-conditional (`src/builtins_io.rs`)
- [x] Remove `http-connect` entirely — deleted builtin, function, TypeEnv entry, corpus test, test assertion (`src/builtins.rs`, `src/builtins_io.rs`, `src/type_env.rs`)
- [ ] Register all new builtins in `standard_builtins()` (`src/builtins.rs`)
- [ ] Tests: error-path corpus tests for quic-session, http2-session, http-request (arity, type errors) in `tests/corpus/eval/builtins/`
- [ ] Update `doc/11a-builtins.md` §Network with Session builtins
**Depends on:** `connect-v2`

### `stdlib-protocols`: net.llt rewrite + protocols/ subdirectory

See `doc/whatif/lib-net-v2.md` §Protocol Library, §Stdlib Layout, §fetch. **Spec chapters:** `doc/03-data-model.md §Sessions`, `doc/11-stdlib.md`.

- [ ] Create `stdlib/protocols/` directory
- [ ] Write `stdlib/protocols/socks5.llt` — SOCKS5 TCP CONNECT only (RFC 1928 §4 + RFC 1929); no-auth + username/password, IPv4/IPv6/hostname target; signature `[socks5-layer handle cap@NetCap host port creds]` — cap re-validated against tunnel target to prevent SSRF; expose `build-socks5-request`/`parse-socks5-response` as testable pure functions; ~80–120 lines; **requires binary I/O primitives** (`read-bytes`, `byte-at`, `write-bytes`) not yet in stdlib (`stdlib/protocols/socks5.llt`)
- [ ] Write `stdlib/protocols/dns.llt` — DNS wire protocol (RFC 1035): query construction, response parsing, record types A/AAAA/MX/TXT/SRV/CNAME/NS/PTR; compression pointer decompression (RFC 1035 §4.1.4) with loop detection (RFC 9267) and max-depth limit; expose `build-dns-query`/`parse-dns-response`/`encode-dns-name` as testable pure functions; ~200–300 lines; **requires binary I/O primitives** (`read-bytes`, `byte-at`, `write-bytes`) (`stdlib/protocols/dns.llt`)
- [ ] Write `stdlib/protocols/grpc.llt` — gRPC-JSON transcoding: 5-byte frame encoding/decoding, gRPC headers over Http2Session (~40 lines pure tinct); expose `build-grpc-frame`/`parse-grpc-frame` as testable pure functions (`stdlib/protocols/grpc.llt`)
- [ ] Write `stdlib/protocols/websocket.llt` — WebSocket upgrade (RFC 6455): HTTP upgrade handshake, frame encoding/decoding with masking and opcode dispatch, send/receive (~80 lines pure tinct); expose `build-ws-frame`/`parse-ws-frame` as testable pure functions (`stdlib/protocols/websocket.llt`)
- [ ] Rewrite `stdlib/net.llt` — fix `parse-http-response` Sequential binding bug; replace `builtin-*` calls; add `http-connect-layer` (signature: `[http-connect-layer handle cap@NetCap host port headers]` — cap re-validated for tunnel target); rewrite `fetch` to auto-negotiate HTTP/1.1/HTTP/2 via ALPN and HTTP/3 via Alt-Svc cache (first request to new server always HTTP/1.1 or HTTP/2); validate Alt-Svc alternate-origin host against NetCap before upgrade; all I/O functions return Result per `error-patterns.md` (`stdlib/net.llt`)
- [ ] Update `doc/11-stdlib.md` with `protocols/` subdirectory layout and new function listings
- [ ] Tests: corpus tests for entry-point error paths (wrong Handle type, missing cap, arity) in `tests/corpus/eval/errors/`; unit tests for internal pure helpers (`build-socks5-request`, `build-dns-query`, `build-ws-frame`, `build-grpc-frame`) using literal byte input — these are the only CI-testable happy paths since Handle I/O requires a live server; also add `fetch` unsupported-scheme error test (`tests/corpus/eval/errors/fetch_unsupported_scheme.llt-eval`)
- [ ] Add Rust unit test for `check_net_cap_allowlist` denial path with a restricted allowlist — the allowlist is the primary security enforcement and has zero corpus coverage; add to `src/builtins_io.rs` `#[cfg(test)]` section
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
- [ ] `tls-connect` — **entire builtin removed** in `connect-v2` sprint (tracked there)
- [ ] `connect` UDP transport — tracked in `connect-v2` sprint
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
- [ ] `test_tco_tail_recursive_function` (`src/eval.rs:8188`) is `#[ignore]`d because full TCO is not wired — PendingCall thunks still accumulate call depth; re-enable once the CEK `Action::Eval` dispatch eliminates depth accumulation (`src/eval.rs`)

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
