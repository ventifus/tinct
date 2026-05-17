# Supplemental Standard Library Modules

## Overview

tinct ships supplemental standard library modules beyond the core
prelude, covering extended strings, math, bitwise encoding, TOML
parsing, streaming file I/O, string-as-sequence operations, a native
`Bytes` type, and a generalised filesystem capability protocol (`FsCap`)
supporting S3, WebDAV, and other object-store backends alongside the
local POSIX filesystem.

Serialization helpers such as `yaml-quote-string`, `toml-escape`, and
`nginx-escape` reduce to one-line predicates rather than chains of
`split` checks. Mathematical configuration involving subnets, frequency
ratios, and calibration curves is expressible. Base64 and hex encoding
are pure-tinct library functions built on bitwise primitives. tinct
scripts read TOML configuration files directly. The peer configuration
languages — Jsonnet, Nickel, Nix, CUE — all treat these capabilities as
non-negotiable.

## Design

Supplemental modules ship in two categories:

| Category | Implementation | Crate dependency |
|----------|----------------|-----------------|
| Pure-tinct | `stdlib/*.llt` | None |
| New Rust builtin | `src/builtins.rs` | None |

No new crates are introduced by any of the proposals in this document
except `subtle` for `ct-equal?`.

### Module Survey

Comparable configuration languages cover these domains in their
standard libraries:

| Feature | Jsonnet | Nickel | Nix | CUE | tinct |
|---------|---------|--------|-----|-----|-------|
| String search (`contains`, `starts-with`) | ✓ | ✓ | ✓ | ✓ | ✓ |
| Regex | `std.native()` | ✓ | ✓ | ✓ | see lib-regex.md |
| Math (`pow`, `sqrt`, trig) | ✓ | ✓ | partial | ✓ full | ✓ |
| Base64 / encoding | ✓ | ✓ | ✗ | ✓ | ✓ |
| Path utilities | ✗ | ✗ | ✓ `lib` | ✓ `path` | ✓ |
| Date/time | ✗ | ✗ | ✗ | ✓ `time` | see lib-datetime.md |

### Extended String Utilities

**Rust builtins in prelude:**

`starts-with?` and `ends-with?` generalize to any sequence — they are
not string-specific. A string's character-Seq participates via
dual-dispatch, so `[starts-with? "he" "hello"]` and
`[starts-with? [1 2] [1 2 3 4]]` both work through the same builtin.
They belong alongside `contains?` in the prelude, not in a string
module.

| Function | Signature | Notes |
|----------|-----------|-------|
| `starts-with?` | `Seq\|String → Seq\|String → Bool` | `starts-with? prefix haystack`; in prelude |
| `ends-with?` | `Seq\|String → Seq\|String → Bool` | `ends-with? suffix haystack`; in prelude |

**Rust builtin in string domain:**

| Function | Signature | Notes |
|----------|-----------|-------|
| `str-slice` | `Int → Int → String → String` | O(1) `String` construction; `str-slice from to s` |

`str-slice` directly constructs `String { source: Rc::clone(&source), start: byte_of(start), end: byte_of(end) }` — constant time, zero allocation.

**`str-chars` — internal implementation primitive.** With strings
participating directly in `map`/`filter`/`first`/`nth` via
dual-dispatch, `str-chars` is not a recommended user-facing function.
It remains as a Rust builtin used internally by `str-find` but is not
exported from `stdlib/strings.llt`. Users who want a Seq of characters
write `[map [fn [c] c] s]` or use string operations directly.

**Pure-tinct additions to `stdlib/strings.llt`:**

```tinct
# stdlib/strings.llt

# str-contains? — true if needle appears anywhere in haystack
str-contains?: [fn@Bool [needle@String haystack@String]
  [> [length [split haystack needle]] 1]]

# pad-left — left-pad s to width with spaces
pad-left: [fn@String [width@Int s@String]
  [str
    [join "" [take [max 0 [- width [str-length s]]] [repeat " "]]]
    s]]

# pad-right — right-pad s to width with spaces
pad-right: [fn@String [width@Int s@String]
  [str s
    [join "" [take [max 0 [- width [str-length s]]] [repeat " "]]]]]

# str-repeat — repeat s n times
str-repeat: [fn@String [n@Int s@String] [join "" [take n [repeat s]]]]

# str-find — character index of first occurrence of needle, or -1
str-find: [fn@Int [needle@String haystack@String]
  [if [str-contains? needle haystack]
    [str-length [first [split haystack needle]]]
    -1]]

# str-reverse — reverse a string character by character
str-reverse: [fn@String [s@String]
  [join "" [reverse s]]]

# str-take — first n characters
str-take: [fn@String [n@Int s@String]
  [join "" [take n s]]]

# str-drop — drop first n characters
str-drop: [fn@String [n@Int s@String]
  [join "" [drop n s]]]

# str-count — count characters matching predicate
str-count: [fn@Int [pred@Fn s@String]
  [count [filter pred s]]]
```

