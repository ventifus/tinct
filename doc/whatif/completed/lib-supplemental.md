# What If: Supplemental Standard Library Modules for tinct

**State:** Accepted — 2026-05-07

What would it take to ship standard library modules beyond the core
prelude, covering extended strings, math, bitwise encoding, TOML
parsing, streaming file I/O, string-as-sequence operations, a native
`Bytes` type, and a generalised filesystem capability protocol (`FsCap`)
supporting S3, WebDAV, and other object-store backends alongside the
local POSIX filesystem?

## Current State

tinct ships a single standard library in `stdlib/prelude.llt` with
52 public functions and approximately 12 Rust-native operator wrappers,
built on top of 46 Rust builtins in `src/builtins.rs`. The core
prelude covers:

- **Collections**: `map`, `filter`, `reduce`, `sort`, `sort-by`, `zip`,
  `flatten`, `group-by`, `partition`, `slice`, `fold`, `flat-map`,
  `deep-merge`, `walk`, `transpose`, `uniq`
- **Dict utilities**: `get`, `get-or`, `get-in`, `set`, `update`,
  `remove`, `entries`, `from-entries`, `values`, `has?`, `empty?`
- **List utilities**: `first`, `last`, `nth`, `rest`, `cons`, `conj`,
  `reverse`, `reindex`
- **Aggregates**: `sum`, `product`, `min`, `max`, `count`, `any?`,
  `all?`, `contains?`
- **Math**: `abs`, `sign`, `clamp`, `quot`, `mod`, `ceil`, `trunc`
  (all pure-tinct on top of `floor` and `round` builtins)
- **String builtins** (Rust): `split`, `replace`, `upper`, `lower`,
  `trim`, `str`, `join`
- **Type predicates**: `int?`, `str?`, `float?`, `bool?`, `dict?`,
  `fn?`
- **Control flow**: `when`, `unless`, `cond`, `until`, `assert`
- **Composition**: `compose`, `->`

A motivating use case — generating YAML output — illustrates the gap.
The `yaml-quote-string` helper must decide whether a string requires
quoting. Without string predicates, it must test every special character
by splitting:

```tinct
# Current: repeated split checks for each special character
yaml-needs-quoting?: [fn [s]
  [or
    [or [= "" s]
              [or
                [> 1 [length [split s ":"]]]
                [> 1 [length [split s "#"]]]]]
    [or
      [> 1 [length [split s "{"]]]
      [> 1 [length [split s "}"]]]
    ]]]
# ... continues for 15+ special characters
```

With `str-contains?` this is terser; with the regex engine from
`doc/whatif/lib-regex.md` it collapses to one `re-match` call.

### What's Missing

1. **Extended string utilities** — no `str-contains?`, `starts-with?`,
   `ends-with?`, character-indexed slicing, string padding, or string repetition.
2. **Extended math** — no `pow`, `sqrt`, `log`, `exp`, or trigonometric
   functions; tinct's math coverage stops at floor/round/abs.
3. **Encoding** — no base64, hex, or bitwise operations; blocks binary
   data handling and HTTP configuration generation.
4. **Binary data** — no native type for byte sequences; TLS certificates,
   SSH keys, cryptographic hashes, and arbitrary binary payloads must
   be stored as strings (violating UTF-8 invariants) or as Dicts of
   integers (correct but expensive and awkward).
5. **TOML parsing** — no way to read TOML files from within tinct scripts;
   blocks self-hosted tooling that reads Cargo.toml, Cargo.lock, or
   other TOML configuration feeds.
6. **Streaming file writes and filesystem operations** — `write-file` is
   atomic and `DirCap` supports only local POSIX paths. No streaming
   writes, no `list-dir`/`rename`/`copy`/`stat`, and no way to plug in
   non-POSIX backends (S3, WebDAV, object stores).
7. **Strings as sequences** — strings cannot be passed directly to `map`,
   `filter`, `reduce`, etc.; you must split to a collection first. Blocks
   natural character-level string processing.
8. **Path manipulation** — no `basename`, `dirname`, `path-join`, or
   `path-extension`; blocks file-path-heavy configuration.

Pattern matching and regex are addressed separately in
`doc/whatif/lib-regex.md`, which depends on the bitwise primitives
from §Bitwise Primitives.

## Why Supplemental Stdlib Matters for tinct

**Serialization helpers become writable.** YAML, TOML, and Nginx
config generation require knowing whether strings need quoting.
Without string predicates, every serialization helper is a pile of
`split` checks.

**Mathematical configuration.** Network configs involve subnets and
masks (bitwise math). Audio/video configs involve sample rates and
frequency ratios (trig, log). Scientific instrument configs involve
calibration curves (exp, pow). None are expressible today.

**Encoding round-trips.** Base64 is the lingua franca for embedding
binary data in config files — TLS certificates, SSH keys, container
image digests. The bitwise primitives enable `base64-encode`
and `hex-encode` as pure-tinct library functions.

**Self-hosted tooling.** tinct scripts that read TOML configuration
files (Cargo.toml, Cargo.lock, channel manifests) need a TOML parser
accessible from within tinct itself.

**Completeness relative to peers.** Jsonnet's `std` has base64 and
math. Nickel's `std.number` has trig. CUE has `math`, `encoding/base64`,
and `path` packages. Nix has 50+ `lib.strings` functions. tinct's peer
languages treat these as non-negotiable.

## Design

Supplemental modules ship in two categories:

| Category | Implementation | Crate dependency |
|----------|----------------|-----------------|
| Pure-tinct | `stdlib/*.llt` | None |
| New Rust builtin | `src/builtins.rs` | None |

No new crates are introduced by any of the proposals in this document.

### Module Survey

Comparable configuration languages cover these domains in their
standard libraries:

| Feature | Jsonnet | Nickel | Nix | CUE | tinct |
|---------|---------|--------|-----|-----|-------|
| String search (`contains`, `starts-with`) | ✓ | ✓ | ✓ | ✓ | proposed |
| Regex | `std.native()` | ✓ | ✓ | ✓ | see lib-regex.md |
| Math (`pow`, `sqrt`, trig) | ✓ | ✓ | partial | ✓ full | partial |
| Base64 / encoding | ✓ | ✓ | ✗ | ✓ | proposed |
| Path utilities | ✗ | ✗ | ✓ `lib` | ✓ `path` | proposed |
| Date/time | ✗ | ✗ | ✗ | ✓ `time` | ✗ |

### Extended String Utilities

**New Rust builtins moving to prelude:**

`starts-with?` and `ends-with?` generalize to any sequence — they are
not string-specific. A string's character-Seq participates via
dual-dispatch, so `[starts-with? "he" "hello"]` and
`[starts-with? [1 2] [1 2 3 4]]` both work through the same builtin.
They belong alongside `contains?` in the prelude, not in a string
module.

| Function | Signature | Notes |
|----------|-----------|-------|
| `starts-with?` | `Seq\|String → Seq\|String → Bool` | `starts-with? prefix haystack`; moves to prelude |
| `ends-with?` | `Seq\|String → Seq\|String → Bool` | `ends-with? suffix haystack`; moves to prelude |

