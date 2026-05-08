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
- [x] Add Bytes dual-dispatch for `starts-with?` and `ends-with?` (byte-prefix/suffix match) (`src/builtins_string.rs`)
- [x] Add Bytes dual-dispatch for `contains?`: byte-pattern search via `bytes-find` on single-byte needle; needle must be Int 0-255 (`stdlib/prelude.llt`)
- [x] Add Bytes dual-dispatch for `length`: byte count (`end - start`) as Int (`src/builtins_dict.rs`); `get`/`nth` still deferred
- [x] Add Bytes dual-dispatch for `map`, `filter`, `reduce`, `first`, `last`, `take`, `drop`: iterate over byte values as Int (0–255); results are Seq; `first`/`last` on Bytes return byte as Int; `fold`, `slice`, `count`, `reverse` still deferred (`src/builtins_seq_reduce.rs`, `src/builtins_seq_xform.rs`, `src/builtins.rs`)
- [x] Add Bytes dual-dispatch for `split`, `replace`, `join`: byte-pattern split/replace, byte-separator join (deferred from `bytes-type` sprint) (`src/builtins_string.rs`)
- [x] Add Seq dual-dispatch for `starts-with?` and `ends-with?` (element-by-element prefix match) (`src/builtins_string.rs`)
- [x] Create `stdlib/strings.llt` with pure-tinct functions: `pad-left`, `pad-right`, `str-repeat`, `str-find`, `str-reverse` (Note: `str-contains?` is already a Rust builtin, `str-repeat` is in prelude but duplicated here per requirements) (`stdlib/strings.llt`)
- [x] Load `stdlib/strings.llt` at startup (alongside `prelude.llt`); `pad-left`, `str-find`, `str-reverse` available without explicit include (`src/builtins.rs`)
- [x] Tests: verify `basename` is NOT in scope without `[include "stdlib/path.llt"]` (`tests/corpus/eval/stdlib/path_scoping.llt-eval`); IS available shown by existing `path_basename.llt-eval`
- [x] Tests: verify `parse-toml-lite` is NOT in scope without `[include "stdlib/toml-lite.llt"]` (`tests/corpus/eval/stdlib/toml_scoping.llt-eval`)
- [x] Register type signatures for all new builtins (`src/types.rs`)
- [x] Tests: corpus tests for starts-with?/ends-with? on String/Bytes/Seq, str-slice O(1), str-find, pad-left/pad-right alignment (`tests/corpus/eval/builtins/`, `tests/corpus/eval/stdlib/`)

### `math-builtins`: Extended Math Builtins

**Spec chapters:** `doc/whatif/lib-supplemental.md` §Extended Math Builtins. Independent of other sprints.

