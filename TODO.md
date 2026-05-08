# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Research

### `research-parameterized-dict`

Investigate whether tinct's type system should support a parameterized
`Dict[K V]` type constructor — algebraic type constructors with kind
`Type → Type → Type`. Motivated by the need to type `transitions` in
`stdlib/regex.llt` as `Dict[Int Seq@Int]` (char-code → successor state
ids) rather than the current unparameterized `@Dict` with a runtime
invariant comment.

**The gap:** BAS (`doc/whatif/boolean-algebraic-subtyping.md`) encodes
multi-field records as intersections of single-field types and handles
union/intersection over specific named fields — but cannot express "all
values in this dict are of type T" because that requires universal
quantification over field labels (∀f. {f: T}), which is outside BAS's
scope. The `transitions` and `groups` dicts in `NfaState`/`NfaDict`
(lib-regex.md) are the concrete cases that remain untyped.

**Questions for the research phase:**

- [ ] Survey how comparable languages type parameterized maps: Haskell
  `Map k v`, TypeScript `Record<K, V>`, Nickel's contract-based approach,
  CUE's structural constraints (`{[string]: int}`). Which model fits
  tinct's use cases?
- [ ] Can BAS accommodate a `Dict[K V]` constructor as a primitive
  type constructor (not derived from records)? What interaction does
  `Dict[K V]` have with union/intersection (`Dict[Int Str] | Dict[Str Int]`)?
- [ ] Is `Dict[K V]` the right primitive, or should tinct distinguish
  between structural records (field names known statically) and dynamic
  maps (keys are runtime values)? The current `Dict` conflates both.
- [ ] Identify all stdlib functions whose type signatures benefit from
  `Dict[K V]`: `transitions` in regex NFA, `groups` in NFA, the `stat`
  return dict, `tls-peer-cert` result, `list-dir` entry dict.
- [ ] Write a `doc/whatif/parameterized-dict.md` proposal.

**Depends on:** BAS adoption (`doc/whatif/boolean-algebraic-subtyping.md`),
since the interaction between `Dict[K V]` and union/intersection types
requires the full BAS constraint solver to be sound.

---

## Supplemental Standard Library

Accepted from `doc/whatif/lib-supplemental.md` (2026-05-07).

### `string-utils`: Extended String Utilities

**Spec chapters:** `doc/whatif/lib-supplemental.md` §Extended String Utilities. **Depends on:** `string-view`.

- [x] Implement `starts-with?` Rust builtin: `str::starts_with` on `&source[start..end]`; register in `standard_builtins()` with type `[String|Bytes|Seq] → [String|Bytes|Seq] → Bool` (`src/builtins_string.rs`, `src/types.rs`)
- [x] Implement `ends-with?` Rust builtin: `str::ends_with`; register analogously (`src/builtins_string.rs`, `src/types.rs`)
- [x] Implement `str-chars` Rust builtin (internal): walk `source[start..end].char_indices()`, yield lazy `Seq` of `Value::String` slices per codepoint (`src/builtins_string.rs`)
- [x] Implement `str-slice` Rust builtin: compute byte offsets for char positions, construct `Value::String { source: Rc::clone, start: byte_of(from), end: byte_of(to) }` — O(n) for UTF-8 char walk, O(1) allocation (`src/builtins_string.rs`, `src/types.rs`)
- [x] Add `starts-with?` and `ends-with?` to prelude scope (loaded at startup alongside `prelude.llt`) (`src/builtins.rs`)
- [ ] Add Bytes dual-dispatch for `starts-with?` and `ends-with?` (byte-prefix/suffix match) (`src/builtins_string.rs`)
- [ ] Add Bytes dual-dispatch for `contains?`: byte-pattern search (deferred from `bytes-type` sprint) (`src/builtins.rs`)
- [ ] Add Bytes dual-dispatch for `length`, `get`, `nth`: byte count and byte-index access (deferred from `bytes-type` sprint) (`src/builtins.rs`, `src/builtins_dict.rs`)
- [ ] Add Bytes dual-dispatch for `map`, `filter`, `reduce`, `fold`, `first`, `last`, `take`, `drop`, `slice`, `count`, `reverse`: iterate over byte values as Int (0–255); results are Seq (not Bytes); use `bytes-of` to collect back to Bytes (deferred from `bytes-type` sprint) (`src/builtins_seq_reduce.rs`, `src/builtins.rs`)
- [ ] Add Bytes dual-dispatch for `split`, `replace`, `join`: byte-pattern split/replace, byte-separator join (deferred from `bytes-type` sprint) (`src/builtins_string.rs`)
- [x] Add Seq dual-dispatch for `starts-with?` and `ends-with?` (element-by-element prefix match) (`src/builtins_string.rs`)
- [x] Create `stdlib/strings.llt` with pure-tinct functions: `pad-left`, `pad-right`, `str-repeat`, `str-find`, `str-reverse` (Note: `str-contains?` is already a Rust builtin, `str-repeat` is in prelude but duplicated here per requirements) (`stdlib/strings.llt`)
- [ ] Tests: verify `pad-left` is NOT in scope without `[include "stdlib/strings.llt"]` (should error `undefined variable`); verify it IS available after include; same for `str-find`, `str-reverse` (`tests/corpus/eval/stdlib/`)
- [ ] Tests: verify `basename`, `path-join` are NOT in scope without `[include "stdlib/path.llt"]`; verify available after include (`tests/corpus/eval/stdlib/`)
- [ ] Tests: verify `parse-toml-lite` is NOT in scope without `[include "stdlib/toml-lite.llt"]`; verify available after include (`tests/corpus/eval/stdlib/`)
- [x] Register type signatures for all new builtins (`src/types.rs`)
- [x] Tests: corpus tests for starts-with?/ends-with? on String/Bytes/Seq, str-slice O(1), str-find, pad-left/pad-right alignment (`tests/corpus/eval/builtins/`, `tests/corpus/eval/stdlib/`)