**New Rust builtin staying in string domain:**

| Function | Signature | Notes |
|----------|-----------|-------|
| `str-slice` | `Int → Int → String → String` | O(1) `String` construction; `str-slice from to s` |

`str-slice` promotes from pure-tinct (which went through `str-chars` +
`take`/`drop` + `join`) to a Rust builtin that directly constructs
`String { source: Rc::clone(&source), start: byte_of(start), end: byte_of(end) }` — constant time, zero allocation.

**`str-chars` — internal implementation primitive.** With strings
participating directly in `map`/`filter`/`first`/`nth` via
dual-dispatch, `str-chars` is no longer a recommended user-facing
function. It remains as a Rust builtin used internally by `str-find`
but is not exported from `stdlib/strings.llt`. Users who want a Seq of
characters write `[map [fn [c] c] s]` or simply use string operations
directly.

`str-chars` takes a `String { source, start, end }` and produces a
lazy `Seq` of `String` slices by walking `source[start..end].char_indices()`,
yielding `String { source: Rc::clone(&source), start: start+off, end: start+off+ch.len_utf8() }` per codepoint. One `Rc::clone` (pointer bump) per step; no string data copied.

**Pure-tinct additions to `stdlib/strings.llt`:**

```tinct
# stdlib/strings.llt

# str-contains? — true if needle appears anywhere in haystack
str-contains?: [fn@Boolean [needle@String haystack@String]
  [> [length [split haystack needle]] 1]]

# pad-left — left-pad s to width with spaces
pad-left: [fn@String [width@Integer s@String]
  [str
    [join "" [take [max 0 [- width [str-length s]]] [repeat " "]]]
    s]]

# pad-right — right-pad s to width with spaces
pad-right: [fn@String [width@Integer s@String]
  [str s
    [join "" [take [max 0 [- width [str-length s]]] [repeat " "]]]]]

# str-repeat — repeat s n times
str-repeat: [fn@String [n@Integer s@String] [join "" [take n [repeat s]]]]

# str-find — character index of first occurrence of needle, or -1
str-find: [fn@Integer [needle@String haystack@String]
  [if [str-contains? needle haystack]
    [str-length [first [split haystack needle]]]
    -1]]

# These become natural with String dual-dispatch:

# str-reverse — reverse a string character by character
str-reverse: [fn@String [s@String]
  [join "" [reverse s]]]

# str-take — first n characters
str-take: [fn@String [n@Integer s@String]
  [join "" [take n s]]]

# str-drop — drop first n characters
str-drop: [fn@String [n@Integer s@String]
  [join "" [drop n s]]]

# str-count — count characters matching predicate
str-count: [fn@Integer [pred@Fn s@String]
  [count [filter pred s]]]
```

`pad-left` and `pad-right` use a single space as the fill character,
covering the column-alignment use case.

`str-find` returns a character offset — correct for ASCII; for
multi-byte Unicode it returns the character count of the prefix before
the match, not the byte offset.

**Adds 3 Rust builtins:** `starts-with?` (to prelude), `ends-with?`
(to prelude), `str-slice`. `str-chars` stays as an internal builtin,
not exported.

### Extended Math Builtins (stdlib/math.llt)

tinct's current math coverage ends at `floor`, `round`, and the
pure-tinct derivatives in prelude. Missing functions are all trivial
wrappers around Rust's `f64` methods — no new crate needed.

**New Rust builtins:**

| Function | Rust equivalent | Notes |
|----------|----------------|-------|
| `pow base exp` | `f64::powf` | Both args coerced to float |
| `sqrt x` | `f64::sqrt` | Returns `Float` |
| `log x` | `f64::ln` | Natural log |
| `log2 x` | `f64::log2` | |
| `log10 x` | `f64::log10` | |
| `exp x` | `f64::exp` | `e^x` |
| `sin x` | `f64::sin` | Radians |
| `cos x` | `f64::cos` | Radians |
| `tan x` | `f64::tan` | Radians |
| `asin x` | `f64::asin` | Returns radians |
| `acos x` | `f64::acos` | Returns radians |
| `atan x` | `f64::atan` | Returns radians |
| `atan2 y x` | `f64::atan2` | Two-argument form |

Pure-tinct additions to `stdlib/math.llt`:

```tinct
# stdlib/math.llt
pi:        3.141592653589793
e:         2.718281828459045
hypot:     [fn [x y] [sqrt [+ [pow x 2] [pow y 2]]]]
deg->rad:  [fn [d] [* d [/ pi 180.0]]]
rad->deg:  [fn [r] [* r [/ 180.0 pi]]]
log-base:  [fn [b x] [/ [log x] [log b]]]
```

`pi` and `e` are Float literals — no Rust builtin needed. All trig
functions operate in radians, consistent with every other language
surveyed. Degree conversion is a pure-tinct helper.

**Adds 13 Rust builtins.** No new crates.

### Bitwise Primitives (stdlib/encoding.llt)

Rather than shipping specific encoding builtins, this section provides
the primitive bitwise operations from which users can implement base64, hex,
subnet masks, permission flags, or any other bit-level algorithm in
pure-tinct. The Rust builtins are the smallest useful layer; derived
operations live in `stdlib/encoding.llt`.

**New Rust builtins:**

**`band a b`** — bitwise AND of two integers.

**`bor a b`** — bitwise OR of two integers.

**`bxor a b`** — bitwise XOR of two integers.

**`shl a n`** — left-shift `a` by `n` bits.

**`shr a n`** — logical right-shift `a` by `n` bits (zero-fills high
bits, treating the value as unsigned 64-bit).

**`char-code s`** — Unicode codepoint of the first character of string
`s` as an `Int`. For ASCII characters this equals the byte value.

**`chr n`** — single-character string whose Unicode codepoint is `n`.
Inverse of `char-code`.

**`str-bytes s`** — UTF-8 byte sequence of `s` as a 0-indexed `Dict`
of integers (0–255). Each entry is one byte of the UTF-8 encoding, not
one Unicode character. For ASCII strings, `char-code` and `str-bytes`
agree; for multi-byte Unicode they differ.

**`bytes-str bytes`** — string whose UTF-8 encoding is the given
0-indexed `Dict` of byte integers. Inverse of `str-bytes`. Errors on
invalid UTF-8.

**Derived operations in `stdlib/encoding.llt` (pure-tinct):**

```tinct
# stdlib/encoding.llt

b64-alpha: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

# encode one 6-bit index to a base64 character
b64-char: [fn [n] [get n [split b64-alpha ""]]]

# encode three bytes to four base64 characters
b64-triple: [fn [b0 b1 b2]
  [str
    [b64-char [shr b0 2]]
    [b64-char [band [bor [shl b0 4] [shr b1 4]] 63]]
    [b64-char [band [bor [shl b1 2] [shr b2 6]] 63]]
    [b64-char [band b2 63]]]]

# base64-encode — full implementation (handles padding)
base64-encode: [fn [s]
  # ... fold over str-bytes in groups of 3, handle 1-2 byte remainders
  ]

# hex-encode — one byte per two hex chars
hex-digit: [fn [n] [get n [split "0123456789abcdef" ""]]]
hex-byte:  [fn [b] [str [hex-digit [shr b 4]]
                        [hex-digit [band b 15]]]]
hex-encode: [fn [s] [join "" [map hex-byte [str-bytes s]]]]

# subnet mask application (common config use case)
mask-apply: [fn [ip mask] [band ip mask]]
```