`str-find` returns a character offset — correct for ASCII; for
multi-byte Unicode it returns the character count of the prefix before
the match, not the byte offset.

**Adds 3 Rust builtins:** `starts-with?` (to prelude), `ends-with?`
(to prelude), `str-slice`. `str-chars` stays as an internal builtin,
not exported.

### Extended Math Builtins (stdlib/math.llt)

Missing functions are all trivial wrappers around Rust's `f64` methods — no new crate needed.

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
functions operate in radians. Degree conversion is a pure-tinct helper.

**Adds 13 Rust builtins.** No new crates.

### Bitwise Primitives (stdlib/encoding.llt)

Rather than shipping specific encoding builtins, this section provides
the primitive bitwise operations from which users implement base64, hex,
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

**`str-bytes s`** — UTF-8 byte sequence of `s` as a `Bytes` value.
Each byte of the UTF-8 encoding, not one Unicode character. For ASCII
strings, `char-code` and `str-bytes` agree; for multi-byte Unicode they
differ.

**`bytes-str bytes`** — string whose UTF-8 encoding is the given `Bytes`
value. Inverse of `str-bytes`. Errors on invalid UTF-8.

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

# base64-encode — encode Bytes as base64 String
base64-encode: [fn [b@Bytes] ...]

# base64-decode — decode base64 String to Bytes; errors on invalid input
base64-decode: [fn [s@String] ...]

# hex-encode — encode Bytes as lowercase hex String
hex-encode: [fn [b@Bytes]
  [join "" [map hex-byte [seq b]]]]

# hex-decode — decode hex String to Bytes; errors on odd length or non-hex chars
hex-decode: [fn [s@String] ...]

# subnet mask application (common config use case)
mask-apply: [fn [ip mask] [band ip mask]]
```

The nine Rust builtins — five bitwise ops, two char↔code conversions,
two string↔bytes conversions — are each independently useful and compose
freely.

**`char-code` is also required by `doc/feature/lib-regex.md`** for
character class range comparisons.

**`HashAlgorithm` type alias** — used by `doc/feature/lib-tls.md` for
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
| Datetime | ✗ | Use parse-timestamp from lib-datetime.md |

**Implementation:** fold over `split content "\n"`, carrying
`{section: Str, tables: Dict}` accumulator state. Uses `starts-with?`
for line-prefix matching.

**Depends on:** `starts-with?`, `ends-with?`, `trim` from §Extended
String Utilities. **Adds 0 Rust builtins.** No new crates.

### Streaming File I/O (WriteHandle)

`write-file` and `write-file-atomic` are atomic operations: the full
content string is passed at once. This is the right default for
configuration output. Scripts that build output incrementally — writing
lines one at a time to a log or report file — use a split open/write/close
model.

**New value type: `Value::WriteHandle`** — wraps `Box<dyn Write>` (a
`BufWriter<File>`). Returned by `open` when mode is `"w"` (truncate)
or `"a"` (append). The type system exposes this as `WriteHandle`.

**New Rust builtins:**

**`write wh str`** — writes `str` to the `WriteHandle`, returns the
`WriteHandle` for chaining. Does not flush.

**`flush wh`** — flushes the `WriteHandle`'s buffer to the OS, returns
`wh`. Use before reading the file in the same script.

**`close wh`** — flushes and closes the `WriteHandle`, returns `null`.
After `close`, the `WriteHandle` is invalid; further writes are errors.

**`open`** already exists and returns `Handle` for `"r"` mode. It is
extended to return `WriteHandle` for `"w"` and `"a"` modes. The
returned type differs by mode — the type checker enforces this
statically once `Type::WriteHandle` is added.

**`stdlib/io.llt` additions:**

```tinct
# Open a file for writing (truncates existing content).
open-write: [fn@WriteHandle [cap@DirCap path@String]
  [open cap path "w"]]

# Open a file for appending.
open-append: [fn@WriteHandle [cap@DirCap path@String]
  [open cap path "a"]]

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

`DirCap` is a built-in capability granting access to a local filesystem
directory — hardwired to POSIX paths via `cap_std`. The **FsCap
protocol** generalises it: any value that implements the required methods
is accepted wherever a `DirCap` is accepted. S3 buckets, WebDAV servers,
and user-defined virtual filesystems are drop-in replacements.

#### Protocol Declaration

A FsCap is a tinct Dict with a `caps` field declaring the capability
flags it supports, plus one field per implemented method. The tinct
evaluator dispatches protocol method calls by looking up the field
name in the FsCap dict.

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
| `Watchable` | — | ✓ (inotify) | ✓ (events, async) | ✗ | ✓ | Requires event-driven model |