### `math-builtins`: Extended Math Builtins

**Spec chapters:** `doc/whatif/lib-supplemental.md` §Extended Math Builtins. Independent of other sprints.

- [x] Implement 13 math Rust builtins as `f64` method wrappers: `pow` (powf), `sqrt`, `log` (ln), `log2`, `log10`, `exp`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2` (`src/builtins_math.rs`)
- [x] Implement 3 float predicate builtins: `nan?` (is_nan), `inf?` (is_infinite), `finite?` (is_finite) (`src/builtins_math.rs`)
- [x] Register all 16 builtins in `standard_builtins()` with correct `Strictness` (`src/builtins.rs`)
- [x] Register type signatures: `pow: Number → Number → Float`, `sqrt/log/sin: Float → Float`, `nan?/inf?/finite?: Float → Bool`, `atan2: Float → Float → Float` (`src/types.rs`)
- [x] Create `stdlib/math.llt` with Float literals (`pi`, `e`, `phi`) and pure-tinct functions (`hypot`, `deg->rad`, `rad->deg`, `log-base`) (`stdlib/math.llt`)
- [ ] Tests: verify `pi`, `hypot`, `deg->rad` are NOT in scope without `[include "stdlib/math.llt"]`; verify available after include (`tests/corpus/eval/stdlib/`)
- [x] Tests: corpus tests for each builtin (exact values, NaN/Inf edge cases, `nan?`/`inf?`/`finite?` predicates) (`tests/corpus/eval/builtins/`)
- [x] Tests: corpus tests for math.llt pure-tinct functions (`tests/corpus/eval/stdlib/`)

### `bitwise-encoding`: Bitwise Primitives & Encoding

**Spec chapters:** `doc/whatif/lib-supplemental.md` §Bitwise Primitives. Independent of other sprints.

- [x] Implement 5 bitwise Rust builtins: `band` (i64 &), `bor` (|), `bxor` (^), `shl` (<<), `shr` (logical >>; treat as u64 for zero-fill) (`src/builtins_math.rs`)
- [x] Implement `char-code` Rust builtin: first char of String → Int codepoint (`src/builtins_string.rs`)
- [x] Implement `chr` Rust builtin: Int codepoint → single-char String (`src/builtins_string.rs`)
- [x] Register all 9 builtins with type signatures (`src/builtins.rs`, `src/types.rs`)
- [x] Define `HashAlgorithm` type alias as a union of nominal variants: `Sha256 | Sha384 | Sha512 | Sha3-256 | Sha3-384 | Sha3-512 | Blake3` — register in prelude scope (`stdlib/encoding.llt` or `src/builtins.rs`)
- [x] Create `stdlib/encoding.llt` with pure-tinct functions: `base64-encode`, `base64-decode`, `hex-encode`, `hex-decode`, `mask-apply`, `bytes-reverse`, `bytes-repeat` (`stdlib/encoding.llt`)
- [ ] Tests: verify `hex-encode`, `base64-encode` are NOT in scope without `[include "stdlib/encoding.llt"]`; verify available after include (`tests/corpus/eval/stdlib/`)
- [ ] Register `HashAlgorithm` type alias in `TypeEnv::with_builtins()` so type checker recognises `Sha256`, `Sha3-256`, `Blake3`, etc. as `HashAlgorithm` members (deferred from `bitwise-encoding` sprint) (`src/type_env.rs` or `src/types.rs`)
- [x] Tests: corpus tests for all bitwise ops, char-code/chr round-trips, hex-encode/hex-decode, base64 (`tests/corpus/eval/builtins/`, `tests/corpus/eval/stdlib/`)

### `handle-caps`: Capability-Typed Handles & Streaming I/O

**Spec chapters:** `doc/whatif/lib-supplemental.md` §Streaming File I/O. **Depends on:** `string-view`.

- [x] Decide `Handle` capability representation — `HashMap<String, Value>`: cap name → associated data. Boolean caps (Readable, Writable, etc.) → `Value::Null`; protocol caps (Tls, Quic, user-defined) → `Value::Dict` carrying handshake/session metadata. User-extensible: any Connector can attach arbitrary data to custom cap names. `tls-peer-cert h` reads `handle.caps.get("Tls")` directly, no special-casing needed.
- [x] Replace `Value::Handle(Rc<RefCell<Box<dyn BufRead>>>)` with a struct carrying `caps: HashMap<String, Value>` + the inner reader/writer; keep existing `Rc<RefCell<Box<dyn BufRead>>>` as the read-side inner (`src/value.rs`)
- [x] Add `Value::WriteHandle` variant carrying `caps: HashMap<String, Value>` + encoding tag (Text/Binary) + `Box<dyn Write>` (`src/value.rs`)
- [x] Implement `cap-data` Rust builtin: `Handle → String → Value` — reads associated data for a named capability; errors if cap is absent (`src/builtins_io.rs`, `src/types.rs`)
- [x] Implement `has-cap?` Rust builtin: `Handle → String → Bool` — tests whether Handle has a named capability (`src/builtins_io.rs`, `src/types.rs`)
- [ ] Refactor `builtin_open`: accept nominal variant capability flags as trailing args; each `Value::Variant { tag, payload }` arg inserts `(tag, payload.unwrap_or(Value::Null))` into the caps HashMap; require at least one arg (no-flag = error); derive encoding (Text/Binary) and direction (read/write) from cap presence (deferred — existing open works, caps HashMap is populated) (`src/builtins_io.rs`)
- [ ] `open` returns `Value::Handle` when `Readable` is in caps, `Value::WriteHandle` when `Writable` (but not `Readable`) is in caps; both carry the full caps HashMap (deferred — requires Variant-based open refactor) (`src/builtins_io.rs`)
- [x] Implement `write` Rust builtin: polymorphic on WriteHandle encoding — `String` arg for Text, `Bytes` arg for Binary; returns WriteHandle for chaining (`src/builtins_io.rs`)
- [x] Implement `flush` Rust builtin: flushes WriteHandle buffer; returns WriteHandle (`src/builtins_io.rs`)
- [x] Implement `close` Rust builtin: flushes and closes WriteHandle; returns null; further writes error (`src/builtins_io.rs`)
- [ ] Implement `seek` Rust builtin: requires Seekable flag; `lseek` to offset; returns Handle (deferred — inner Box<dyn BufRead> can't Seek without downcast) (`src/builtins_io.rs`)
- [ ] Implement `seek-end` Rust builtin: requires Seekable; seek to end (deferred — inner Box<dyn BufRead> can't Seek without downcast) (`src/builtins_io.rs`)
- [ ] Implement `position` Rust builtin: requires Seekable; returns current byte offset as Int (deferred — inner Box<dyn BufRead> can't Seek without downcast) (`src/builtins_io.rs`)
- [x] Update `builtin_slurp`: dispatch on Handle encoding — Text → String, Binary → Bytes (`src/builtins_io.rs`)
- [x] Update `builtin_lines`: require Text encoding flag; error on Binary handles (`src/builtins_io.rs`)
- [x] Update `builtin_connect`: return `Handle` with caps HashMap `{"Binary": Null, "Readable": Null, "Writable": Null, "Stream": Null}` (no Seekable — streams are sequential); network protocol layers (Tls) insert their cap with `Value::Dict` metadata during handshake (`src/builtins_io.rs`)
- [x] Register type signatures for all new builtins (`src/types.rs`)
- [ ] Update `stdlib/io.llt`: add `write-line`; extend `write-file`/`write-file-atomic` to accept `content@[String Bytes]`; remove old `open` mode-string wrappers (`stdlib/io.llt`)
- [ ] Tests: corpus tests for open with flags (Readable, Writable, Binary), write + slurp round-trip, seek + position, close-then-write error, encoding mismatch error (`tests/corpus/eval/builtins/`)

### `fscap-protocol`: FsCap Protocol & DirCap Extension

**Spec chapters:** `doc/whatif/lib-supplemental.md` §Filesystem Capabilities. **Depends on:** `handle-caps`.

- [ ] Implement `list-dir` Rust builtin: DirCap → String → `Seq` of `{name type size mtime}` dicts; use `cap_std::fs::Dir::entries()` (`src/builtins_io.rs`)
- [ ] Implement `stat` Rust builtin: DirCap → String → full metadata Dict (all fields; null for unavailable) (`src/builtins_io.rs`)
- [ ] Implement `make-dir` Rust builtin: DirCap → String → null; `mkdir -p` semantics via `Dir::create_dir_all()` (`src/builtins_io.rs`)
- [ ] Implement `remove` Rust builtin: DirCap → String → null; unlink file or empty dir (`src/builtins_io.rs`)
- [ ] Implement `rename` Rust builtin: DirCap → String → String → null; atomic via `Dir::rename()` (`src/builtins_io.rs`)
- [ ] Implement `copy` Rust builtin: DirCap → String → String → null; efficient via `copy_file_range` where available (`src/builtins_io.rs`)
- [ ] Implement `link` Rust builtin: DirCap → String → String → null; hard link via `Dir::hard_link()` (`src/builtins_io.rs`)
- [ ] Implement `read-link` Rust builtin: DirCap → String → String; readlink target (`src/builtins_io.rs`)
- [ ] Add `Value::Uri { scheme: String, uri: String }` to Value enum (`src/value.rs`)
- [ ] Add `Type::Uri` to Type enum (`src/types.rs`)
- [ ] Update `--cap-fs` CLI parsing: detect URI scheme; `file://` → DirCap; other schemes → `Value::Uri` (`src/main.rs`)
- [ ] Register type signatures for all 8 DirCap builtins (`src/types.rs`)
- [ ] Tests: corpus tests for list-dir, stat, make-dir + remove round-trip, rename atomic, copy, --cap-fs with file:// and s3:// URIs (`tests/corpus/eval/builtins/`, `tests/cli_tests.rs`)
- [ ] Spec: update `doc/03-data-model.md` §Value Types for Uri; update `doc/12-tooling.md` §CLI for --cap-fs URI