The nine Rust builtins — five bitwise ops, two char↔code conversions,
two string↔bytes conversions — are each independently useful and compose
freely.

**`char-code` is also required by `doc/whatif/lib-regex.md`** for
character class range comparisons.

**`HashAlgorithm` type alias** — used by `doc/whatif/lib-tls.md` for
strongly-typed certificate pin fingerprints. Declared here because hash
algorithms are a general encoding primitive, not TLS-specific:

```tinct
[
  HashAlgorithm: [type
    # SHA-2 family (via ring — already in rustls; no new dep)
    Sha256   # 32 bytes; current HPKP/TLSA convention; 128-bit quantum security
    Sha384   # 48 bytes
    Sha512   # 64 bytes
    # SHA-3 / Keccak family (requires sha3 crate — new dep)
    Sha3-256 # 32 bytes; NIST FIPS 202; independent Keccak construction
    Sha3-384 # 48 bytes
    Sha3-512 # 64 bytes
    # BLAKE3 (blake3 crate — already in tinct for $include integrity)
    Blake3   # 32 bytes; fast, modern; not NIST-standardized
  ]
]
```

`Sha256` remains the current standard for SPKI pinning tools. `Sha3-256`
or `Blake3` are preferred for new infrastructure where tooling permits.
SHA-1 is excluded — broken for collision resistance since 2017.

**Adds 9 Rust builtins.** No new crates.

### TOML Parsing Lite (stdlib/toml-lite.llt)

A pure-tinct TOML subset parser for reading configuration files common
in Rust projects — Cargo.toml, Cargo.lock, and TOML-format channel
manifests. The design follows the same two-file pattern as
`from-json` / `in/json.llt`:

- `stdlib/toml-lite.llt` — exports `parse-toml-lite: [fn [s@String] → Dict]`
- `stdlib/in/toml-lite.llt` — pipeline wrapper: `[parse-toml-lite [slurp stdin]]`

Scripts that obtain TOML strings via `read-file` or HTTP fetch call
`parse-toml-lite` directly. `stdlib/in/toml.llt` is intentionally
left unwritten to reserve the slot for a future full Rust-builtin-backed
parser (e.g. wrapping the `toml` crate).

**Supported subset:**

| Feature | Supported | Notes |
|---------|-----------|-------|
| `[section]` standard tables | ✓ | |
| `[[array-table]]` | ✓ | Returns list of dicts under that key |
| `key = "string-value"` | ✓ | Double-quoted strings only |
| `key = 'literal'` | ✗ | |
| Comments (`# ...`) | ✓ | |
| Blank lines | ✓ | Skipped |
| Multi-line strings | ✗ | |
| Dotted keys (`a.b = 1`) | ✗ | |
| Inline tables (`{a = 1}`) | ✗ | |
| Integer / float / bool values | ✗ | String values only |
| Datetime | ✗ | No datetime type in tinct |

**Implementation:** fold over `split content "\n"`, carrying
`{section: Str, tables: Dict}` accumulator state. Uses `starts-with?`
for line-prefix matching.

**Depends on:** `starts-with?`, `ends-with?`, `trim` from §Extended
String Utilities. **Adds 0 Rust builtins.** No new crates.

### Streaming File I/O (WriteHandle)

The archived `io.md` specifies `write-file` and `write-file-atomic` as
atomic operations: the full content string is passed at once. This is
the right default for configuration output. But scripts that build
output incrementally — writing lines one at a time to a log or report
file — need a split open/write/close model.

**Note:** This section describes an early draft. The final `open` API uses capability flag types instead of string modes — see §Streaming File I/O Final Design below.

**`open`** takes capability flag types as positional arguments after the path. `[open cap path]` with no flags is a type error — intent must be explicit.

```tinct
[open cap path Readable]           # Read handle — Handle[Text Readable Seekable]
[open cap path Writable]           # Write handle (truncate) — Handle[Text Writable Seekable]
[open cap path Writable Appendable] # Append handle — Handle[Text Writable Appendable Seekable]
```

**`stdlib/io.llt` additions:**

```tinct
# Open a file for writing (truncates existing content).
open-write: [fn@[Handle [Writable]] [cap@DirCap path@String]
  [open cap path Writable]]

# Open a file for appending.
open-append: [fn@[Handle [Writable Appendable]] [cap@DirCap path@String]
  [open cap path Writable Appendable]]

# Write a string followed by a newline.
write-line: [fn@WriteHandle [wh@WriteHandle s@String]
  [write wh [str s "\n"]]]

# Write all elements of a Seq or Dict, one per line.
write-lines: [fn@WriteHandle [wh@WriteHandle xs]
  [each xs [fn [x] [write wh [str x "\n"]]]]
  wh]
```

Example — a script that builds a report incrementally:

```tinct
[include "stdlib/io.llt"]

# tinct run --cap-fs fs=. report.llt
[out: [open-write fs "report.txt"]]
[write-line out "=== Dependency Report ==="]
[each deps [fn [d]
  [write-line out [str d.name ": " d.version]]]]
[close out]
```

**Adds 3 Rust builtins:** `write`, `flush`, `close`. `open` is extended
for write modes. No new crates.

### Filesystem Capabilities (`FsCap` protocol)

`DirCap` (from `doc/whatif/io.md`) is a built-in capability granting
access to a local filesystem directory — currently hardwired to POSIX
paths via `cap_std`. This section generalises it into a **FsCap
protocol**: any value that implements the required methods can be used
wherever a `DirCap` is accepted. S3 buckets, WebDAV servers, and
user-defined virtual filesystems all become drop-in replacements.

#### Protocol Declaration

A FsCap is a tinct Dict with a `caps` field declaring the capability
flags it supports, plus one field per implemented method. The tinct
evaluator dispatches protocol method calls by looking up the field
name in the FsCap dict — exactly like any other tinct function call.

```tinct
# Minimal shape of a user FsCap:
[
  caps:       [Readable Writable Exclusive]   # declared capabilities (nominal variants)
  open:       [fn [path flags...] ...]
  write-file: [fn [path content] ...]
  list-dir:   [fn [path] ...]
  stat:       [fn [path] ...]
  copy:       [fn [src dst] ...]
  remove:     [fn [path] ...]
  make-dir:   [fn [path] [error "not supported"]]
  rename:     [fn [src dst] [error "rename not atomic on this backend"]]
]
```

`DirCap` (the built-in Rust value) carries the same `caps` field
internally; the type checker reads it to enforce flag constraints.

#### Capability Flags

Extending the flag set from §Streaming File I/O:

