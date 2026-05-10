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

### `hkt-monads`: Research higher-kinded types and generic monadic `[do]` for tinct

The `error-patterns` proposal adopts `[do monad ...]` with explicit monad-dict dispatch as a HKT-free path to monadic composition. The door is left open: when HKT is available, the explicit monad argument becomes optional (inferred from the return type of the first expression), and `[do]` dispatches through a `Monad` typeclass instead of a runtime field access.

Research questions:
- What is the right HKT model for tinct? Full System F-omega? Rank-1 kind polymorphism? Defunctionalization (as in Elm's `elm-program-test` or Oleg Kiselyov's tagless-final)?
- How does HKT interact with tinct's row polymorphism and BAS? Row variables are already kind `Row`; adding type constructors of kind `* → *` extends the kind system non-trivially.
- Can `[do]` remain backward-compatible — monad dict explicit when no Monad instance exists, inferred when one does?
- What is the minimal HKT extension needed to express `Monad m`, `Functor f`, and `Foldable t` without requiring full System F-omega? (Rank-1 kind polymorphism may suffice.)
- Survey: how do ML-family languages (OCaml 5 with effects, F#, SML) and recent functional languages (Koka, Frank, Unison) handle HKT and monadic abstraction?

- [ ] Research HKT and generic monadic do-notation — write proposal to `doc/whatif/hkt-monads.md`

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
- [ ] `tls-connect` Handle form (`src/builtins_io.rs:2451-2454`) — **implement** (needed for STARTTLS and mid-connection TLS upgrades); **blocked on `net-layers` research**: the Handle form is part of the broader layering model; the specific implementation (raw_tcp field in Value::Handle, or a different approach) should be informed by the net-layers design; keep stub until research is complete
- [ ] `connect` UDP transport (`src/builtins_io.rs:612`) — **blocked on `net-layers` research**: UDP is not simply "TCP but unreliable" — QUIC builds session semantics on UDP, DTLS wraps TLS over datagrams; the right answer depends on the composable layer model; keep stub until `net-layers` design is complete
- [ ] `--cap-net` CIDR range entries (`src/main.rs:718-723`) — **implement** with combined hostname+CIDR validation semantics:
  - Add `NetCapEntry::Cidr(ipnet::IpNet)` to `src/value.rs`; parse via `s.parse::<ipnet::IpNet>()`
  - Add `ipnet` as direct dep (`Cargo.toml`; already transitive at `2.12.0`)
  - Support comma-separated entries in one flag: `--cap-net localonly=*.local,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16`
  - **Validation semantics** (at connection time in `src/builtins_io.rs`):
    - IP literal host: allowed iff any CIDR entry contains it
    - Hostname, cap has no CIDR entries: allowed iff hostname matches any Hostname/Glob entry
    - Hostname, cap has CIDR entries: must match a Hostname/Glob entry **and** DNS-resolve to an IP in a CIDR entry — both gates must pass
  - **DNS resolution**: resolve at connection time; connect directly to the resolved IP (not the hostname again) to mitigate DNS rebinding; the resolved IP is also used for TLS SNI separately
  - Example: `--cap-net localonly=*.local,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16` — allows `.local` hostnames only if they resolve to RFC 1918 addresses, plus RFC 1918 IP literals directly
  - (`src/builtins_io.rs`, `src/value.rs`, `src/main.rs`)

**spki-pinning-wrong** — `compute_spki_hash` hashes full DER cert instead of SPKI field; `tls-peer-cert` returns placeholder strings:

- [ ] Fix `compute_spki_hash` to extract the SPKI field from the DER certificate before hashing (use `x509-parser` or `rustls-pki-types` to parse the cert and extract `subject_public_key_info`); the hash must match what `spki-pin` generates (`src/builtins_io.rs`)
- [ ] Implement `tls-peer-cert` subject/issuer parsing — currently returns placeholder strings `"(certificate parsing not yet implemented)"` (`src/builtins_io.rs:3172-3205`)

**variant-payload-eq** — `=` returns false for Variant values with payloads (`builtins_math.rs:199`):

- [ ] Implement recursive payload equality for `Variant` values in the `=` builtin: if both are `Variant { tag, payload: Some(p1) }` and `Variant { tag, payload: Some(p2) }` with matching tags, force and compare `p1` and `p2` recursively (`src/builtins_math.rs:199`)

**guard-default-missing** — `default:` annotation not applied on guard failures (`eval.rs:1634`):

- [ ] In `ThunkState::Guarded` and the guard-failure path: check if the annotation carries a `default:` value; if so, return that value instead of propagating the error (`src/eval.rs`, `src/eval_materialize.rs`)

**pin-patterns** — `$name` in match arms binds new variable instead of pinning (`parser.rs:3915`):

- [ ] Distinguish `$name` (pin — match against variable value) from bare `name` (bind — introduce new variable) in `Pattern::VarRef`; the token kind (`EscapedRef` vs `Identifier`) must be preserved through the pattern-parsing path (`src/parser.rs`)

**ignored-tests** — Disabled tests masking real gaps:

- [ ] `test_typecheck_corpus` (`tests/corpus_tests.rs:271`) is `#[ignore]`d because `get` (a prelude function) lacks a type signature in `TypeEnv`, and `merge`/`+` produce row-polymorphism false positives — add `get`, `merge`, and `+` type signatures to `TypeEnv::with_builtins()` or `build_prelude_env()` so the typecheck corpus can be re-enabled (`src/type_env.rs`, `src/imports.rs`)
- [ ] `test_tco_tail_recursive_function` (`src/eval.rs:8188`) is `#[ignore]`d because full TCO is not wired — PendingCall thunks still accumulate call depth; re-enable once the CEK `Action::Eval` dispatch eliminates depth accumulation (`src/eval.rs`)

**typecheck-named-arg-gaps** — Remaining named-arg type checking gaps found in typecheck-bugs panel review:

- [ ] `Type::Function` `PartialEq` includes param names — `Fn[(Some("x"), Int)]` and `Fn[(None, Int)]` compare unequal even though they are type-equivalent; affects Union deduplication and reflexive short-circuits in `unify`/`is_subtype`. Fix: override `PartialEq` to compare types only for `Function` variant (`src/types.rs:192`)
- [ ] Named arg arity overlap check (C-NO-OVERLAP): type checker does not detect `[call $f positional-arg x: val]` where `x` is param 0 — counts arity correctly but may double-check the same param slot; evaluator rejects at runtime. Fix: after positional zip, exclude consumed param indices from named-arg search (`src/typecheck.rs:2527`)
- [ ] CALL-MONO named-arg `?` short-circuit: `infer_expr(...)?` on line ~2582 returns immediately on first value inference failure instead of accumulating into `errors` (asymmetry with positional multi-error pattern) (`src/typecheck.rs:2582`)
- [ ] `expand_macros` depth guard at `em_depth > 10` uses `panic!` — should return `Err(EvalError::...)` for consistency with `expand_expr` guard (`src/expand.rs:306`)
- [ ] Corpus tests: `named_arg_wrong_type.llt-eval` and `named_arg_unknown_name.llt-eval` are in `eval/typecheck/` but carry `=== error` sections, violating the directory's "passes typecheck" contract; move to `eval/errors/` or make them typecheck-clean (`tests/corpus/eval/typecheck/`)
- [ ] Corpus coverage gap: CALL-POLY and `check_call_with_scheme` named-arg paths have no corpus tests; add two-document corpus tests that exercise each path (`tests/corpus/eval/typecheck/`)
- [ ] `collect_pattern_bindings` `Or` and `Constructor` arms have no unit tests (`src/typecheck.rs:1287-1293`)

**int-float-precision** — Int→Float promotion silently loses precision for integers > 2^53 (`builtins_math.rs:191`):

- [x] Decide: **error always** when implicit Int→Float promotion would lose precision (|n| > 2^53); consistent with CUE/Nickel/Dhall which require explicit conversion rather than silent precision loss. Provide `[float n]` as the explicit escape hatch — analogous to Rust's `as f64` — that makes the lossy conversion intentional.
- [ ] In arithmetic builtins that promote Int to Float (`+`, `-`, `*`, `/` with mixed Int/Float args): check `|int_val| > 9007199254740992` (2^53) before promotion; emit `EvalError::user_error` if exceeded; suggest `[float n]` in the error message (`src/builtins_math.rs:191`)
- [ ] Add `float` builtin: takes an Int or Float, returns Float; for Int, performs the `as f64` cast unconditionally (no precision check — user opted in); for Float, is a no-op (`src/builtins_math.rs`, `src/builtins.rs`)
- [ ] Update `doc/03-data-model.md` §Numeric Types: document that implicit Int→Float promotion errors on precision loss; document `[float n]` as the explicit opt-in conversion

**e-flag-ordering** — `-e` expressions don't interleave with file arguments (`main.rs:750`):

- [ ] Track relative order of file and `-e` arguments in the CLI parser; build the pipeline stage list in declaration order rather than files-then-expressions (`src/main.rs`)

**net-llt-parse-http-response** — `parse-http-response` returns wrong dict for any real HTTP response (one with headers):

- [ ] In `parse-http-response` (`stdlib/net.llt`): extract the else-branch body into a helper function (`parse-header-body`) so the `[hdr-lines: ...]` Sequential binding is in fn-body position, not inside an `if`-branch argument where it is parsed as a dict literal `{hdr-lines: ...}` instead of a let binding; current behaviour: returns `{sections: ..., 0: ...}` instead of `{status: headers: body:}` (`stdlib/net.llt`)
- [ ] Replace remaining `builtin-eq` / `builtin-add` calls in `net.llt` with `=` / `+` while fixing the above (`stdlib/net.llt`)
- [ ] Corpus test: `[fetch cap "http://..."]` returns a dict with `status`, `headers`, `body` keys (`tests/corpus/`)

**http-connect-untested** — `http-connect` + `http-get` (reqwest-based) builtins fail in the container environment with "error sending request"; never had an end-to-end test:

- [ ] Add corpus or CLI integration test that exercises `http-connect` + `http-get` against a local HTTP server (e.g., `python3 -m http.server`) to verify basic request/response (`tests/`)
- [ ] Investigate why reqwest blocking client fails in `rust:1.95` container with `--network=host` while raw `tls-connect` succeeds; likely a CA cert or runtime configuration issue (`src/builtins_io.rs:3465`)

## Standard Library

### `stdlib-doc-annotations`: Add `@[doc: "..."]` to all exported stdlib functions

The `doc-annotations` sprint wired up the full infrastructure (DocMap extraction, `:describe` in REPL, LSP hover, `tinct describe` CLI) and seeded 8 functions in `prelude.llt` as examples. The remaining exported functions across all stdlib files have no doc annotations. Only the **last dict** in each multi-expression file is exported — internal helpers in earlier dicts should not be annotated.

Annotation format: `fn-name@[doc: "One-line description"]: [fn ...]` for public entries; param docs go on the `fn` annotation: `[fn@ReturnType [param@[type: T doc: "Description"]] ...]`.

- [ ] `stdlib/prelude.llt` — annotate all ~190 exported public functions (the last-dict entries); skip `-impl`, `-step`, `-check` helpers in earlier dicts (`stdlib/prelude.llt`)
- [ ] `stdlib/strings.llt` — `pad-left`, `pad-right`, `str-find`, `str-reverse`, `str-repeat` (`stdlib/strings.llt`)
- [ ] `stdlib/math.llt` — `pi`, `e`, `phi`, `hypot`, `deg->rad`, `rad->deg`, `log-base` (`stdlib/math.llt`)
- [ ] `stdlib/encoding.llt` — `base64-encode`, `base64-decode`, `hex-encode`, `hex-decode`, `mask-apply`, `bytes-reverse`, `bytes-repeat` (`stdlib/encoding.llt`)
- [ ] `stdlib/numeric.llt` — all exported numeric utility functions (`stdlib/numeric.llt`)
- [ ] `stdlib/path.llt` — `basename`, `dirname`, `path-join`, `extension`, `path-parts` (`stdlib/path.llt`)
- [ ] `stdlib/io.llt` — `write-line`, `write-file`, `write-file-atomic`, `read-file`, `read-lines` (`stdlib/io.llt`)
- [ ] `stdlib/net.llt` — `http-get`, `fetch`, `parse-url`, `build-http-request`, `parse-http-response` and helpers (`stdlib/net.llt`)
- [ ] `stdlib/datetime.llt` — all exported datetime functions (`stdlib/datetime.llt`)
- [ ] `stdlib/regex.llt` — all exported regex functions (`stdlib/regex.llt`)
- [ ] `stdlib/toml-lite.llt` — `parse-toml-lite` and its exported helpers (`stdlib/toml-lite.llt`)
- [ ] `stdlib/macros.llt` — `tmpl-transformer` and any other exported macros (`stdlib/macros.llt`)
- [ ] `stdlib/formatter/compact.llt`, `stdlib/formatter/pretty.llt` — exported formatting functions (`stdlib/formatter/`)
- [ ] `stdlib/out/*.llt` — the public `json`, `yaml`, `csv`, `toml`, `env`, `llt`, `raw` formatter entry points (`stdlib/out/`)
- [ ] Verify `:describe` and LSP hover return doc strings for all newly annotated functions after each file (`tinct repl`)

## Codebase Health

### dep-bumps: Bump direct dependencies to latest

From `just versions` on 2026-05-09. Also: `versions.llt` has a false-positive on Rust toolchain
(compares `"1.95"` != `"1.95.0"` as strings — both are the same release).

- [ ] `jiff`: `0.1.29` → `0.2.24` — breaking major bump; audit jiff API changes (`Cargo.toml`)
- [ ] `reqwest`: `0.12.28` → `0.13.3` — minor but potentially breaking; check release notes (`Cargo.toml`)
- [ ] `rustls-native-certs`: `0.7.3` → `0.8.3` (`Cargo.toml`)
- [ ] `sha2`: `0.10.9` → `0.11.0` — breaking minor; audit digest API changes in `src/builtins_io.rs` (`Cargo.toml`)
- [ ] `sha3`: `0.10.9` → `0.11.0` — same digest ecosystem breaking change as sha2 (`Cargo.toml`)
- [ ] `subtle`: `1.0.0` → `2.6.1` — breaking major; audit constant-time comparison callers in `src/builtins_io.rs` (`Cargo.toml`)
- [ ] `webpki-roots`: `0.26.11` → `1.0.7` — breaking major; `1.0.7` already in lock as transitive dep, consolidates (`Cargo.toml`)