#### Protocol Methods

**`open fscap path Flags... → Handle@[...]`**

The FsCap validates that all requested flags are in its `caps` set. If
any flag is unsupported, `open` returns an error. The returned Handle
carries the intersection of the requested flags and the backend's
capabilities.

**`write-file fscap path content@[String Bytes] → null`**

Atomic write: on POSIX, temp file + rename. On S3, a single PUT
request. On WebDAV, a PUT with `Content-Length`.

**`list-dir fscap path → [Seq Dict]`**

Returns a lazy `Seq` of entry dicts. Each entry contains at minimum:

```
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

```
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
directories; objects with `/` in their key form virtual prefixes).

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
`write-file dst (slurp (open src Readable))`.

**`link fscap src dst → null`**

Requires `Linkable`. Hard link. POSIX only.

**`read-link fscap path → String`**

Requires `Symlinkable`. Returns the symlink target. POSIX only.

**`watch fscap path handler → WatchHandle`**

Requires `Watchable`. Change notification APIs are highly OS-specific and require an event-driven execution model. The flag and method name are reserved for this purpose.

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
```

#### Type System

`DirCap` carries a `caps` field internally. The type checker reads it
to enforce constraints: `[open s3-cap path Writable Seekable]` is a
type error if `S3FsCap.caps` does not include `Seekable`. User FsCap
dicts declare their `caps` field; the type checker inspects this
field when the type of the FsCap value is statically known.

`rename` is typed as requiring a FsCap with `Atomic` in its `caps`.
Non-atomic rename is a static type error rather than a runtime surprise.

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
`DirCap`. Bare paths without a scheme are treated as `file://`.

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
are constructed in tinct code.

### Strings as Character Sequences (`Value::String`)

**The representation:** `Value::String { source: Rc<str>, start: usize, end: usize }` — a
shared, zero-copy slice into a reference-counted string buffer.

Every string is a `(source, start, end)` triple. When a new string is
constructed (by `str`, `upper`, `split`, etc.), it is allocated once
as a `Rc<str>` and all references into it are zero-copy `String`
slices. The `Rc` is cloned (pointer bump only) each time a slice is
created; the underlying bytes are never copied.

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

**Zero-copy `split`.** `split "a:b:c" ":"` yields three `String`
values all sharing the same `Rc<str>` — no new string allocations for the parts.

**Type system:** `Type::String` remains unchanged. `str?` returns true.
JSON serialization: `String` is a string — serializes as a JSON string, not an array.

### Bytes Type (`Value::Bytes`)

A native binary-data type for byte sequences that have no character
representation: TLS certificates, SSH keys, cryptographic hashes,
serialized protobuf payloads, arbitrary binary file content.

**New value type: `Value::Bytes { source: Rc<[u8]>, start: usize, end: usize }`**

The same `(source, start, end)` triple pattern as `Value::String`,
backed by `Rc<[u8]>`. `bytes-slice` returns a zero-copy subview — one
pointer bump, zero bytes copied. `Bytes` values are immutable.

**New Rust builtins:**

| Function | Signature | Notes |
|----------|-----------|-------|
| `bytes-length b` | `Bytes → Int` | Number of bytes (not characters) |
| `bytes-get b i` | `Bytes → Int → Int` | Byte at index `i` as Int (0–255) |
| `bytes-slice b start end` | `Bytes → Int → Int → Bytes` | Zero-copy subslice |
| `bytes-concat b1 b2` | `Bytes → Bytes → Bytes` | Concatenate two byte sequences |
| `bytes-equal? b1 b2` | `Bytes → Bytes → Bool` | Structural equality (fast; short-circuits; not constant-time) |
| `ct-equal? b1 b2` | `Bytes → Bytes → Bool` | Constant-time comparison via `subtle::ConstantTimeEq`; use for HMAC/token verification |

**`str-bytes` and `bytes-str`** take/return `Bytes` (not `Dict`).

**`stdlib/encoding.llt` revised:**

`base64-encode` and `hex-encode` take `Bytes`. To encode a string's
UTF-8 bytes as base64: `[base64-encode [str-bytes s]]`.

**Sequence operations.** `Bytes` participates in dual-dispatch for
collection operations — `map`, `filter`, `reduce`, `first`, `last`,
`nth`, `length` — iterating over byte values as `Int` (0–255).

**JSON serialization.** `Bytes` serializes as a base64-encoded string
(the convention used by Kubernetes, protobuf JSON encoding, and JOSE/JWT).

**No literal syntax.** Bytes values are created via conversion:
- `[str-bytes s]` — from a UTF-8 string
- `[base64-decode s]` — from a base64 string
- `[hex-decode s]` — from a hex string
- `[bytes-slice b start end]` — from an existing Bytes value