| Flag | O_ flag | POSIX | S3 | WebDAV | NFS | Notes |
|------|---------|-------|----|--------|-----|-------|
| `Readable` | `O_RDONLY` | ✓ | ✓ | ✓ | ✓ | |
| `Writable` | `O_WRONLY` | ✓ | ✓ | ✓ | ✓ | |
| `Appendable` | `O_APPEND` | ✓ | ✗ | ✓ | ✓ | S3 must re-upload entire object |
| `Seekable` (read) | — | ✓ | ✓ (byte-range GETs) | ✓ | ✓ | |
| `Seekable` (write) | — | ✓ | ✗ | ✓ | ✓ | S3 requires full atomic upload |
| `Exclusive` | `O_EXCL` | ✓ | ✓ (`If-None-Match: *`) | ✓ | ✓ | |
| `Sync` | `O_SYNC` | ✓ | ✗ | ✗ | ✓ | |
| `NoFollow` | `O_NOFOLLOW` | ✓ | n/a | n/a | ✓ | |
| `Atomic` | — | ✓ | ✗ | ✓ | ✓ | Gates `rename`; S3 rename = copy+delete |
| `Linkable` | — | ✓ | ✗ | ✗ | ✓ | Gates `link`; no concept in object stores |
| `Symlinkable` | — | ✓ | ✗ | ✗ | ✓ | Gates `symlink`/`read-link` |
| `Watchable` | — | ✓ (inotify) | ✓ (events, async) | ✗ | ✓ | **Stub — future** |

#### Protocol Methods

**`open fscap path Flags... → Handle[...]`**

The FsCap validates that all requested flags are in its `caps` set. If
any flag is unsupported, `open` returns an error. The returned Handle
carries the intersection of the requested flags and the backend's
capabilities.

**`write-file fscap path content@[String Bytes] → null`**

Atomic write: on POSIX, temp file + rename. On S3, a single PUT
request (S3 objects become visible atomically when upload completes,
so `write-file` and `write-file-atomic` have identical semantics on
S3). On WebDAV, a PUT with `Content-Length`.

**`list-dir fscap path → Seq[Dict]`**

Returns a lazy `Seq` of entry dicts. Each entry contains at minimum:

```json
{
  name:  String     # filename only (no directory component)
  type:  String     # "file" | "dir" | "symlink" | "other"
  size:  Int        # bytes; 0 for dirs where size is not meaningful
  mtime: Timestamp  # last modified (from lib-datetime.md)
}
```

Backends that cannot determine a field return `null` for it. On S3,
`type` is always `"file"` (S3 has no real directories; common prefixes
are returned as `{name: "prefix/" type: "dir" size: 0 mtime: null}`
as a convention).

**`stat fscap path → Dict`**

Full metadata dict. All fields are present; backends that cannot
provide a field return `null`:

```json
{
  name:         String      # filename
  type:         String      # "file" | "dir" | "symlink" | "other"
  size:         Int         # bytes
  mtime:        Timestamp   # last modified
  atime:        Timestamp   # last accessed (null on S3/WebDAV)
  ctime:        Timestamp   # last status change (null on S3/WebDAV)
  inode:        Int         # inode number (null on S3/WebDAV)
  nlink:        Int         # hard link count (null on S3/WebDAV)
  mode:         Int         # POSIX permission bits as Int (null on S3/WebDAV)
  uid:          Int         # owner user ID (null on S3/WebDAV)
  gid:          Int         # owner group ID (null on S3/WebDAV)
  etag:         String      # content hash / ETag (null on POSIX unless xattr)
  content-type: String      # MIME type (null on POSIX unless xattr)
}
```

**`make-dir fscap path → null`**

POSIX: `mkdir -p`. WebDAV: `MKCOL`. S3: no-op (S3 has no real
directories; objects with `/` in their key form virtual prefixes). A
user FsCap for S3 should implement `make-dir` as a no-op or as
creating a zero-byte placeholder object (`path/`).

**`remove fscap path → null`**

POSIX: unlink for files, rmdir for empty directories. S3: `DELETE`.
WebDAV: `DELETE`. Errors if path does not exist.

**`rename fscap src dst → null`**

Requires `Atomic` in the FsCap's `caps`. If the FsCap does not declare
`Atomic`, calling `rename` is a type error at the call site — the
caller must use `copy` + `remove` explicitly for non-atomic rename.
POSIX: atomic via `rename(2)`. WebDAV: `MOVE`.

**`copy fscap src dst → null`**

Server-side copy where the backend supports it efficiently (S3:
`CopyObject`; WebDAV: `COPY`; POSIX: `copy_file_range`/`sendfile`).
A FsCap that does not implement `copy` is not required to — callers
that need copy-on-backends-without-it must implement it themselves as
`write-file dst (slurp (open src Readable))`. `copy` is the
optimisation path, not the contract.

**`link fscap src dst → null`**

Requires `Linkable`. Hard link. POSIX only.

**`read-link fscap path → String`**

Requires `Symlinkable`. Returns the symlink target. POSIX only.

**`watch fscap path handler → WatchHandle`**

**Stub — not yet designed.** Requires `Watchable`. Change notification
APIs are highly OS-specific (inotify, kqueue, FSEvents,
ReadDirectoryChangesW, S3 Event Notifications via SNS/SQS) and require
an event-driven execution model that tinct does not yet have. The flag
and method name are reserved. A FsCap that does not support `watch`
should omit the field; calling `watch` on it is a type error.

#### FsCap Capability Matrix

| Backend | Supported flags | Notes |
|---------|----------------|-------|
| Local `DirCap` | All | Full POSIX via `cap_std` |
| S3 bucket | Readable, Writable, Exclusive | No Seekable-write, no Atomic, no links |
| WebDAV | Readable, Writable, Seekable, Atomic, Exclusive | Atomic rename, real dirs, no links |
| NFS (mounted) | All (same as local) | Appears as local path; DirCap suffices |
| User-defined | Any declared subset | Error on undeclared flags/methods |

#### Example — S3 FsCap in pure-tinct

```tinct
# s3-connect returns a FsCap dict over http-connect
[s3: [s3-connect net "my-bucket" aws-creds]]

# Use it anywhere DirCap was accepted:
[config:  [slurp [open s3 "configs/prod.toml" Readable]]]
[write-file s3 "output/result.json" result]

# Atomic create-only (Exclusive → If-None-Match: * header):
[lock: [open s3 "locks/deploy.lock" Writable Exclusive]]
[close lock]

# Server-side copy (S3 CopyObject — no data transferred):
[copy s3 "templates/base.yaml" "envs/prod.yaml"]

# list-dir returns Seq of entry dicts:
[entries: [list-dir s3 "configs/"]]
# → [{name: "prod.toml" type: "file" size: 1234 mtime: <Timestamp>} ...]

# stat returns full metadata:
[meta: [stat s3 "configs/prod.toml"]]
# → {name: "prod.toml" type: "file" size: 1234 mtime: <Timestamp>
#    etag: "\"abc123\"" content-type: "application/toml"
#    atime: null ctime: null inode: null nlink: null mode: null ...}

# These error on S3 (not declared in S3FsCap.caps):
# [open s3 "file.bin" Writable Seekable]   # Seekable-write not in caps
# [rename s3 "old" "new"]                  # Atomic not in caps → type error
# [link s3 "src" "dst"]                    # Linkable not in caps → type error
```