- [x] Implement 13 math Rust builtins as `f64` method wrappers: `pow` (powf), `sqrt`, `log` (ln), `log2`, `log10`, `exp`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2` (`src/builtins_math.rs`)
- [x] Implement 3 float predicate builtins: `nan?` (is_nan), `inf?` (is_infinite), `finite?` (is_finite) (`src/builtins_math.rs`)
- [x] Register all 16 builtins in `standard_builtins()` with correct `Strictness` (`src/builtins.rs`)
- [x] Register type signatures: `pow: Number → Number → Float`, `sqrt/log/sin: Float → Float`, `nan?/inf?/finite?: Float → Bool`, `atan2: Float → Float → Float` (`src/types.rs`)
- [x] Create `stdlib/math.llt` with Float literals (`pi`, `e`, `phi`) and pure-tinct functions (`hypot`, `deg->rad`, `rad->deg`, `log-base`) (`stdlib/math.llt`)
- [x] Load `stdlib/math.llt` at startup (alongside `prelude.llt`); `pi`, `hypot`, `deg->rad`, `rad->deg`, `log-base` available without explicit include (`src/builtins.rs`)
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
- [x] Load `stdlib/encoding.llt` at startup (alongside `prelude.llt`); `hex-encode`, `hex-decode`, `base64-encode`, `base64-decode`, `mask-apply` available without explicit include (`src/builtins.rs`)
- [x] Register `HashAlgorithm` type alias in `TypeEnv::with_builtins()` as union of `StringLiteral` variants (pending `Type::Variant`); type checker recognises `HashAlgorithm` in annotations (`src/type_env.rs`)
- [x] Tests: corpus tests for all bitwise ops, char-code/chr round-trips, hex-encode/hex-decode, base64 (`tests/corpus/eval/builtins/`, `tests/corpus/eval/stdlib/`)

### `handle-caps`: Capability-Typed Handles & Streaming I/O

**Spec chapters:** `doc/whatif/lib-supplemental.md` §Streaming File I/O. **Depends on:** `string-view`.

- [x] Decide `Handle` capability representation — `HashMap<String, Value>`: cap name → associated data. Boolean caps (Readable, Writable, etc.) → `Value::Null`; protocol caps (Tls, Quic, user-defined) → `Value::Dict` carrying handshake/session metadata. User-extensible: any Connector can attach arbitrary data to custom cap names. `tls-peer-cert h` reads `handle.caps.get("Tls")` directly, no special-casing needed.
- [x] Replace `Value::Handle(Rc<RefCell<Box<dyn BufRead>>>)` with a struct carrying `caps: HashMap<String, Value>` + the inner reader/writer; keep existing `Rc<RefCell<Box<dyn BufRead>>>` as the read-side inner (`src/value.rs`)
- [x] Add `Value::WriteHandle` variant carrying `caps: HashMap<String, Value>` + encoding tag (Text/Binary) + `Box<dyn Write>` (`src/value.rs`)
- [x] Implement `cap-data` Rust builtin: `Handle → String → Value` — reads associated data for a named capability; errors if cap is absent (`src/builtins_io.rs`, `src/types.rs`)
- [x] Implement `has-cap?` Rust builtin: `Handle → String → Bool` — tests whether Handle has a named capability (`src/builtins_io.rs`, `src/types.rs`)
- [ ] Refactor `builtin_open`: replace 3-arg (DirCap, path, mode-string) signature with variadic Variant flags; each `Value::Variant { tag, payload }` arg inserts `(tag, payload.unwrap_or(Value::Null))` into the caps HashMap; require at least one flag (error otherwise); derive encoding (Text/Binary) and direction from cap presence (`src/builtins_io.rs`)
- [ ] `open` returns `Value::Handle` when `Readable` is in caps, `Value::WriteHandle` when `Writable` (but not `Readable`) is in caps; both carry the full caps HashMap (`src/builtins_io.rs`)
- [x] Implement `write` Rust builtin: polymorphic on WriteHandle encoding — `String` arg for Text, `Bytes` arg for Binary; returns WriteHandle for chaining (`src/builtins_io.rs`)
- [x] Implement `flush` Rust builtin: flushes WriteHandle buffer; returns WriteHandle (`src/builtins_io.rs`)
- [x] Implement `close` Rust builtin: flushes and closes WriteHandle; returns null; further writes error (`src/builtins_io.rs`)
- [ ] Implement `seek` Rust builtin: requires Seekable in caps; change inner read type from `Box<dyn BufRead>` to `Box<dyn Read + Seek>` for Seekable handles (non-Seekable handles keep BufRead); `lseek` to byte offset; returns Handle (`src/builtins_io.rs`, `src/value.rs`)
- [ ] Implement `seek-end` Rust builtin: requires Seekable; seek to end (`src/builtins_io.rs`)
- [ ] Implement `position` Rust builtin: requires Seekable; returns current byte offset as Int (`src/builtins_io.rs`)
- [x] Update `builtin_slurp`: dispatch on Handle encoding — Text → String, Binary → Bytes (`src/builtins_io.rs`)
- [x] Update `builtin_lines`: require Text encoding flag; error on Binary handles (`src/builtins_io.rs`)
- [x] Update `builtin_connect`: return `Handle` with caps HashMap `{"Binary": Null, "Readable": Null, "Writable": Null, "Stream": Null}` (no Seekable — streams are sequential); network protocol layers (Tls) insert their cap with `Value::Dict` metadata during handshake (`src/builtins_io.rs`)
- [x] Register type signatures for all new builtins (`src/types.rs`)
- [x] Update `stdlib/io.llt`: add `write-line`; extend `write-file`/`write-file-atomic` to accept `content@[String Bytes]`; remove old `open` mode-string wrappers (`stdlib/io.llt`)
- [x] Tests: corpus tests for open with flags (Readable, Writable, Binary), write + slurp round-trip, seek + position, close-then-write error, encoding mismatch error (`tests/corpus/eval/builtins/`) — write+slurp roundtrip done; seek/position/close-then-write deferred

## TLS & HTTP

Accepted from `doc/whatif/lib-tls.md` (2026-05-07).

### `uri-type`: Uri/Url/Urn Types & HTTP Client Redesign

**Spec chapters:** `doc/whatif/lib-tls.md` §Type Checker (Uri/Url/Urn types, http-get merged). **Depends on:** `string-utils`, `bytes-type`.

- [ ] Implement `uri` Rust builtin: parse any URI string → Uri; `host`/`port` nullable — null for non-hierarchical (mailto:, tel:, urn:, news:); IPv6 brackets stripped; query/fragment separated (`src/builtins_uri.rs`)
- [ ] Implement `url` Rust builtin: parse hierarchical URL → Url; error if no authority (no host); port scheme-defaulted (80 for http, 443 for https, etc.) if absent (`src/builtins_uri.rs`)
- [ ] Implement `urn` Rust builtin: parse URN → Urn per RFC 8141; error if scheme is not "urn"; split NID and NSS; parse `?+r-component` and `?=q-component` as separate fields (distinct from standard query); empty r-component silently accepted (`src/builtins_uri.rs`)
- [ ] Register `Uri`, `Url`, `Urn` type aliases and builtin signatures in `TypeEnv` (`src/types.rs`, `src/builtins.rs`)
- [ ] Implement pure-tinct `uri-params: [fn@Dict [u@[Uri Url]]]` — parse `u.query` → `{key: value}`; `{}` if null (`stdlib/net.llt`)
- [ ] Implement pure-tinct `uri-origin: [fn@String [u@Url]]` — `"scheme://host:port"` (Url only) (`stdlib/net.llt`)
- [ ] Implement pure-tinct `uri->string: [fn@String [u@[Uri Url Urn]]]` — reconstruct full URI/URL/URN string (`stdlib/net.llt`)
- [ ] Refactor `stdlib/net.llt` `http-get` to take `url@Url`; dispatch on `url.scheme`; remove separate `https-get`; remove `parse-url` internal helper (`stdlib/net.llt`)
- [ ] Update `http-connect` Rust builtin to take `url@Url` instead of `host`/`port` separately (`src/builtins_io.rs`, `src/types.rs`)
- [ ] Tests: `uri` parsing of hierarchical (https, postgres, s3) and non-hierarchical (mailto, tel, urn) URIs; `url` error on non-hierarchical; `urn` NID/NSS splitting; `uri-params` multi-value; `uri->string` round-trip; `http-get` dispatching on url.scheme (`tests/corpus/eval/builtins/`)

### `http-net`: HTTP Client & Network Stack

**Spec chapters:** `doc/whatif/lib-tls.md` §HttpConn, §HTTP/2 HTTP/3, §Network Stack Summary. **Depends on:** `connector-tls`, `uri-type`.

- [ ] Add `reqwest = { version = "0.12", features = ["http2", "http3", "brotli", "rustls-tls"] }` to `Cargo.toml` (`Cargo.toml`)
- [ ] Add `Value::HttpConn` to Value enum (wraps reqwest Client or connection pool) (`src/value.rs`)
- [ ] Implement `http-connect` Rust builtin — Connector form: `http-connect connector uri@Uri opts` → `HttpConn`; Handle form: `http-connect h@Handle uri@Uri` → `HttpConn`; internally opens Tcp (HTTP/1.1+2) or Udp (HTTP/3) based on ALPN (`src/builtins_io.rs`)
- [ ] Implement `http-get` overload on `HttpConn`: `http-get conn uri@Uri headers` → response Dict (`src/builtins_io.rs`)
- [ ] Implement `socks5-connect` Rust builtin: SOCKS5 tunnel; takes Handle + host + port + creds → Handle[Stream RW] (`src/builtins_io.rs`)
- [ ] Implement `proxy-connect` Rust builtin: HTTP CONNECT tunnel; takes Handle + host + port → Handle[Stream RW] (`src/builtins_io.rs`)
- [ ] Register all type signatures (`src/types.rs`)
- [ ] Tests: corpus tests for HttpConn connection reuse, proxy tunneling, http-get via HttpConn with Url (`tests/corpus/eval/builtins/`, integration tests)

---

## Tooling

### `doc-annotations`: Inline Documentation via `@[doc: "..."]`

Extend the existing `DocMap` infrastructure (already wired into LSP hover) to cover dict entry bindings and fn return annotations, add `:describe` to the REPL, and surface doc strings in `tinct describe`. See `src/typecheck.rs` (`extract_doc_strings`) and `src/lsp/analysis.rs` (`doc_suffix`, `hover_at`).

- [x] Extend `extract_doc_from_expr` to extract `doc:` from **dict entry key annotations**: `name@[doc: "..."]: value` — insert `name → doc_string` into DocMap. Currently only param annotations are extracted. (`src/typecheck.rs`)
- [x] Extend `extract_doc_from_expr` to extract `doc:` from **fn return-type PropertyDict**: `fn@[type: T  doc: "..."]` — thread the enclosing dict entry name down through recursion to key the doc string. (`src/typecheck.rs`)
- [x] Add REPL meta-command infrastructure: detect lines starting with `:` before passing to `eval_input`; dispatch to a `handle_meta_command` function. (`src/repl.rs`)
- [x] Implement `:describe <name>` REPL command: look up `name` in the session's DocMap and TypeMap; format and print type signature + doc string. Output format mirrors LSP hover: `name : TypeSignature\n\nDoc string here.` (`src/repl.rs`)
- [x] Implement `:type <name>` REPL command: type signature only, no doc. (`src/repl.rs`)
- [x] Implement `:help` REPL command: list available meta-commands with one-line descriptions. (`src/repl.rs`)
- [ ] Extend `run_describe` (`tinct describe`): include doc strings from DocMap alongside type signatures in both text and JSON output modes. (`src/main.rs`)
- [x] Add `@[doc: "..."]` annotations to 8 representative prelude functions as working examples and adoption seed: `map`, `filter`, `reduce`, `sorted`, `contains?`, `empty?`, `get`, `get-or`. (`stdlib/prelude.llt`; `first`/`last` are Rust builtins, not in prelude)
- [x] Tests: unit tests verifying `doc:` on dict entry binding, function param, and fn return annotation are parsed and available. (`src/typecheck.rs`)
- [ ] Tests: CLI test for `tinct describe` output including doc string when annotation is present. (`tests/cli_tests.rs`)


## Standard Library

### `stdlib-modernize` (continued)

Remaining items deferred from the completed `stdlib-modernize` sprint (type annotations + pattern matching done; see DONE.md).

**Tasks — `prelude.llt`:**

- [ ] Public/private split: move all `-impl`, `-step`, `-check` helpers (≈30 functions) into a first dict in the same document; move all public functions into a second (final) dict; helpers are reachable by plain name from the public dict and are not exported (`stdlib/prelude.llt`)
- [ ] Union type annotations for dual-dispatch parameters: add `@[Dict Seq]` to `sorted`, `sorted-by`, `zip`, `contains?`, `flat-map`, `partition`, `group-by`, `fold`, `map` (wrapper), `reduce` (wrapper) — failed in initial attempt due to type system limitation (`stdlib/prelude.llt`)


**Tasks — `formatter/compact.llt`:**

- [ ] Public/private split: move `join-strings-impl`, `map-list-impl`, `make-entry` into a first dict; public formatting functions in the final dict reference them by plain name (`stdlib/formatter/compact.llt`)
- [x] Replace `format-node` `cond` dispatch on `node.type` string with `[match [get "type" node] "literal" ... _ [error ...]]` (`stdlib/formatter/compact.llt`)
- [x] Replace `format-literal` `cond` dispatch on `node.kind` string with `[match [get "kind" node] "int" ... _ [error ...]]` (`stdlib/formatter/compact.llt`)

**Tasks — `out/` formatters (7 files: `json`, `json-pretty`, `yaml`, `csv`, `toml`, `env`, `raw`):**

- [x] Annotation pass: add `fn@Str` return types to all output-generating functions and `@Type` to all params (7 files) (`stdlib/out/`)
- [ ] For each file: (a) identify internal helpers; apply public/private split if any exist; (b) replace any `type-of`/cond-string dispatch with `[match]` (`stdlib/out/`)

**Tasks — `in/json.llt`, `io.llt`, `net.llt`:**

- [x] Annotation pass: complete type annotations for all functions (`stdlib/in/json.llt`, `stdlib/io.llt`, `stdlib/net.llt`)
- [ ] For each file: public/private split, pattern match modernization (`stdlib/in/json.llt`, `stdlib/io.llt`, `stdlib/net.llt`)

**Tests and spec:**

- [x] Run full corpus test suite after each file refactor; zero regressions required (`tests/corpus/`)
- [ ] Add one corpus test per pattern-matched `try` result site verifying the new dispatch path: `[ok: v]` arm and `[err: e]` arm both exercised (`tests/corpus/eval/stdlib/`)
- [ ] Update `doc/11-stdlib.md` type signature table to reflect new union-type annotations (`@[Dict Seq]` on dual-dispatch functions) and any newly-annotated functions (`doc/11-stdlib.md`)