**Type system:** `Type::Bytes` — a new type. `bytes?` predicate. Not
a subtype of `String` or `Dict`; not interchangeable without explicit
conversion.

**Adds 5 Rust builtins:** `bytes-length`, `bytes-get`, `bytes-slice`,
`bytes-concat`, `ct-equal?`. Updates `str-bytes`/`bytes-str`
signatures. No new crates.

### Path Utilities (stdlib/path.llt)

Path manipulation is entirely implementable in pure-tinct. POSIX path
semantics assumed; Windows paths are out of scope.

```tinct
# stdlib/path.llt — all pure-tinct
path-parts:   [fn [p] [split p "/"]]
basename:     [fn [p] [last [path-parts p]]]
dirname:      [fn [p] [join "/" [rest [reverse [path-parts p]]]]]
extension:    [fn [p] [last [split [basename p] "."]]]
path-join:    [fn [...parts] [join "/" parts]]
```

**Adds 0 Rust builtins.** No new crates.

## Implementation

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

**Bytes (4+1):**
`bytes`, `bytes-find`, `bytes-of`, `bytes-equal?`, `ct-equal?`.
`split`, `replace`, `join`, `contains?`, `length`, `get`, `nth`,
`slice`, `take`, `drop` gain `Bytes` dispatch — no new builtins,
dispatch changes only.

**FsCap / DirCap extension (8):**
`make-dir`, `remove`, `rename`, `copy`, `link`, `read-link`,
`list-dir`, `stat`.

### Standard Library Files

**New files:**
- `stdlib/strings.llt` — `str-contains?`, `pad-left`, `pad-right`,
  `str-repeat`, `str-find`, `str-reverse`
- `stdlib/math.llt` — `pi`, `e`, `phi` (Float literals), `hypot`,
  `deg->rad`, `rad->deg`, `log-base`
- `stdlib/encoding.llt` — `base64-encode`, `base64-decode`,
  `hex-encode`, `hex-decode`, `bytes-reverse`, `bytes-repeat`,
  `mask-apply`
- `stdlib/toml-lite.llt` — `parse-toml-lite: [fn [s@String] → Dict]`
- `stdlib/in/toml-lite.llt` — `[parse-toml-lite [slurp stdin]]`
- `stdlib/path.llt` — `basename`, `dirname`, `extension`,
  `path-join`, `path-parts`
- `stdlib/regex.llt` — see `doc/feature/lib-regex.md`

**Extended files:**
- `stdlib/prelude.llt` — gains `starts-with?`, `ends-with?` (now
  multi-dispatch on `String`/`Bytes`/`Seq`); `slice`, `take`, `drop`,
  `count`, `reverse`, `contains?`, `get`, `nth`, `length` gain
  `String` and `Bytes` dual-dispatch
- `stdlib/io.llt` — gains `write-line`; `write-file`/`write-file-atomic`
  signatures extended to `content@[String Bytes]`; `list-dir`, `stat`,
  `make-dir`, `remove`, `rename`, `copy`, `link`, `read-link` for
  local `DirCap`; `open` now takes explicit capability flags

`strings.llt`, `math.llt`, and `encoding.llt` are loaded at startup
alongside `prelude.llt`. `toml-lite.llt` is opt-in (`$include`
explicitly). `stdlib/in/toml-lite.llt` is available as `-i toml-lite`.

### Type Checker (`src/typecheck.rs`)

**New type definitions:**

```tinct
[
  Uri: [type [scheme: @String  uri: @String]]

  DirEntry: [type [
    name:  @String
    type:  @String            # "file" | "dir" | "symlink" | "other"
    size:  @Int
    mtime: @[Timestamp Null]
  ]]

  StatResult: [type [
    name:         @String
    type:         @String
    size:         @Int
    mtime:        @[Timestamp Null]
    atime:        @[Timestamp Null]
    ctime:        @[Timestamp Null]
    inode:        @[Int Null]
    nlink:        @[Int Null]
    mode:         @[Int Null]
    uid:          @[Int Null]
    gid:          @[Int Null]
    etag:         @[String Null]
    content-type: @[String Null]
  ]]

  FsCap: [type [
    caps:       @[Seq Any]
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
    watch:      [fn@Any          [path@String  handler@Fn]]
  ]]
]
```

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
  sharing one buffer, avoiding per-character allocation.
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
- doc/feature/lib-datetime.md — `Timestamp` type used in `stat` return
  dict (`mtime`, `atime`, `ctime`).
- doc/feature/lib-regex.md — regex engine that depends on `str-chars`
  and `char-code` from this document.
- doc/feature/lib-tls.md — Connector protocol and `http-connect` used
  by user-defined FsCaps (S3, WebDAV) to make HTTP requests.