#### Type System

`DirCap` gains a `caps` field internally. The type checker reads it
to enforce constraints: `[open s3-cap path Writable Seekable]` is a
type error if `S3FsCap.caps` does not include `Seekable`. User FsCap
dicts declare their `caps` field; the type checker can inspect this
field if the type of the FsCap value is statically known.

`rename` is typed as requiring a FsCap with `Atomic` in its `caps`.
This makes non-atomic rename a static type error rather than a
runtime surprise.

#### New Rust Builtins for DirCap Extension

| Builtin | Signature | Notes |
|---------|-----------|-------|
| `make-dir cap path` | `DirCap → String → null` | `mkdir -p` semantics |
| `remove cap path` | `DirCap → String → null` | Unlink file or empty dir |
| `rename cap src dst` | `DirCap → String → String → null` | Atomic via `rename(2)` |
| `copy cap src dst` | `DirCap → String → String → null` | `copy_file_range` / `sendfile` |
| `link cap src dst` | `DirCap → String → String → null` | Hard link |
| `read-link cap path` | `DirCap → String → String` | Readlink |
| `list-dir cap path` | `DirCap → String → Seq` | Seq of `{name type size mtime}` dicts |
| `stat cap path` | `DirCap → String → Dict` | Full metadata dict (all fields) |

No new crates — `cap_std` is already present. S3/WebDAV FsCaps are
user-implemented in pure-tinct over `http-connect`.

#### CLI — `--cap-fs`

`--cap-fs NAME=URI` accepts either a bare path or a URI. The scheme
determines what gets injected as `$NAME`:

| Argument form | Injected value | Type |
|---|---|---|
| `--cap-fs fs=/var/data` | DirCap for `/var/data` | `DirCap` |
| `--cap-fs fs=file:///var/data` | DirCap for `/var/data` | `DirCap` |
| `--cap-fs fs=file://.` | DirCap for current directory | `DirCap` |
| `--cap-fs bucket=s3://my-bucket` | Inert URI reference | `Value::Uri` |
| `--cap-fs dav=webdav://dav.internal/files` | Inert URI reference | `Value::Uri` |

**`file://` URIs** are resolved immediately by the CLI into a local
`DirCap` — a POSIX FsCap with all flags (`Readable`, `Writable`,
`Seekable`, `Atomic`, `Linkable`, `Symlinkable`, `Exclusive`, `Sync`,
`NoFollow`, `Watchable`). Bare paths without a scheme are treated as
`file://`.

**Non-`file://` URIs** inject a `Value::Uri { scheme: "s3", uri: "s3://my-bucket" }` — an
inert typed reference that carries no access authority of its own.
It cannot be passed to `open`, `list-dir`, or any FsCap protocol
method directly. The script passes it to a user-provided library
constructor that validates the URI and creates a proper FsCap:

```tinct
[include "lib/s3.llt"]

# tinct run --cap-fs bucket=s3://my-bucket
#           --cap-net aws=*.s3.amazonaws.com
#           script.llt

# $bucket is Value::Uri — inert until activated by a library
[s3: [s3-mount bucket aws aws-creds]]   # Uri + NetCap + creds → FsCap dict

# Now $s3 is a full FsCap:
[config: [slurp [open s3 "configs/prod.toml" Readable]]]
[write-file s3 "outputs/result.json" result]
```

The `NetCap` separately grants network access; the `Value::Uri` merely
identifies what to connect to. Authority comes from the capabilities,
not the URI.

There is no `--cap-s3` or `--cap-webdav` CLI flag — non-POSIX FsCaps
are constructed in tinct code. The CLI's role is to declare the
resource identity (`--cap-fs`) and grant the required access
(`--cap-net`). The script composes them using its own libraries.

### Strings as Character Sequences (`Value::String`)

**The representation change:** replace `Value::String(String)` with
`Value::String { source: Rc<str>, start: usize, end: usize }` — a
shared, zero-copy slice into a reference-counted string buffer.

```rust
// Before
String(String),

// After
String { source: Rc<str>, start: usize, end: usize },
```

Every string in the system is a `(source, start, end)` triple. No
`Value::String(String)` variant remains. When a new string is
constructed (by `str`, `upper`, `split`, etc.), it is allocated once
as a `Rc<str>` and all references into it are zero-copy `String`
slices. The `Rc` is cloned (pointer bump only) each time a slice is
created; the underlying bytes are never copied.

**Character iteration — zero string-data copies.** `[str-chars s]`
produces a lazy `Seq` of `String` slices, each spanning one
Unicode codepoint in the original buffer:

```text
"hello" → String { source: Rc("hello"), start: 0, end: 5 }

str-chars "hello" →
  Seq(
    head: String { source: Rc("hello"), start: 0, end: 1 },  # "h"
    tail: Seq(
      head: String { source: Rc("hello"), start: 1, end: 2 }, # "e"
      ...
    )
  )
```

Each tail step is one `Rc::clone` (pointer bump) + one arena thunk
slot. Zero bytes of string data are copied at any step.

**All collection operations work on strings directly:**

```tinct
[map [fn [c] [upper c]] "hello"]      # → Seq of Strings
[filter [fn [c] [= c "l"]] "hello"]   # → Seq("l" "l")
[first "hello"]                        # → "h"
[nth 2 "hello"]                        # → "l"
[contains? "hello" "l"]                # → true
[length "hello"]                       # → 5  (character count)
```

Result of mapping/filtering a string is a `Seq` of values. To
reassemble a string: `[join "" [map f "hello"]]`.

**Zero-copy `split`.** Because `split` can return `String` slices
into the original `source`, splitting `"a:b:c"` on `":"` yields three
`String` values all sharing the same `Rc<str>` — no new string
allocations for the parts.

**String builtins operate on `&source[start..end]`.** `upper`,
`lower`, `trim`, `replace`, `starts-with?`, `ends-with?` all deref the
`String` to a `&str` slice — same cost as today's `&String` deref.
No reconstruction overhead.

**`str-chars` return type: `Seq` of `String` slices** (not `Dict`,
not `Seq` of new `String`s). `str-slice` and `str-find` are
implemented in terms of `str-chars` and inherit zero-copy behavior.

**Codebase impact.** This replaces every `Value::String` match arm
throughout `src/eval.rs`, `src/builtins.rs`, `src/lib.rs`,
`src/typecheck.rs`, and `src/value.rs`. The skeptic agent confirmed
(reading the actual source) that `Value::String` is the current variant
at `src/value.rs:144` and that a new variant requires explicit match
arms — there is no free ride via existing `Seq` dispatch. This is a
real refactor, not a trivial addition.

**Memory comparison for a 20-char string:**

- Current `Value::String`: 44 bytes, 1 allocation
- `Value::String`: 36-byte `Rc<str>` (header + data) + 16 bytes
  for `(start, end)` in the `Value` enum ≈ same order of magnitude,
  with the benefit that all substrings and character slices share the
  backing allocation

**Type system:** `Type::String` remains unchanged. `String` is a
runtime representation detail; the static type is still `String`.
`str?` returns true for `String`. JSON serialization: `String`
is a string — serializes as a JSON string, not an array.