## Date-Time

Accepted from `doc/whatif/lib-datetime.md` (2026-05-07).

### `datetime`: Date-Time Support

**Spec chapters:** `doc/whatif/lib-datetime.md`. **Depends on:** `string-utils` (pad-left for format-date).

- [ ] Add `jiff = { version = "0.1", default-features = false }` to `Cargo.toml`; disable `jiff-tzdb` (we read system zoneinfo via DirCap) (`Cargo.toml`)
- [ ] Add `Value::Timestamp(i64)` (nanoseconds since epoch) to Value enum (`src/value.rs`)
- [ ] Add `Value::Duration(i64)` (signed nanoseconds) to Value enum (`src/value.rs`)
- [ ] Add `Value::ClockCap(Rc<ClockCapInner>)` with `enum ClockCapInner { Real, Fixed(i64) }` (`src/value.rs`)
- [ ] Add `Value::Timezone(Rc<...>)` wrapping parsed TZ rules (`src/value.rs`)
- [ ] Add `Type::Timestamp`, `Type::Duration`, `Type::ClockCap`, `Type::Timezone` (`src/types.rs`)
- [ ] Implement timestamp construction builtins: `parse-timestamp` (RFC 3339 → Timestamp; validate fits i64 range), `format-timestamp` (Timestamp → RFC 3339 string), `timestamp->unix`, `unix->timestamp` (`src/builtins_datetime.rs`)
- [ ] Implement clock builtins: `now` (ClockCap → Timestamp), `fixed-clock` (Timestamp → ClockCap) (`src/builtins_datetime.rs`)
- [ ] Implement timestamp arithmetic: `timestamp-add` (Timestamp + Duration → Timestamp), `timestamp-diff` (Timestamp - Timestamp → Duration; `i64::checked_sub`, error on overflow) (`src/builtins_datetime.rs`)
- [ ] Implement timestamp comparison: `timestamp<?`, `timestamp>?`, `timestamp=?` (`src/builtins_datetime.rs`)
- [ ] Implement timestamp extraction (UTC): `timestamp-year`, `timestamp-month`, `timestamp-day`, `timestamp-hour`, `timestamp-minute`, `timestamp-second`, `timestamp-parts` (`src/builtins_datetime.rs`)
- [ ] Implement duration constructors: `duration-nanos`, `duration-seconds`, `duration-minutes`, `duration-hours`, `duration-days`, `duration->seconds`, `duration->nanos` (`src/builtins_datetime.rs`)
- [ ] Implement timezone builtins: `load-tz` (DirCap → String → Timezone; use jiff `TimeZoneDatabase::from_dir()`; return error never panic on malformed TZif), `timestamp-in-tz` (Timestamp → Timezone → Dict), `local->timestamp`, `local-tz-name` (`src/builtins_datetime.rs`)
- [ ] Add CLI flags: `--cap-clock NAME` (inject real ClockCap), `--cap-clock-fixed "RFC3339" NAME` (inject fixed; validate fits i64 range) (`src/main.rs`)
- [ ] Create `stdlib/datetime.llt` with pure-tinct helpers: `days-between`, `timestamp-in-range?`, `format-date` (`stdlib/datetime.llt`)
- [ ] JSON serialization: Timestamp → RFC 3339 string; Duration → ISO 8601 string (`src/lib.rs`)
- [ ] Register all builtin type signatures (~25 builtins) (`src/types.rs`)
- [ ] Tests: corpus tests for parse/format round-trip, arithmetic, extraction, comparison, ClockCap fixed vs real, timezone conversion, i64 overflow error, RFC 5280 sentinel clamping (`tests/corpus/eval/builtins/`)
- [ ] Tests: CLI tests for --cap-clock and --cap-clock-fixed (`tests/cli_tests.rs`)