**Depends on:** `Value::Seq` and `ThunkId` arena infrastructure
(already present). Adds 0 new builtins. No new crates. Requires
replacing all `Value::String` match sites.

### Bytes Type (`Value::Bytes`)

A native binary-data type for byte sequences that have no character
representation: TLS certificates, SSH keys, cryptographic hashes,
serialized protobuf payloads, arbitrary binary file content. Storing
these as strings is wrong — strings carry a UTF-8 validity invariant
that binary data violates. Storing them as `Dict` of `Int` (0–255) is
semantically correct but expensive and awkward to work with.

**New value type: `Value::Bytes { source: Rc<[u8]>, start: usize, end: usize }`**

The same `(source, start, end)` triple pattern as `Value::String`,
backed by `Rc<[u8]>` — a fat pointer (pointer + length) with one
allocation (header + data), no double indirection. Every `Bytes` value
is already a view; there is no separate `BytesView` variant. A whole
buffer is `start=0, end=source.len()`. `bytes-slice` returns
`Bytes { source: Rc::clone(&source), start, end }` — one pointer bump,
zero bytes copied. `Bytes` values are immutable.

**New Rust builtins:**

| Function | Signature | Notes |
|----------|-----------|-------|
| `bytes-length b` | `Bytes → Int` | Number of bytes (not characters) |
| `bytes-get b i` | `Bytes → Int → Int` | Byte at index `i` as Int (0–255) |
| `bytes-slice b start end` | `Bytes → Int → Int → Bytes` | Zero-copy subslice |
| `bytes-concat b1 b2` | `Bytes → Bytes → Bytes` | Concatenate two byte sequences |
| `bytes-equal? b1 b2` | `Bytes → Bytes → Bool` | Structural equality (fast; short-circuits; not constant-time) |
| `ct-equal? b1 b2` | `Bytes → Bytes → Bool` | Constant-time comparison via `subtle::ConstantTimeEq`; use for HMAC/token verification |

**Revised builtins from §Bitwise Primitives:**

`str-bytes` and `bytes-str` now return/accept `Bytes` instead of `Dict`:

| Function | Old signature | New signature |
|----------|--------------|---------------|
| `str-bytes s` | `String → Dict` | `String → Bytes` — UTF-8 encode string to bytes |
| `bytes-str b` | `Dict → String` | `Bytes → String` — UTF-8 decode bytes to string; errors on invalid UTF-8 |

**`stdlib/encoding.llt` revised:**

`base64-encode` and `hex-encode` take `Bytes`, not `String`. To
encode a string's UTF-8 bytes as base64, use `[base64-encode [str-bytes s]]`:

```tinct
# stdlib/encoding.llt (revised)

# base64-encode — encode Bytes as base64 String
base64-encode: [fn [b@Bytes] ...]

# base64-decode — decode base64 String to Bytes; errors on invalid input
base64-decode: [fn [s@String] ...]

# hex-encode — encode Bytes as lowercase hex String
hex-encode: [fn [b@Bytes]
  [join "" [map hex-byte [seq b]]]]   # seq b iterates bytes as Ints

# hex-decode — decode hex String to Bytes; errors on odd length or non-hex chars
hex-decode: [fn [s@String] ...]
```

**Sequence operations.** `Bytes` participates in dual-dispatch for
collection operations — `map`, `filter`, `reduce`, `first`, `last`,
`nth`, `length` — iterating over byte values as `Int` (0–255). This
mirrors how `String` iterates over `String` character slices. The
result of mapping over `Bytes` is a `Seq`, not `Bytes`; to reassemble,
use `bytes-concat` or collect via `bytes-str`.

**JSON serialization.** `Bytes` serializes as a base64-encoded string
(the convention used by Kubernetes, protobuf JSON encoding, and JOSE/JWT).
A `Bytes` value with content `[0xDE 0xAD 0xBE 0xEF]` serializes as
`"3q2+7w=="`. This is reversible via `base64-decode`.

**No literal syntax.** Bytes values are created via conversion:

- `[str-bytes s]` — from a UTF-8 string  
- `[base64-decode s]` — from a base64 string
- `[hex-decode s]` — from a hex string
- `[bytes-slice b start end]` — from an existing Bytes value

**Type system:** `Type::Bytes` — a new type. `bytes?` predicate. Not
a subtype of `String` or `Dict`; not interchangeable without explicit
conversion. `str-bytes`/`bytes-str` are the explicit bridges.

**Adds 5 Rust builtins:** `bytes-length`, `bytes-get`, `bytes-slice`,
`bytes-concat`, `ct-equal?`. Updates `str-bytes`/`bytes-str`
signatures. No new crates.

### Path Utilities (stdlib/path.llt)

Path manipulation is entirely implementable in pure-tinct using `split`
and `join`. POSIX path semantics assumed; Windows paths are out of scope.

```tinct
# stdlib/path.llt — all pure-tinct
path-parts:   [fn [p] [split p "/"]]
basename:     [fn [p] [last [path-parts p]]]
dirname:      [fn [p] [join "/" [rest [reverse [path-parts p]]]]]
extension:    [fn [p] [last [split [basename p] "."]]]
path-join:    [fn [...parts] [join "/" parts]]
```

**Adds 0 Rust builtins.** No new crates.

## What Would Change

### Dependencies (`Cargo.toml`)

`subtle = "1"` — required for `ct-equal?`. Provides `ConstantTimeEq`
which prevents LLVM from optimizing the comparison into a short-circuit
branch. Zero transitive dependencies; widely audited (used by `rustls`,
`ring`, `ed25519-dalek`). All other builtins use only the Rust standard
library.

### New Value Variants (`src/value.rs`)

| Variant | Description |
|---------|-------------|
| `Value::String { source: Rc<str>, start: usize, end: usize }` | Replaces `Value::String(String)` — zero-copy string slices |
| `Value::Bytes { source: Rc<[u8]>, start: usize, end: usize }` | New binary data type |
| `Value::WriteHandle(WriteHandleInner)` | Write-mode file handles; carries text/binary encoding |
| `Value::Uri { scheme: String, uri: String }` | Inert URI reference from `--cap-fs` |

`Value::String` change is a pervasive refactor — every match site in
`eval.rs`, `builtins.rs`, `lib.rs`, `typecheck.rs`, `value.rs` must
be updated. All other new variants are additive.

### Evaluator Builtins (`src/builtins.rs`)

40 new Rust builtins across six groups:

**String (2 to prelude + 1 internal + 1 str-domain):**
`starts-with?`, `ends-with?` (move to prelude, gain `Seq`/`Bytes`
dual-dispatch), `str-chars` (internal), `str-slice` (O(1) String
construction).

**Math (16):**
`pow`, `sqrt`, `log`, `log2`, `log10`, `exp`,
`sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`,
`nan?`, `inf?`, `finite?`.
`pi`, `e`, `phi` are Float literals in `stdlib/math.llt`.

**Bitwise (9):**
`band`, `bor`, `bxor`, `shl`, `shr`,
`char-code`, `chr`, `str-bytes`, `bytes-str`.

**I/O (6):**
`write`, `flush`, `close`, `seek`, `seek-end`, `position`.
`open` is extended (not new) — gains capability flag dispatch.

**Bytes (4):**
`bytes`, `bytes-find`, `bytes-of`, `bytes-equal?`, `ct-equal?`.
`split`, `replace`, `join`, `contains?`, `length`, `get`, `nth`,
`slice`, `take`, `drop` gain `Bytes` dispatch in the prelude — no
new builtins, dispatch changes only.

**FsCap / DirCap extension (8):**
`make-dir`, `remove`, `rename`, `copy`, `link`, `read-link`,
`list-dir`, `stat`.

### Standard Library Files

**New files:**

- `stdlib/strings.llt` — `str-contains?`, `pad-left`, `pad-right`,
  `str-repeat`, `str-find`, `str-reverse`
  (`starts-with?`/`ends-with?` move to prelude; `str-slice` subsumed
  by generalized `slice`; `str-chars` is internal-only)
- `stdlib/math.llt` — `pi`, `e`, `phi` (Float literals), `hypot`,
  `deg->rad`, `rad->deg`, `log-base`
- `stdlib/encoding.llt` — `base64-encode`, `base64-decode`,
  `hex-encode`, `hex-decode`, `bytes-reverse`, `bytes-repeat`,
  `mask-apply` (pure-tinct on bitwise primitives; encoding functions
  take/return `Bytes`)
- `stdlib/toml-lite.llt` — `parse-toml-lite: [fn [s@String] → Dict]`
- `stdlib/in/toml-lite.llt` — `[parse-toml-lite [slurp stdin]]`
- `stdlib/path.llt` — `basename`, `dirname`, `extension`,
  `path-join`, `path-parts`
- `stdlib/regex.llt` — see `doc/whatif/lib-regex.md`

**Extended files:**

- `stdlib/prelude.llt` — gains `starts-with?`, `ends-with?` (now
  multi-dispatch on `String`/`Bytes`/`Seq`); `slice`, `take`, `drop`,
  `count`, `reverse`, `contains?`, `get`, `nth`, `length` gain
  `String` and `Bytes` dual-dispatch
- `stdlib/io.llt` — gains `write-line`; `write-file`/`write-file-atomic`
  signatures extended to `content@[String Bytes]`; `list-dir`, `stat`,
  `make-dir`, `remove`, `rename`, `copy`, `link`, `read-link` for
  local `DirCap`; `open` now takes explicit capability flags (no mode
  strings)

`strings.llt`, `math.llt`, and `encoding.llt` are loaded at startup
alongside `prelude.llt`. `toml-lite.llt` is opt-in (`$include`
explicitly). `stdlib/in/toml-lite.llt` is available as `-i toml-lite`.

### Type Checker (`src/typecheck.rs`)

**New type definitions:**

```tinct
# Type aliases using [type ...] syntax.
# These would be declared in a stdlib/io.llt types block.
[
  # Inert URI reference — not a FsCap; must be activated by a library constructor
  Uri: [type [scheme: @String  uri: @String]]

  # Directory listing entry (returned per item by list-dir)
  DirEntry: [type [
    name:  @String            # filename only (no directory component)
    type:  @String            # "file" | "dir" | "symlink" | "other"
    size:  @Integer               # bytes
    mtime: @[Timestamp Null]  # last modified; null if unavailable
  ]]

  # Full file metadata (returned by stat)
  StatResult: [type [
    name:         @String
    type:         @String           # "file" | "dir" | "symlink" | "other"
    size:         @Integer
    mtime:        @[Timestamp Null]
    atime:        @[Timestamp Null]  # null on S3/WebDAV
    ctime:        @[Timestamp Null]  # null on S3/WebDAV
    inode:        @[Int Null]        # null on S3/WebDAV
    nlink:        @[Int Null]        # null on S3/WebDAV
    mode:         @[Int Null]        # POSIX permission bits; null on S3/WebDAV
    uid:          @[Int Null]        # null on S3/WebDAV
    gid:          @[Int Null]        # null on S3/WebDAV
    etag:         @[String Null]     # content hash; null on POSIX
    content-type: @[String Null]     # MIME type; null on POSIX
  ]]

  # FsCap protocol type (structural — any dict with these fields)
  FsCap: [type [
    caps:       @[Seq Any]    # declared capability flags (nominal variants)
    open:       [fn@Handle       [path@String]]
    write-file: [fn@Null         [path@String  content@[String Bytes]]]
    list-dir:   [fn@[Seq DirEntry] [path@String]]
    stat:       [fn@StatResult   [path@String]]
    make-dir:   [fn@Null         [path@String]]
    remove:     [fn@Null         [path@String]]
    rename:     [fn@Null         [src@String  dst@String]]
    copy:       [fn@Null         [src@String  dst@String]]
    link:       [fn@Null         [src@String  dst@String]]
    read-link:  [fn@String       [path@String]]
    watch:      [fn@Any          [path@String  handler@Fn]]   # stub
  ]]
]

# Handle capability rows: nominal cap variants compose the row
# Handle[Readable Writable Stream ...]  — any subset of cap variants
# WriteHandle[Text] / WriteHandle[Binary] — write-mode, encoding-tagged
```

**New builtin signatures in `TypeEnv::with_builtins()`:**

```tinct
# Prelude additions (multi-dispatch: String | Bytes | Seq)
starts-with? : [fn@Boolean         [prefix@[String Bytes Seq]  haystack@[String Bytes Seq]]]
ends-with?   : [fn@Boolean         [suffix@[String Bytes Seq]  haystack@[String Bytes Seq]]]

# String-domain
str-chars : [fn@[Seq String]  [s@String]]          # internal; Seq of single-char String slices
str-slice : [fn@String      [from@Integer  to@Integer  s@String]]  # O(1) zero-copy substring

# Math
pow    : [fn@Float  [base@Number  exp@Number]]
sqrt   : [fn@Float  [x@Float]]
log    : [fn@Float  [x@Float]]    # log2, log10, exp analogous
sin    : [fn@Float  [x@Float]]    # cos, tan, asin, acos, atan analogous
atan2  : [fn@Float  [y@Float  x@Float]]
nan?   : [fn@Boolean   [x@Float]]
inf?   : [fn@Boolean   [x@Float]]
finite?: [fn@Boolean   [x@Float]]
# pi, e, phi are Float literals in math.llt, not registered builtins

# Bitwise primitives
band      : [fn@Integer     [a@Integer  b@Integer]]
bor       : [fn@Integer     [a@Integer  b@Integer]]
bxor      : [fn@Integer     [a@Integer  b@Integer]]
shl       : [fn@Integer     [a@Integer  n@Integer]]
shr       : [fn@Integer     [a@Integer  n@Integer]]
char-code : [fn@Integer     [s@String]]   # Unicode codepoint of first char
chr       : [fn@String  [n@Integer]]      # single-char string for codepoint
str-bytes : [fn@Bytes   [s@String]]   # UTF-8 encode
bytes-str : [fn@String  [b@Bytes]]    # UTF-8 decode; errors on invalid UTF-8

# Bytes
bytes       : [fn@Bytes  [...@Bytes]]              # variadic concat; mirrors str
bytes-find  : [fn@Integer    [pattern@Bytes  b@Bytes]] # byte index, or -1
bytes-of    : [fn@Bytes  [seq@[Seq Int]]]           # collect byte Ints (0-255)
bytes-equal?: [fn@Boolean  [b1@Bytes  b2@Bytes]]   # fast structural equality (short-circuits)
ct-equal?:    [fn@Boolean  [b1@Bytes  b2@Bytes]]   # constant-time (subtle::ConstantTimeEq); use for secrets

# I/O (WriteHandle encoding is tracked in the type)
write    : [fn@WriteHandle  [wh@WriteHandle  content@[String Bytes]]]
           # content type must match wh encoding (String for Text, Bytes for Binary)
flush    : [fn@WriteHandle  [wh@WriteHandle]]
close    : [fn@Null         [wh@WriteHandle]]
seek     : [fn@Handle       [h@Handle  offset@Integer]]   # h must carry Seekable
seek-end : [fn@Handle       [h@Handle]]               # h must carry Seekable
position : [fn@Integer          [h@Handle]]               # h must carry Seekable

# FsCap DirCap extension
make-dir  : [fn@Null         [cap@DirCap  path@String]]
remove    : [fn@Null         [cap@DirCap  path@String]]
rename    : [fn@Null         [cap@DirCap  src@String  dst@String]]  # cap must declare Atomic
copy      : [fn@Null         [cap@DirCap  src@String  dst@String]]
link      : [fn@Null         [cap@DirCap  src@String  dst@String]]  # cap must declare Linkable
read-link : [fn@String       [cap@DirCap  path@String]]             # cap must declare Symlinkable
list-dir  : [fn@[Seq DirEntry] [cap@DirCap  path@String]]
stat      : [fn@StatResult   [cap@DirCap  path@String]]
```