## Regex

Accepted from `doc/whatif/lib-regex.md` (2026-05-07).

### `regex`: Pure-Tinct Regex Engine

**Spec chapters:** `doc/whatif/lib-regex.md`. **Depends on:** `string-view` (string dual-dispatch for fold), `bitwise-encoding` (char-code).

- [ ] Implement regex parser in pure-tinct: pattern string → AST dict (recursive descent over chars; handles `.`, `*`, `+`, `?`, `{n,m}`, `[a-z]`, `[^...]`, `|`, `()`, `(?P<name>...)`, `^`, `$`) (`stdlib/regex.llt`)
- [ ] Implement Thompson NFA compiler in pure-tinct: AST → NFA dict via `nfa-char`, `nfa-concat`, `nfa-alt`, `nfa-star`, `nfa-plus`, `nfa-opt`, `nfa-repeat`, `nfa-group` (`stdlib/regex.llt`)
- [ ] Implement NFA simulator in pure-tinct: `nfa-step` (advance active states on one char via char-code transition lookup + epsilon closure), `nfa-run` (fold over input string), `nfa-accepts` (`stdlib/regex.llt`)
- [ ] Implement character class matching: ranges via `char-code` comparison (`[a-z]` = code 97-122); negated classes (`[^...]`); predefined `\d`, `\w`, `\s` (`stdlib/regex.llt`)
- [ ] Implement capture group tracking: `nfa-run` carries `Dict[group-id → [start end]]` snapshots through active state set (`stdlib/regex.llt`)
- [ ] Implement `re-compile`: parse + compile → `Value::Variant { tag: "Pattern", payload: nfa-dict }` (`stdlib/regex.llt`, register Pattern variant)
- [ ] Implement `re-match`: `[String|Pattern] → String → Bool`; compile if String; simulate NFA (`stdlib/regex.llt`)
- [ ] Implement `re-find`: `[String|Pattern] → String → MatchResult | []`; return first match with `match`/`start`/`end` + named capture group keys (`stdlib/regex.llt`)
- [ ] Implement `re-findall`: `[String|Pattern] → String → Seq@MatchResult`; all non-overlapping matches (`stdlib/regex.llt`)
- [ ] Implement `re-replace`: `[String|Pattern] → String → String → String`; back-references `\1`, `\2`, `\k<name>`, `\0` in replacement string (`stdlib/regex.llt`)
- [ ] Implement `re-split`: `[String|Pattern] → String → Seq@String`; zero-length match policy: skip at boundary of previous match (Python 3.7+ semantics) (`stdlib/regex.llt`)
- [ ] Implement `re-escape-replacement`: escape backslashes in replacement strings to prevent injection (`stdlib/regex.llt`)
- [ ] Tests: verify `re-compile`, `re-match` are NOT in scope without `[include "stdlib/regex.llt"]`; verify available after include (`tests/corpus/eval/stdlib/`)
- [ ] Tests: corpus tests for literal matching, character classes, quantifiers, alternation, anchors, groups, named groups, re-find/findall/replace/split, zero-length match edge cases, re-escape-replacement (`tests/corpus/eval/stdlib/`)

## TLS & HTTP

Accepted from `doc/whatif/lib-tls.md` (2026-05-07).

### `connector-tls`: Connector Protocol & TLS

**Spec chapters:** `doc/whatif/lib-tls.md` §Connector Protocol, §Handle Types, §tls-connect, §CA Root Selection, §Client Certificates, §SPKI Pins. **Depends on:** `handle-caps`, `bytes-type`, `bitwise-encoding`.

- [ ] Add `rustls = "0.23"`, `rustls-native-certs = "0.7"`, `webpki-roots = "0.26"`, `sha3 = "0.10"` to `Cargo.toml` (`Cargo.toml`)
- [ ] Define `Transport` nominal variants: `Tcp`, `Udp` as unit variants registered in prelude (`src/builtins.rs`)
- [ ] Generalize `builtin_connect`: accept any Connector (dispatch on value type: `NetCap` → built-in TCP/UDP; user dict → call dict's `connect` method); accept `Transport` variant as arg (Tcp default when omitted); return `Handle` with `{Binary Readable Writable Stream}` for Tcp, `{Binary Readable Writable Datagram}` for Udp (`src/builtins_io.rs`)
- [ ] Implement `tls-connect` Rust builtin — Connector form: `tls-connect connector Transport host port opts` opens via connect then layers TLS via `rustls::ClientConfig`; Handle form: `tls-connect handle sni opts` layers TLS on existing stream Handle; both return `Handle[Binary Readable Writable Stream Tls]` with `TlsInfo` (`src/builtins_io.rs`)
- [ ] Implement CA root loading: default = `rustls-native-certs` system roots; `ca-bundle` Handle → read PEM, add to root store; `no-system-roots: true` → drop system roots; `mozilla-roots: true` → add `webpki-roots` (`src/builtins_io.rs`)
- [ ] Implement mTLS: read `client-cert` and `client-key` Handle PEM bytes; configure `ClientConfig` with client auth (`src/builtins_io.rs`)
- [ ] Implement `SpkiPin` type: `spki-pin` builtin takes `HashAlgorithm` variant + `Bytes` fingerprint → dict; `pins` option in TLS opts validates leaf cert SPKI against pin list using specified hash algorithm (`src/builtins_io.rs`)
- [ ] Implement SPKI hash computation for pin matching: SHA-256/384/512 via `ring`, SHA3-256/384/512 via `sha3`, BLAKE3 via `blake3` (`src/builtins_io.rs`)
- [ ] Implement ALPN: `alpn` option → `ClientConfig::alpn_protocols`; default `["http/1.1"]` (`src/builtins_io.rs`)
- [ ] Implement `tls-peer-cert` Rust builtin: requires Handle with `Tls` flag; return dict with `subject`, `issuer`, `sans`, `not-before` (@Timestamp), `not-after` (@Timestamp), `spki-sha256` (`src/builtins_io.rs`)
- [ ] Add `--cap-net NAME=ENTRY` CLI documentation for Connector protocol context (`doc/12-tooling.md`)
- [ ] Register all type signatures (`src/types.rs`)
- [ ] Tests: corpus tests for connect Tcp/Udp, tls-connect with system roots, tls-connect with custom CA, mTLS, certificate pinning (SpkiPin), ALPN negotiation, tls-peer-cert return shape (`tests/corpus/eval/builtins/`)

### `http-net`: HTTP Client & Network Stack

**Spec chapters:** `doc/whatif/lib-tls.md` §HttpConn, §HTTP/2 HTTP/3, §Network Stack Summary. **Depends on:** `connector-tls`, `string-utils`.

- [ ] Add `reqwest = { version = "0.12", features = ["http2", "http3", "brotli", "rustls-tls"] }` to `Cargo.toml` (`Cargo.toml`)
- [ ] Implement pure-tinct `http-get` in `stdlib/net.llt`: connect → write HTTP/1.0 request → slurp → parse response (`stdlib/net.llt`)
- [ ] Implement pure-tinct `https-get` in `stdlib/net.llt`: tls-connect → write → slurp → parse (`stdlib/net.llt`)
- [ ] Implement pure-tinct `fetch` in `stdlib/net.llt`: parse URL, dispatch on `starts-with? "https://" url` (`stdlib/net.llt`)
- [ ] Implement pure-tinct `build-http-request` helper: construct HTTP/1.0 GET request with headers (`stdlib/net.llt`)
- [ ] Implement pure-tinct `parse-http-response` helper: split status line / headers / body (`stdlib/net.llt`)
- [ ] Implement pure-tinct `parse-url` helper: extract scheme, host, port, path (`stdlib/net.llt`)
- [ ] Add `Value::HttpConn` to Value enum (wraps reqwest Client or connection pool) (`src/value.rs`)
- [ ] Implement `http-connect` Rust builtin — Connector form: `http-connect connector host port opts` → `HttpConn`; Handle form: `http-connect handle host` → `HttpConn`; internally opens Tcp (HTTP/1.1+2) or Udp (HTTP/3) based on ALPN (`src/builtins_io.rs`)
- [ ] Implement `http-get` overload on `HttpConn`: `http-get conn path headers` → response Dict (`src/builtins_io.rs`)
- [ ] Implement `socks5-connect` Rust builtin: SOCKS5 tunnel; takes Handle + host + port + creds → Handle[Stream RW] (`src/builtins_io.rs`)
- [ ] Implement `proxy-connect` Rust builtin: HTTP CONNECT tunnel; takes Handle + host + port → Handle[Stream RW] (`src/builtins_io.rs`)
- [ ] Register all type signatures (`src/types.rs`)
- [ ] Tests: corpus tests for pure-tinct http-get against a local test HTTP server, fetch URL dispatch, HttpConn connection reuse, proxy tunneling (`tests/corpus/eval/builtins/`, integration tests)

---

## Phase D: Advanced Typing

### `type-classes-full`

See doc/06-type-inference.md §Type Classes, doc/07-type-extensions.md. **Depends on:** `type-classes-constrained` (B4), `param-type-aliases` (B3), let-generalization complete. **Note:** multi-parameter type classes and functional dependencies are explicitly out of scope for this sprint.

**Parsing and AST:**
- [ ] Verify `[class [ClassName params] superclasses... methods...]` parser against spec syntax; add `class` and `instance` to keyword denylist if not already present (`src/lexer.rs`, `src/parser.rs`)
- [ ] Verify `[instance [ClassName Type] methods...]` parser; method entries may be signature-only or signature+body (default implementations) (`src/parser.rs`)
- [ ] Formatter: round-trip `Expr::ClassDecl` and `Expr::InstanceDecl` without losing method bodies (`src/formatter.rs`)

**Kind system:**
- [ ] `Kind::Var(u32)` variant for kind variables; `KindState` analogous to `InferState` for kind unification (`src/types.rs`)
- [ ] `unify_kind(k1: &Kind, k2: &Kind, state: &mut KindState) -> Result<(), KindError>` — Robinson unification on `Kind` terms (`src/types.rs`)
- [ ] Kind inference for class type parameters from method signatures: infer kind of `f` in `Mappable f` from how `f` is used in `f a` in method types (`src/typecheck.rs`)
- [ ] Kind checking at instance declaration: instance type's kind must match the class parameter's inferred kind; `[instance [Mappable Int] ...]` is a kind error (Int has kind `*`, Mappable expects `* → *`) (`src/typecheck.rs`)
- [ ] Kind defaulting: unresolved kind variables default to `Kind::Type` after class declaration is processed (Jones 1993, §4) (`src/typecheck.rs`)

**Class/instance registration:**
- [ ] `ClassEnv` population from `Expr::ClassDecl`: register class with methods (signature + optional default body) and superclasses; compute superclass transitive closure at registration time (`src/typecheck.rs`)
- [ ] `InstanceEnv` population from `Expr::InstanceDecl`: replace string-key lookup with unification-based instance resolution — attempt `unify(instance_head_type, target_type)` to select matching instance (Hall et al. 1996, §3.2) (`src/typecheck.rs`, `src/types.rs`)
- [ ] Instance coherence: reject overlapping instances for the same class+type pair globally — `InstanceEnv::insert` must be global, not dict-scoped (`src/typecheck.rs`)
- [ ] Scoping: class declarations are dict-scoped (visible in the dict and children); instance declarations are globally registered in `InstanceEnv` (coherence requires global uniqueness) (`src/typecheck.rs`)

**Dictionary construction and passing:**
- [ ] Dictionary value construction: `Value::Dict` with method name as key, eagerly materialized at instance registration time; superclass dictionary embedded as a sub-dict under the superclass name (`src/eval.rs`)
- [ ] Superclass dictionary embedding: `Comparable` dict contains `Equatable` sub-dict under key `"equatable"`; `entailment(context, target)` extracts sub-dict when only a superclass dict is available (`src/eval.rs`)
- [ ] Dictionary threading in evaluator: constrained function calls receive implicit dictionary argument; `eval` for call nodes looks up the appropriate dict from `InstanceEnv` and prepends it to args (`src/eval.rs`)
- [ ] Ensure dictionary values are materialized (not thunked) when passed to constrained functions — dicts must not be re-forced on every method call (`src/eval.rs`)
- [ ] Default method implementations: at instance construction time, methods absent from the instance declaration are filled in from `ClassDecl.default_methods` before building the dict (`src/eval.rs`)

**Type inference integration:**
- [ ] Constraint entailment: `entails(context: &[Constraint], target: &Constraint) -> bool` using superclass transitive closure — `Comparable a` entails `Equatable a` if `Equatable` is a superclass of `Comparable` (`src/typecheck.rs`)
- [ ] Constraint simplification during generalization: remove redundant constraints (if `Comparable a` is present, remove `Equatable a`) (`src/typecheck.rs`)
- [ ] Instance resolution during constraint solving: when a type variable is unified with a concrete type, resolve pending class constraints against `InstanceEnv`; error if no matching instance (`src/typecheck.rs`)
- [ ] Integration with B4 constrained type variables: B4's hardcoded instance sets (`Equatable`, `Numeric`, etc.) become backed by actual `ClassEnv`/`InstanceEnv` entries registered at startup (`src/typecheck.rs`, `src/builtins.rs`)

**Testing (25+ tests):**
- [ ] Tests: class declaration parsing/round-trip; instance declaration parsing; dictionary construction and method dispatch; superclass hierarchy and entailment; kind checking at instance sites; constraint propagation through let-generalization; missing instance error; kind mismatch error; overlapping instance error; integration with B4 constrained vars; higher-kinded `Mappable Seq` instance; default method implementations (`tests/corpus/eval/type_system/`)

**Spec:**
- [ ] Write `doc/06-type-inference.md` §Type Classes with formal rules: constraint generation, entailment checking, dictionary elaboration, instance resolution, superclass extraction (`doc/06-type-inference.md`)

---

## Standard Library

### `stdlib-modernize`

Modernize all 14 stdlib `.llt` files to leverage the typing cluster facilities implemented 2026-05-07. Three orthogonal improvements applied uniformly: (1) **encapsulation** — internal helpers scoped to a private first-document, public API in the final document; (2) **pattern matching** — replace `type-of` string comparisons and `try`-result `[first [keys r]]` checks with `[match ...]`; (3) **full type annotations** — every public function carries `fn@ReturnType` and every parameter carries `@Type`. **Applies to:** `prelude.llt`, `numeric.llt`, `formatter/compact.llt`, `formatter/pretty.llt`, `out/json.llt`, `out/json-pretty.llt`, `out/yaml.llt`, `out/csv.llt`, `out/toml.llt`, `out/env.llt`, `out/raw.llt`, `in/json.llt`, `io.llt`, `net.llt`.

**Architecture: public/private encapsulation pattern**

`eval_document` evaluates each expression in a document sequentially: intermediate dicts are materialized and their entries become bindings in a child environment for the next expression; only the **last expression's value is returned**. This gives true encapsulation with no special syntax — internal helpers are simply a separate earlier dict in the same document:

```tinct
# Internal helpers — in scope for the public dict, but NOT returned
[
    make-entry: [fn@Dict [k v] [$k: v]]
    any?-impl:  [fn@Bool [pred@Fn xs@Dict ks i@Int len@Int] ...]
    # ... all -impl / -step / -check helpers
]
# Public API — last expression, so this is the only value returned/exported
[
    any?: [fn@Bool [pred@Fn xs@Dict]
        [builtin-if [seq? xs]
            [error "any?: expected Dict, got Seq"]
            [any?-impl pred xs [keys xs] 0 [length xs]]]]
    # ... all public functions; reference helpers by plain name
]
```

Helpers are reachable by plain name inside the public dict (via the child env chain). They do not appear in the returned dict and are not exported. No `%.fn` or `_impl.fn` prefixes needed.

**Files with no internal helpers** (`numeric.llt` and most `out/` files) remain as a single dict — no split needed.

**Pattern: `try` result dispatch**

Three sites in `prelude.llt` use `[builtin-eq [first [keys result]] "ok"]` to inspect `try` outcomes. Replace all with structural pattern matching:

```tinct
# Before
try-or-impl: [fn [try-result@Dict default]
    [builtin-if [builtin-eq [first [keys try-result]] "ok"]
        try-result.ok
        default]]

# After
try-or-impl: [fn [try-result@Dict default]
    [match try-result
        [ok: v]  v
        [err: _] default]]
```

Apply to: `has?-impl`, `try-or-impl`, `find-deep-try-check`.

**Pattern: `type-of` string comparison → predicate or `[match]`**

Replace `[builtin-eq [type-of x] "Dict"]` with `[dict? x]` where only a boolean is needed, or with `[match x Dict ... _ ...]` where the dispatch selects between typed branches:

```tinct
# Before (walk)
walk: [fn [f@Fn xs]
    [builtin-if [builtin-eq [type-of xs] "Dict"]
        [f [walk-dict f xs]]
        [f xs]]]

# After
walk: [fn [f@Fn xs]
    [match xs
        Dict [f [%.walk-dict f xs]]
        _    [f xs]]]
```

Apply to: `walk`, `flatten-step`, `deep-merge-step`, `find-deep-check`.

**Pattern: string-literal dispatch in formatters**

The formatter files use `[cond [[= [get "type" node] "literal"] ...] ...]` chains. Replace with `[match [get "type" node] "literal" ... "var" ... _ [error ...]]`:

```tinct
# Before (compact.llt format-node)
format-node: [fn [node]
  [cond [
    [[= [get "type" node] "literal"] [format-literal node]]
    [[= [get "type" node] "var"]     [get "name" node]]
    ...
  ]]]

# After
format-node: [fn [node]
  [match [get "type" node]
    "literal"        [format-literal node]
    "var"            [get "name" node]
    ...
    _                [error [str "unknown node type: " [get "type" node]]]]]
```

Apply to: `format-node` and `format-literal` in `compact.llt` and analogous dispatches in `pretty.llt`.

**Tasks — `prelude.llt`:**

- [ ] Public/private split: move all `-impl`, `-step`, `-check` helpers (≈30 functions) into a first dict in the same document; move all public functions into a second (final) dict; helpers are reachable by plain name from the public dict and are not exported (`stdlib/prelude.llt`)
- [ ] `try` result pattern matching: rewrite `has?-impl`, `try-or-impl`, `find-deep-try-check` using `[match result [ok: v] ... [err: _] ...]` (`stdlib/prelude.llt`)
- [ ] `type-of` → predicate/match: rewrite `walk`, `flatten-step`, `deep-merge-step`, `find-deep-check` to use `[dict? x]` / `[match x Dict ...]` instead of `[builtin-eq [type-of x] "Dict"]` (`stdlib/prelude.llt`)
- [ ] Union type annotations for dual-dispatch parameters: add `@[Dict Seq]` to `sorted`, `sorted-by`, `zip`, `contains?`, `flat-map`, `partition`, `group-by`, `fold`, `map` (wrapper), `reduce` (wrapper) (`stdlib/prelude.llt`)
- [ ] Complete annotation pass: add missing `fn@ReturnType` and `param@Type` annotations to `find-deep` family (return type missing), `walk` (return type missing), `get-in`/`get-in-or` (return `@Any`), `zip`/`zip-seq-impl` (unannotated), `cond`/`when`/`unless` (return `@Any`) (`stdlib/prelude.llt`)
- [ ] `sign` → match: replace nested `builtin-if` chain in `sign` with a `[match ...]` expression using a literal 0 arm and guard arms for positive/negative (`stdlib/prelude.llt`)
- [ ] `doc:` annotations: add `doc: "..."` to the return-type annotation of every exported function in the second (public) dict, e.g. `fn@[type: Bool  doc: "Returns true if pred holds for any element"]` (`stdlib/prelude.llt`)

**Tasks — `numeric.llt`:**

- [ ] Add return type annotation to `to-bytes`: `fn@Str`; verify type alias entries (`UInt8`, `UInt16`, etc.) carry correct `Int` range constraints (`stdlib/numeric.llt`)

**Tasks — `formatter/compact.llt`:**

- [ ] Public/private split: move `join-strings-impl`, `map-list-impl`, `make-entry` into a first dict; public formatting functions in the final dict reference them by plain name (`stdlib/formatter/compact.llt`)
- [ ] Replace `format-node` `cond` dispatch on `node.type` string with `[match [get "type" node] "literal" ... _ [error ...]]` (`stdlib/formatter/compact.llt`)
- [ ] Replace `format-literal` `cond` dispatch on `node.kind` string with `[match [get "kind" node] "int" ... _ [error ...]]` (`stdlib/formatter/compact.llt`)
- [ ] Complete annotation pass: add `fn@Str` return types on all formatting functions; add param annotations for `node@Dict`, `entry@Dict`, `na@Dict` etc. (`stdlib/formatter/compact.llt`)

**Tasks — `formatter/pretty.llt`:**

- [ ] Read file; apply same three improvements: public/private split, string-dispatch → `[match]`, complete annotation pass (`stdlib/formatter/pretty.llt`)

**Tasks — `out/` formatters (7 files: `json`, `json-pretty`, `yaml`, `csv`, `toml`, `env`, `raw`):**

- [ ] For each file: (a) identify internal helpers; apply public/private `---` split if any exist; (b) replace any `type-of`/cond-string dispatch with `[match]`; (c) add `fn@Str` return types to all output-generating functions and `@Type` to all params (`stdlib/out/`)

**Tasks — `in/json.llt`, `io.llt`, `net.llt`:**

- [ ] For each file: public/private split, pattern match modernization, complete annotation pass (`stdlib/in/json.llt`, `stdlib/io.llt`, `stdlib/net.llt`)

**Tests and spec:**

- [ ] Run full corpus test suite after each file refactor; zero regressions required (`tests/corpus/`)
- [ ] Add one corpus test per pattern-matched `try` result site verifying the new dispatch path: `[ok: v]` arm and `[err: e]` arm both exercised (`tests/corpus/eval/stdlib/`)
- [ ] Update `doc/11-stdlib.md` type signature table to reflect new union-type annotations (`@[Dict Seq]` on dual-dispatch functions) and any newly-annotated functions (`doc/11-stdlib.md`)

## Testing & Quality

### `corpus-section-consistency`

Enforce one semantic per labeled section and unify all corpus runners behind a single shared enforcement engine. The canonical contract after this sprint:

- `=== out` — eval succeeds; output matches substring. Never used for error messages.
- `=== error` — eval fails; message matches substring. Must be non-empty. Every `=== error` file must also produce a runtime error message containing `[EXXX]`.
- `=== warn` — type warnings match substring. Orthogonal to `out`/`error`. Absent section asserts zero warnings. Enforced uniformly by the shared runner regardless of pipeline.

The `valid/` and `eval/` directory distinction is load-bearing: `valid/` = parse-only tests (some files reference undefined vars that parse legally but would fail eval); `eval/` = full evaluation tests. The runner is unified; the pipeline passed to it differs.

**Shared runner infrastructure (`tests/test_helpers.rs`):**

- [ ] Extract `split_test_file` into `tests/test_helpers.rs`; reconcile `String`-vs-`&str` return type (favor owned `String`); re-export for use by `corpus_tests.rs` and `lsp_corpus_tests.rs` (`tests/test_helpers.rs`)
- [ ] Add `run_corpus_dir(dir: &Path, pipeline: impl Fn(&TestFile) -> CorpusOutcome) -> Vec<Failure>` to `tests/test_helpers.rs`; `CorpusOutcome { output: Option<String>, warnings: Vec<String>, error: Option<String> }`; the function handles: find files, split, mutual-exclusivity guard (`=== out` + `=== error` rejected), call pipeline, compare each channel against expectations, collect failures (`tests/test_helpers.rs`)
- [ ] Add runner guard inside `run_corpus_dir`: `=== error` section must be non-empty; `=== warn` section must be non-empty; blank labeled sections are a test-file authoring error (`tests/test_helpers.rs`)
- [ ] Add `[EXXX]` runtime check inside `run_corpus_dir`: when `CorpusOutcome.error` is `Some`, assert it contains an `[EXXX]` prefix — preserves the check currently in `test_eval_error_corpus:721` and applies it universally (`tests/test_helpers.rs`)

**Rewrite `test_eval_corpus` using shared runner:**

- [ ] Replace the inline enforcement logic in `test_eval_corpus` with a call to `run_corpus_dir`; the eval pipeline closure calls `eval_source_with_config` and `typecheck_source`, returning `CorpusOutcome { output, warnings, error }` (`tests/corpus_tests.rs`)
- [ ] Remove the `errors_dir` exclusion filter — `eval/errors/` runs through the unified runner once its files use `=== error` (`tests/corpus_tests.rs`)
- [ ] Delete `test_eval_error_corpus` and `test_eval_error_corpus_has_error_codes` — fully superseded (`tests/corpus_tests.rs`)

**Rewrite `test_valid_corpus` using shared runner:**

- [ ] Replace `test_valid_corpus` with a call to `run_corpus_dir` using a parse-only pipeline: `parse()` + `typecheck_source()`, returning `CorpusOutcome { output: None, warnings, error: None }`; parse failure maps to a `Failure` directly; `=== out` and `=== error` sections in `valid/` files are rejected as authoring errors (`tests/corpus_tests.rs`)
- [ ] This gives `valid/` files full `=== warn` enforcement for free — the shared runner applies the same warn-channel logic regardless of pipeline

**`=== error` migration — `eval/errors/` (113 files):**

- [ ] Rename `=== out` → `=== error` in all 113 files under `tests/corpus/eval/errors/` (scripted: `sed -i 's/^=== out$/=== error/' tests/corpus/eval/errors/**/*.llt-eval`); verify with `cargo test` (`tests/corpus/eval/errors/`)

**LSP runner:**

- [ ] Update `lsp_corpus_tests.rs` to use `split_test_file` from `tests/test_helpers.rs` — the LSP runner has its own enforcement logic and keeps it, but shares the file parsing step (`tests/lsp_corpus_tests.rs`)