## Dependencies

**Between sections in this document:**

- §TOML Parsing Lite requires `starts-with?`, `ends-with?`, `trim`
  from §Extended String Utilities.
- §Strings as Character Sequences (`Value::String` refactor) makes
  `str-chars` return a `Seq` of `String` slices — `str-find` and the
  internal implementation of `str-slice` depend on this. All other
  sections are independent of the representation change.
- §Bytes Type requires `Value::Bytes` (new variant) and `Type::Bytes`.
  `str-bytes`/`bytes-str` signatures change from `Dict` to `Bytes`;
  `encoding.llt` must be updated accordingly. `bytes-reverse` and
  `bytes-repeat` depend on `bytes-of`.
- §Streaming File I/O and §Filesystem Capabilities share the `open`
  capability-flag infrastructure. `Value::WriteHandle` is required by
  Streaming I/O; `Value::Uri` is required by the FsCap CLI (`--cap-fs`
  with non-`file://` URIs).
- §Extended Math and §Bitwise Primitives are independent of each other
  and of §Extended String Utilities.
- §Path Utilities has no dependencies.

**On other whatif documents:**

- `doc/whatif/lib-regex.md` requires `str-chars` (§Extended String
  Utilities) and `char-code` (§Bitwise Primitives).
- §Filesystem Capabilities `stat` return dict includes `Timestamp`
  fields — depends on `doc/whatif/lib-datetime.md` for the
  `Timestamp` type (`mtime`, `atime`, `ctime`). Where lib-datetime
  is not implemented, these fields return `null`.
- User-defined FsCaps (S3, WebDAV) are built in tinct over
  `http-connect` — depends on `doc/whatif/lib-tls.md` Connector
  protocol and `HttpConn`.

**Stubs:**

- `Watchable` flag and `watch` protocol method are reserved with no
  implementation — requires a future event-driven execution model.

## References

- Jsonnet Standard Library. jsonnet.org/ref/stdlib.html. —
  `std.base64()`, `std.manifestYamlDoc()`, `std.parseInt()` as
  precedent for essential encoding and serialization builtins.
- Nickel Standard Library. nickel-lang.org/stdlib/. —
  `std.string` (30+ functions) and `std.number` module for trig/math
  coverage.
- Nix Built-in Functions. nix.dev/manual/nix/2.26/language/builtins.
  — `lib.strings` (50+ functions including `escapeShellArg`,
  `escapeRegex`, `versionOlder`) as reference for config-specific
  string utilities.
- CUE Standard Library. cuelang.org/docs/tour/packages/standard-library/. —
  `math` package, `encoding/base64` package as examples of a
  comprehensive modular stdlib for configuration languages.
- Josefsson, S. (2006). "The Base16, Base32, and Base64 Data
  Encodings." RFC 4648. — Normative reference for the base64 alphabet
  and padding behavior used in `stdlib/encoding.llt`.
- Coutts, R., Leshchinskiy, R. & Stewart, D. (2007). "Stream Fusion:
  From Lists to Streams to Nothing at All." *ICFP 2007*. — The formal
  model behind `Value::String`: a (ptr, offset, length) triple
  sharing one buffer, avoiding per-character allocation. GHC's
  `Data.ByteString` implements this directly; `Value::String` is
  the same structure adapted to tinct's `Rc<str>` ownership model.
- Berners-Lee, T., Fielding, R. & Masinter, L. (2005). "Uniform
  Resource Identifier (URI): Generic Syntax." RFC 3986. — URI scheme
  syntax used by `--cap-fs` for non-`file://` backends.
- Hoffman, P. & Masinter, L. (2016). "The file URI Scheme." RFC 8089.
  — Normative reference for `file://` URI resolution in `--cap-fs`.
- The Open Group (2018). "The Single UNIX Specification, Version 4." —
  POSIX filesystem semantics (`rename(2)`, `link(2)`, `symlink(2)`,
  `stat(2)`, `O_*` flags) that define the local `DirCap` FsCap.
- Amazon Web Services. "Amazon S3 API Reference." — S3 operations
  mapped to FsCap protocol: `GetObject` → `open Readable`, `PutObject`
  → `write-file`, `CopyObject` → `copy`, `DeleteObject` → `remove`,
  `ListObjectsV2` → `list-dir`, `If-None-Match: *` → `Exclusive`.
- RFC 4918. Dusseault, L. (2007). "HTTP Extensions for Web Distributed
  Authoring and Versioning (WebDAV)." IETF. — WebDAV method mapping:
  `GET` → `open Readable`, `PUT` → `write-file`, `COPY` → `copy`,
  `MOVE` → `rename`, `MKCOL` → `make-dir`, `DELETE` → `remove`.
- Miller, M.S. (2006). *Robust Composition*. — Object-capability model
  underpinning FsCap: capabilities as unforgeable references, attenuation
  via narrowing, `Value::Uri` as inert non-capability reference.
- doc/whatif/lib-datetime.md — `Timestamp` type used in `stat` return
  dict (`mtime`, `atime`, `ctime`).
- doc/whatif/lib-regex.md — regex engine that depends on `str-chars`
  and `char-code` from this document.
- doc/whatif/lib-tls.md — Connector protocol and `http-connect` used
  by user-defined FsCaps (S3, WebDAV) to make HTTP requests.
