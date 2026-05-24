# General I/O

## Overview

tinct's I/O model covers file reads and writes, network requests, environment
variables, and stdin — consistent with its lazy call-by-need semantics and the
object capability model (Miller 2006).

tinct lives at system boundaries: it generates YAML for Kubernetes, reads
secrets from files, calls APIs to validate schemas. The I/O model makes these
operations first-class:

- **Shell pipelines compose naturally.** A tinct program reads config files,
  fetches a schema from a URL, and emits YAML in one evaluation — no external
  shell wiring needed.
- **`emit` unlocks formatters.** Without `emit`, tinct produces only JSON.
  The formatter model depends entirely on `emit`.
- **`stdin` enables Unix-style composability.** `llt eval process.llt` reading
  from stdin composes naturally with grep, jq, and other Unix tools.
- **TCP primitives enable `fetch` as library code.** With `connect` and `tls`
  as Rust primitives, `fetch` is tinct code — no dedicated Rust HTTP builtin
  needed.

All phases are implemented: `io-phase1` through `io-phase4` and
`io-include-cap`.

## Design

### The Pragmatic Model: Strict I/O Builtins

tinct adopts the same model as Nix and Dhall: **I/O builtins are strict
functions that execute immediately when forced, return pure values, and do not
require monadic infrastructure**. This is the correct model for a lazy
call-by-need configuration language. The formal justification is in §Formal
Grounding below.

Each I/O builtin:

- Is **strict in its arguments** — arguments are materialized before the I/O
  operation executes
- **Returns a pure tinct value** — the result (string, dict, seq, or an opaque
  handle/cap) is a first-class value with no residual I/O structure
- **Executes at force time** — in a lazy binding, it runs when the binding is
  first forced; in a pipeline stage, it runs when the stage is evaluated

### Capability-Based I/O

All file and network I/O flows through **capability values** — opaque,
unforgeable tinct values that represent authority over a resource. There is no
ambient `open-file path` that any code can call: opening a resource requires a
capability, and capabilities are explicitly received (passed as arguments or
injected by the CLI). This is the object capability model (Miller 2006) applied
to tinct's I/O layer.

Four capability types plus a revocable wrapper:

**`Value::DirCap`** — authority to open files within a directory tree. Wraps
`cap_std::fs::Dir`. On Linux 5.6+, `cap_std` uses the `openat2(RESOLVE_BENEATH)`
syscall, making path traversal (`../`) and symlink escapes structurally
impossible at the kernel level. On older kernels and macOS, `cap_std` falls
back to a userspace emulation that validates each path component individually;
the security property holds in both paths.

**`Value::NetCap`** — authority to open TCP/TLS connections to a specified set
of hosts and subnets. The allowlist may contain exact hostnames, hostname:port
pairs, and IPv4/IPv6 CIDR ranges. See §NetCap Allowlist Specification.

**`Value::Handle`** — authority to read from and write to one specific open
resource (a file or socket). Created by `open`, `connect`, or `tls`; received
from the runtime as `stdin`. A `Handle` is itself a capability — more narrowly
scoped than a `DirCap` (one file vs. a whole directory tree). In the type system,
`Handle` is parameterized by a capability row: `Type::Handle(Box<Type>)` where
the inner type describes capabilities like `Readable`, `Writable`, `Binary`, etc.
`Handle[Readable]` means a read-only handle, `Handle[Writable]` means write-only.
The gradual type `Handle` (no parameters shown) is syntactic sugar for
`Handle[Unknown]` — unknown capabilities at compile time.

**`Value::RevocableDirCap`** — a `DirCap` wrapper that can be invalidated after
the fact. See §Handle Revocation.

Three values are automatically injected into the root environment by the
runtime:

**`pwd`** — a `DirCap` for the working directory at the time `llt eval` is
invoked. Used for project-local file access: `[include pwd "config.llt"]`,
`[open pwd "output.yaml" Writable]`. Suppressed by `--no-cwd`.

**`libdir`** — a `DirCap` for the system library directory: where tinct's
standard libraries reside. Used as `[include libdir "io.llt"]`. Survives
`--no-cwd` — it is language infrastructure equivalent to builtins. Suppressed
only by `--no-libdir`. The backing source (installed files, embedded bytes, or
a `--libdir-path` override) is an implementation detail; `libdir` is the stable
interface.

**`stdin`** — a `Handle` for fd 0. Suppressed by `--no-stdin`.

All three are real values and participate fully in the cap model: `pwd` and
`libdir` can be narrowed (`[narrow libdir "net"]` is pure attenuation and
harmless), passed to functions, and used with `revocable`. They cannot be
widened — `DirCap` authority flows only downward via `narrow`.

### Capabilities Are Bound at Open Time

The capability check happens exactly once: when a resource is opened. After
that, the returned `Value::Handle` embodies the authority to use that specific
resource. Subsequent operations — `slurp`, `write`, `lines` — take a `Handle`
and do not need the original cap. This is not a security gap; it is more
precise attenuation:

- A function that receives a `DirCap` can open any file within the directory
- A function that receives a `Handle` can only read or write that one open resource
- A function that receives neither cannot access anything

This is identical to the Unix model: `open(2)` checks permissions and returns a
file descriptor; subsequent `read(fd)` and `write(fd)` calls do not re-check
because the descriptor IS the authority. Unix file descriptors are capabilities
in the Miller sense (Dennis & Van Horn 1966).

```tinct
# Cap check at open time — fs is a DirCap for /var/data
[fh: [open fs "secrets/key" Readable]]

# Handle IS the capability — fs not needed again
# fh has authority over this one file; it cannot open others
[secret: [slurp fh]]
```

The auditable access points are `include`, `open`, `connect`, and `tls` —
grep these to find every place new authority is acquired. `slurp`, `write`, and
`lines` consume an existing `Handle` and never acquire new authority.

**Handle aliasing and write ordering:** A `Handle` is backed by
`Rc<RefCell<...>>`. If the same handle is referenced from two independent lazy
bindings, both of which call `write`, the write order depends on which thunk is
forced first — exactly the kind of observable effect ordering a lazy language
normally avoids. For deterministic ordering, create an explicit data dependency
by nesting writes or using pipeline stages (see §Sequencing).

**Handle non-revocability:** Handles are non-revocable once issued. If you pass
a `Handle` to untrusted code, there is no mechanism to invalidate it before the
resource closes naturally. For cases where revocation is needed, use `revocable`
at the `DirCap` level to prevent future `open` calls; handles already in flight
are unaffected. This is acceptable for tinct's single-shot config evaluation
model — long-lived interactive programs would need Miller's caretaker pattern,
which `revocable` implements.

**Delegation and confinement:** Capabilities are transferable — a function that
receives a `DirCap` can pass it to any function it calls. This is by design;
transferability is a fundamental property of the capability model (Miller 2006,
Ch. 9). `narrow` is the delegation mechanism: to give a callee access to only a
subdirectory, narrow the cap before passing it. Confinement (preventing a cap
from being communicated to third parties) is not supported and is not needed for
configuration evaluation.

### Handle Revocation

Miller's *caretaker pattern* wraps a capability in a forwardable proxy with an
on/off switch. `revocable` implements this for `DirCap`:

```tinct
# revocable wraps a DirCap in a revocable proxy
# Returns a dict with two fields: the attenuated cap and a revoke function
[pair: [revocable fs]]

# Pass the attenuated cap to untrusted library code
[process-config pair.cap %]

# Later: revoke future opens via this cap
# (handles already issued are unaffected)
[pair.revoke null]

# Any subsequent open on pair.cap now fails with "capability revoked"
```

`pair.cap` is a `Value::RevocableDirCap` — it wraps the original `DirCap` and
an `Rc<Cell<bool>>` revoked flag. `pair.revoke` is a Rust builtin closure that
closes over the same `Rc<Cell<bool>>` and sets it to `true` when called. Every
`open` on a `RevocableDirCap` checks the flag before delegating to the inner
`DirCap`.

`revocable` does not revoke handles already opened through the cap. Its effect
is prospective — it prevents future `open` calls from succeeding. This is the
standard caretaker semantics (Miller 2006, Ch. 8).

`revocable` applies only to `DirCap`. Network connections (`connect`, `tls`)
are inherently single-use — the Handle IS the connection and closes when
dropped. Revocation at the `NetCap` level is not provided.

### Opening Resources: The Cap Primitives

```tinct
# Open a file within a DirCap
# narrow calls Dir::open(subpath) — RESOLVE_BENEATH applies to the subpath,
# so "../.." fails at narrow time, not at first open
[fh:      [open fs "config/settings.yaml" Readable]]
[out:     [open fs "output/result.yaml"    Writable]]
[log:     [open fs "logs/app.log"          Writable Appendable]]

# Narrow to a subdirectory (attenuation)
[log-cap: [narrow fs "logs/app"]]
[process-logs log-cap %]
# process-logs can open files under /var/data/logs/app — nothing else in /var/data

# Revocable cap for untrusted callee
[pair:    [revocable fs]]
[third-party-plugin pair.cap %]
[pair.revoke null]   # future opens via pair.cap fail

# TCP connection
[conn:    [connect net "db.internal" 5432]]
# TLS connections: see doc/feature/lib-tls.md
```

### `include` Is Always Cap-Qualified

`include` takes a `DirCap` as its first argument. There is no ambient path
resolution:

```tinct
[include libdir "io.llt"]      # system library
[include pwd    "config.llt"]  # project-local file
[include pkg    "auth.llt"]    # explicit user cap
```

All paths are relative to the provided cap and enforced via `RESOLVE_BENEATH` —
the same guarantee as `open`. The cap provides full disambiguation: `"io.llt"`
relative to `libdir` is a system library module; `"io.llt"` relative to `pwd`
is a project file. No path-prefix convention is needed.

`include` adds three behaviours on top of `open`: the file is parsed and
evaluated as tinct, its bindings are merged into the caller's environment, and
the result is cached by `(st_dev, st_ino)` so the same physical file is
evaluated at most once regardless of which cap or path was used to reach it.
Cycle detection uses the same key.

### Using Handles: The Three Handle Operations

Handles are consumed by three Rust builtins that take no cap argument:

```tinct
# Read all bytes to string (like Clojure's slurp)
[content: [slurp fh]]

# Write string to handle — returns the handle for chaining
[conn-written: [write conn "GET / HTTP/1.0\r\n\r\n"]]

# Lazy coinductive stream of lines — each line read on demand
[log-lines: [lines [open log-cap "access.log" Readable]]]
```

### `write` Returns the Handle: Sequencing via Data Dependency

`write` returns the handle it wrote to. This creates a data dependency chain
that enforces evaluation order in the lazy language without monadic sequencing
or pipeline stages:

```tinct
# connect → write request → read response
# Each step cannot be forced until the previous one completes —
# data dependency is the sequencing mechanism
[slurp
  [write
    [connect net "host" 80]
    "GET / HTTP/1.0\r\nHost: host\r\n\r\n"]]
```

For multiple sequential writes, nesting enforces order via the same data
dependency. Each `write` takes the handle returned by the previous one:

```tinct
# Three writes in sequence — a full HTTP/1.0 request line by line
# Innermost write happens first, outermost last
[slurp
  [write                          # 3. write final CRLF
    [write                        # 2. write Host header
      [write                      # 1. write request line
        [connect net "host" 80]
        "GET /index.html HTTP/1.0\r\n"]
      "Host: host\r\n"]
    "\r\n"]]
```

For more than two or three sequential writes, pipeline stages are cleaner —
the `%` pipeline value carries the handle between stages:

```tinct
[connect net "host" 80]
---
[write % "GET /index.html HTTP/1.0\r\n"]
---
[write % "Host: host\r\n"]
---
[write % "\r\n"]
---
[slurp %]
```

Both forms have explicit data dependencies that enforce the write order.
Independent writes to aliased handles (two bindings both holding the same
handle) do not have this guarantee — their order depends on evaluation order,
which is unspecified in a lazy letrec.

### NetCap Allowlist Specification

A `NetCap`'s allowlist is a list of entries. Each entry is one of:

| Entry form | Matches |
|-----------|---------|
| `"api.internal"` | Exact hostname (case-insensitive), any port |
| `"api.internal:5432"` | Exact hostname and port |
| `"*.internal"` | Hostname glob — prefix wildcard only |
| `"10.42.0.0/16"` | IPv4 CIDR range |
| `"fd00::/8"` | IPv6 CIDR range |

Matching at `connect`/`tls` time:

1. Check the target hostname against all hostname and glob entries (exact, pre-DNS check)
2. Resolve the hostname to one or more IP addresses
3. Check each resolved IP against all CIDR entries
4. The connection is **allowed if step 1 or step 3 produces a match**; denied otherwise

No ranges are denied by default. Developer and microservice environments
legitimately connect to RFC1918 addresses (`10.0.0.0/8`, `172.16.0.0/12`,
`192.168.0.0/16`) and link-local addresses (`169.254.0.0/16`) and blocking
these by default would break common use cases. The operator specifies exactly
what is permitted.

**DNS rebinding caveat:** Hostname-only allowlists (`"api.external.com"`) check
the hostname before DNS resolution. An attacker who controls DNS can change the
hostname's resolved IP after the hostname check passes. To prevent this, include
target CIDR ranges in the allowlist alongside the hostname — the resolved IP is
then also checked. When both hostname and CIDR are present, both must match for
a connection to proceed.

**Multiple entries at the CLI:** `--cap-net` uses the same name to accumulate
entries into one cap:

```bash
# $net allows connections to api.internal (any port) and 10.42.0.0/16 (any host)
llt eval --cap-net net=api.internal --cap-net net=10.42.0.0/16 script.llt
```

**From tinct code:**

```tinct
[net: [net-cap ["api.internal" "db.internal:5432" "10.42.0.0/16"]]]
```

> **NetCap security properties:** DNS pinning, allowlist precedence, interaction with IPv4-mapped IPv6 addresses, and behavior when DNS resolution returns multiple addresses are specified in the `net-gaps` sprint in TODO.md.

### TLS Configuration

TLS connections are designed separately. See `doc/feature/lib-tls.md` for the
full specification, including CA root selection, mutual TLS, certificate pinning, and
ALPN. The `tls` builtin and its option dict are not part of this document.

### `stdin` and `emit`

`stdin` and `emit` do not require user-provided caps. stdin and stdout are file
descriptors the process inherits from its parent; requiring a capability to
write to stdout would add verbosity with no meaningful security benefit (the
parent process already granted stdout when it spawned `llt eval`).

`stdin` is a `Value::Handle` for fd 0, injected into the root environment at
startup. It can be suppressed with `--no-stdin` for batch jobs that should never
read from stdin. `emit` writes to stdout — it is a special Rust builtin that
also sets the `emitted` flag so the CLI suppresses default JSON serialization.
Note: in embedded contexts where `llt` is used as a library or language server,
fd 1 may be an IPC socket rather than a terminal; `emit` writes to whatever
fd 1 is.

```tinct
# stdin: slurp all and parse as JSON
[parse-json [slurp stdin]]

# Or: stream stdin line by line
[filter [fn [line] [str-contains line "ERROR"]] [lines stdin]]

# emit: write to stdout
[emit [to-yaml %]]
```

### `env`: Environment Variable Access

`env name` reads one environment variable by name and returns its value as a
`String`, or `Null` if unset. Environment variables are a standard channel for
secrets in CI and container environments (`DATABASE_URL`, `AWS_SECRET_ACCESS_KEY`,
`GITHUB_TOKEN`, etc.), so `env` is gated:

- Under `--no-env`: `env` returns `Null` for all names. A sandboxed invocation
  cannot read env vars.
- Under `--allow-env NAME`: only the named variable(s) are readable; all others
  return `Null`. Multiple `--allow-env` flags accumulate.
- Default (neither flag): all env vars readable. Appropriate for trusted programs
  run by the user.

```bash
# Fully sandboxed — no env access
llt eval --no-cwd --no-env --timeout 5s script.llt

# Specific variable allowlist
llt eval --allow-env DATABASE_URL --allow-env APP_ENV script.llt
```

`Value::EnvCap` provides a language-level capability for env access, injectable
via `--cap-env NAME=VARPATTERN` alongside `--cap-fs` and `--cap-net`.

### Capability Creation: The Trust Boundary

**CLI injection (recommended for untrusted programs):**

```bash
llt eval --cap-fs pkg=/var/lib/plugins --cap-net api=schema.internal script.llt
```

Inside `script.llt`, `pkg` and `api` are available alongside the
runtime-injected `pwd`, `libdir`, and `stdin`. The program cannot open files
outside `/var/lib/plugins` (via `pkg`) or the working directory (via `pwd`),
and cannot connect to hosts other than `schema.internal`.

The `--no-*` flags suppress individual runtime-injected values by name:

```bash
--no-cwd      # pwd not injected — [include pwd ...] and [open pwd ...] fail
--no-libdir   # libdir not injected — [include libdir ...] fails
--no-stdin    # stdin not injected — [slurp stdin] fails
--no-env      # env returns Null for all names
```

A fully sandboxed invocation:

```bash
llt eval --no-cwd --no-stdin --no-env --timeout 5s --max-memory 64M script.llt
```

(`libdir` is retained even in sandboxed invocations so stdlib is accessible.
Suppress it explicitly with `--no-libdir` if needed.)

**Explicit creation (for trusted programs):**

```tinct
# dir-cap creates a DirCap for an arbitrary path (allowlist-checked)
[fs:  [dir-cap "/var/data"]]

# net-cap accepts a list of allowlist entries
[net: [net-cap ["api.internal" "10.42.0.0/16"]]]
```

### The Stdlib Layer

```tinct
# stdlib/io.llt
read-file:  [fn [cap path]    [slurp [open cap path Readable]]]
write-file: [fn [cap path s]  [write [open cap path Writable] s]]
read-lines: [fn [cap path]    [lines [open cap path Readable]]]
println:    [fn [s]           [emit [str s "\n"]]]
```

```tinct
# stdlib/net.llt
# fetch supports HTTP only; HTTPS requires the tls builtin (see doc/feature/lib-tls.md)
fetch: [fn [net-cap url]
  [parsed: [parse-url url]]
  [conn: [connect net-cap parsed.host parsed.port]]
  [req: [str
    "GET " parsed.path " HTTP/1.0\r\n"
    "Host: " parsed.host "\r\n"
    "Connection: close\r\n\r\n"]]
  [http-parse-response
    [slurp [write conn req]]]]
```

### Streaming I/O: Coinductive Line Streams

`lines handle` returns a lazy coinductive stream (a `Value::Seq`) where each
`tail` forces one `readline()` call on the underlying handle. The handle closes
via Rust's `Drop` when the last `Rc` reference to it is dropped — when the Seq
is fully consumed or goes out of scope.

This avoids lazy I/O's equational reasoning violations: each line-read is a
strict I/O operation triggered by the observable force of a `tail`, not deferred
via `unsafeInterleaveIO`. The finalization guarantee is weaker than Kiselyov
(2012)'s fold-based iteratees — `Drop` timing depends on when the `Rc`
reference count hits zero, which in a lazy language is unpredictable if the Seq
is kept alive in a long-lived binding. For tinct's single-shot config evaluation
model this is acceptable; the process exits shortly after evaluation completes,
releasing all handles.

```tinct
[include libdir "io.llt"]

[read-lines pwd "large-log.txt"]
---
[filter [fn [line] [str-contains line "ERROR"]] %]
---
[take 100 %]
# Only the first 100 matching lines are ever read from disk
```

### Formal Grounding

Why not other I/O models?

**IO monad (Haskell, Moggi 1991, Peyton Jones & Wadler 1993).** Provides total
ordering and referential transparency outside IO via monadic bind (`>>=`).
Requires type classes or higher-kinded types, do-notation, and a distinguished
`IO` type. For tinct — a configuration language without type classes or HKTs —
this is disproportionate infrastructure. The data dependency chain from `write`
returning the handle, combined with the pipeline model, covers tinct's
sequencing needs.

**Algebraic effects + handlers (Plotkin & Pretnar 2009, Leijen/Koka 2014).**
Koka's row-polymorphic effect types are closest to tinct's type system (both use
HM + row polymorphism). However, algebraic effects require knowing at each call
site which effects may occur. In call-by-need, a thunk may or may not perform
effects depending on whether it is forced — the effect type of a lazy binding is
undecidable in general (it depends on whether the binding is used). This problem
is unsolved for full call-by-need and would be disproportionate infrastructure
for a configuration language. Frank (Lindley & McBride 2017) explicitly notes
that Haskell-style laziness prevents direct-style effectful programming with
handlers.

**Lazy I/O (Haskell `hGetContents`, `unsafeInterleaveIO`).** Formally unsound.
Kiselyov showed lazy I/O breaks equational reasoning: evaluation order becomes
observable, file handles stay open for unbounded time, exceptions are
unpredictable. `lines` avoids this by making each line-read strict (triggered by
an observable `tail` force) rather than interleaved with pure computation.

**Linear types (Clean uniqueness types, Bernardy et al. 2018).** Ensure handles
are used exactly once. Composability with lazy evaluation is deeply problematic:
a linear value in a thunk might never be forced (leaked) or shared via
memoization (used more than once). The cap model achieves a weaker but sufficient
resource guarantee via `Rc` reference counting and `Drop`.

**Capability-based I/O (Miller 2006, WASI).** This is tinct's chosen model.
`Value::DirCap`, `Value::NetCap`, `Value::RevocableDirCap`, and `Value::Handle`
are all capabilities: opaque, unforgeable, passable. The key properties:

- *No ambient authority*: `open` without a `DirCap` is a type error; there is
  no `open-file path`
- *Attenuation*: `narrow` produces a strictly narrower `DirCap`; handles are
  narrower still; `revocable` adds prospective revocation
- *No confused deputy*: `RESOLVE_BENEATH` (via cap-std) is the mitigation —
  even if a function is passed a user-controlled path string alongside a
  `DirCap`, the path cannot escape the cap's root, so the confused deputy cannot
  access files outside the cap's scope
- *Auditability*: grep `include`, `open`, `connect`, `tls` to find all resource
  acquisition sites
- *Transferability*: capabilities are first-class values; a function that
  receives a `DirCap` can pass it to callees. `narrow` is the mechanism for
  limiting what is delegated.

### Strictness Annotation

I/O builtins documented with Mycroft (1981) strictness annotations in
`doc/08-evaluation.md` §Selective Materialization:

| Builtin | Args strict | Returns | Notes |
|---------|------------|---------|-------|
| `emit` | S | `Null` | Writes to stdout; sets `emitted` flag |
| `include` | S, S | `Env` | Evaluates `.llt`/`.json`/`.yaml` within DirCap; caches by `(st_dev, st_ino)` |
| `dir-cap` | S | `DirCap` | Creates directory capability; allowlist-checked |
| `net-cap` | S | `NetCap` | Creates network capability; requires `--allow-network` |
| `open` | S, S, S | `Handle` | Opens file within DirCap (RESOLVE_BENEATH) |
| `connect` | S, S, S | `Handle` | Opens TCP socket within NetCap |
| `narrow` | S, S | `DirCap` | Attenuates DirCap to subdirectory via `Dir::open(subpath)` |
| `revocable` | S | `Dict` | Wraps DirCap in revocable proxy; returns `[cap: RevocableDirCap  revoke: fn]` |
| `slurp` | S | `Str` | Reads Handle to EOF |
| `write` | S, S | `Handle` | Writes to Handle; returns same Handle |
| `lines` | S | `Seq` | Opens coinductive stream; each `tail` forces one readline |
| `env` | S | `Str\|Null` | Reads env var; returns Null under `--no-env` or if not in `--allow-env` list |
| `stdin` | — | `Handle` | Runtime-injected handle to fd 0; suppressed by `--no-stdin` |
| `pwd` | — | `DirCap` | Runtime-injected DirCap for the working directory; suppressed by `--no-cwd` |
| `libdir` | — | `DirCap` | Runtime-injected DirCap for the system library directory; suppressed by `--no-libdir` |

### Type System Integration

`Type::DirCap`, `Type::NetCap`, and `Type::Handle` are distinct types in the
type system. `open` is typed as `Fn@Handle [DirCap Str Str]`. Passing a
`Handle` where a `DirCap` is expected is a type error. Annotations use these
names directly: `cap@DirCap`, `nc@NetCap`, `fh@Handle`.

## Implementation

### New Rust Builtins (`src/builtins.rs`)

**`emit`:** Write `String` to stdout. Strict. Returns `Null`. Sets
`emitted: bool` in `EvalContext`.

**`include dir-cap path`:** Modified — now takes a `DirCap` as its first
argument. Evaluates the file at `path` within `dir-cap` using RESOLVE_BENEATH,
then merges the resulting bindings into the caller's environment. Caches by
`(st_dev, st_ino)` pair rather than canonical path string, so the same physical
file accessed via different caps gets a single cache entry. Cycle detection uses
the same key.

**`dir-cap path`:** Create `Value::DirCap` wrapping `cap_std::fs::Dir`. Strict.
Allowlist-checked against `--allow-path`; fails if the path is not allowlisted.

**`net-cap entries`:** Create `Value::NetCap` from a Seq or single String of
allowlist entries (exact hostnames, host:port, IPv4/IPv6 CIDR). Strict.
Requires `--allow-network`; fails if network access is not permitted.

**`open dir-cap path flag...`:** Open file at path relative to `DirCap`. On
Linux 5.6+: `openat2(RESOLVE_BENEATH)`. On older kernels/macOS: cap-std
userspace emulation. At least one capability flag is required: `Readable`,
`Writable`, or `Appendable` (may combine `Writable Appendable`). Optional
flags: `Binary`, `Text`, `Seekable`. Returns `Value::Handle`.

**`connect net-cap host port`:** Resolve hostname, check against NetCap
allowlist (hostname entries pre-DNS, CIDR entries post-DNS), open TCP socket.
Returns `Value::Handle`.

**`narrow dir-cap subpath`:** Calls `cap_std::fs::Dir::open(subpath)` —
RESOLVE_BENEATH applies to `subpath`, so `"../../etc"` fails at narrow time.
Returns attenuated `Value::DirCap`.

**`revocable dir-cap`:** Wraps `DirCap` in
`Value::RevocableDirCap { inner: DirCap, revoked: Rc<Cell<bool>> }`. Returns
`[cap: Value::RevocableDirCap  revoke: Value::Builtin(set-flag)]`. The revoke
builtin takes one argument (ignored; pass `null`) and sets the flag. Subsequent
`open` on the RevocableDirCap returns an error.

**`slurp handle`:** Read `Value::Handle` to EOF; return `String`. Strict.

**`write handle str`:** Write `String` to `Value::Handle`; return the same
handle. Strict in both args.

**`lines handle`:** Return a coinductive `Value::Seq` backed by the handle.
Strict in handle. Each `tail` forces one `BufRead::read_line()`. `Drop` on the
last `Rc` closes the underlying OS fd.

**`env name`:** Read environment variable. Returns `String` or `Null`. Under
`--no-env`: always `Null`. Under `--allow-env NAME`: returns value only for
allowed names; `Null` otherwise. Under neither flag: reads freely.

**`stdin`:** `Value::Handle` for fd 0, injected into the root environment at
startup. Suppressed by `--no-stdin`.

**`pwd`:** `Value::DirCap` for the working directory, injected into the root
environment at startup. Suppressed by `--no-cwd`.

**`libdir`:** `Value::DirCap` for the system library directory, injected into
the root environment at startup. Suppressed by `--no-libdir`. The backing
directory is resolved from `--libdir-path` if provided, otherwise from the
default installation path.

### New Stdlib (`stdlib/io.llt`, `stdlib/net.llt`)

`stdlib/io.llt`: `read-file`, `write-file`, `append-file`, `read-lines`,
`println`.

`stdlib/net.llt`: `fetch` (HTTP only), `http-parse-response`, `parse-url`,
`http-format-request`.

### Evaluator (`src/eval.rs`)

**`Value::DirCap`:** Wraps `cap_std::fs::Dir`. Cloning dups the underlying fd
(cheap).

**`Value::NetCap`:** Wraps `Vec<NetCapEntry>` where `NetCapEntry` is an enum
of hostname, host:port, IPv4 CIDR, IPv6 CIDR.

**`Value::RevocableDirCap`:** Wraps `Value::DirCap` + `Rc<Cell<bool>>`. `open`
checks the flag before delegating.

**`Value::Handle`:** Wraps `Rc<RefCell<Box<dyn io::Read + io::Write>>>`. Note:
`Rc` is `!Send` — handles cannot cross thread boundaries, consistent with
tinct's single-threaded evaluator. Parallelism would require migrating to
`Arc<Mutex<...>>`. `lines` Seq tails hold an `Rc` clone; `Drop` on the last
clone closes the fd.

**`EvalContext`:** Add `emitted: bool`, `env_allowlist: Option<HashSet<String>>`
(populated from `--allow-env` flags; `None` means unrestricted, `Some([])` means
none allowed). Root environment gains `pwd`, `libdir`, and `stdin` bindings
unless suppressed by their respective `--no-*` flags.

### CLI (`src/main.rs`)

**`--cap-fs NAME=PATH`:** Create `DirCap` for `PATH`, bind as `$NAME` in the
root environment. Allowlist-checked. Repeatable.

**`--cap-net NAME=ENTRY`:** Accumulate entries into `NetCap` for `$NAME`.
Repeatable; multiple uses of the same name accumulate into one cap.

**`--no-cwd`:** Suppress the `pwd` runtime injection.

**`--no-libdir`:** Suppress the `libdir` runtime injection. Programs cannot load
stdlib via `[include libdir ...]`.

**`--no-stdin`:** Suppress the `stdin` runtime injection.

**`--no-env`:** Silence `env` — returns `Null` for all environment variable
names.

**`--libdir-path PATH`:** Override the system library directory (default:
installation path). Useful for development and custom deployments.

**`--allow-env NAME`:** Add `NAME` to the env variable allowlist. Repeatable.
When any `--allow-env` flag is present, `env` returns `Null` for unlisted names.

### Sandbox (`doc/12-tooling.md`)

The cap model supplements the OS-level sandbox. `--allow-path` gates `dir-cap`.
`--allow-network` gates `net-cap`. Landlock and seccomp remain as
defense-in-depth. The `--no-cwd`, `--no-stdin`, and `--no-env` flags add
language-level restrictions on top of the OS layer, suppressing individual
runtime-injected values by name.

### Type Checker (`src/typecheck.rs`)

Cap and handle types are distinct: `Type::DirCap`, `Type::NetCap`,
`Type::Handle`. The type checker enforces capability type separation —
passing a `Handle` where a `DirCap` is expected produces a type error.

## References

- Miller, M.S. (2006). *Robust Composition: Towards a Unified Approach to Access Control and Concurrency Control*. PhD thesis, Johns Hopkins University. — Object capability model. `Value::DirCap`, `Value::NetCap`, `Value::RevocableDirCap`, and `Value::Handle` are capabilities in Miller's sense. `revocable` implements Miller's caretaker pattern (Ch. 8). Transferability and non-confinement are standard properties (Ch. 9).
- Dennis, J.B. & Van Horn, E.C. (1966). "Programming semantics for multiprogrammed computations." *Communications of the ACM*, 9(3), 143–155. — Introduces capability-based addressing. Unix file descriptors are capabilities in this sense; the `open(2)`→fd→`read(fd)` model used to justify Handle-as-capability is traceable to this paper.
- Moggi, E. (1991). "Notions of computation and monads." *Information and Computation*, 93(1), 55–92. doi:10.1016/0890-5401(91)90052-4 — Foundation for the Haskell IO monad. Considered and rejected: requires HKTs and type classes disproportionate to a configuration language.
- Peyton Jones, S.L. & Wadler, P. (1993). "Imperative functional programming." *POPL '93*, pp. 71–84. doi:10.1145/158511.158524 — IO monad in Haskell. Rejected for tinct; see §Formal Grounding.
- Peyton Jones, S.L. (2001). "Tackling the awkward squad: monadic input/output, concurrency, exceptions, and foreign-function calls in Haskell." In *Engineering Theories of Software Construction*, NATO ASI Series, IOS Press, pp. 47–96. — Definitive treatment of why lazy I/O (`hGetContents`) is unsound.
- Plotkin, G.D. & Pretnar, M. (2009). "Handlers of algebraic effects." *ESOP '09*, LNCS 5502, pp. 80–94. — Algebraic effects. Requires CBPV; the effect-typing problem for call-by-need is unsolved.
- Leijen, D. (2014). "Koka: Programming with row polymorphic effect types." *arXiv:1406.2061*. — Row-polymorphic effect types with HM inference. Unsolved for call-by-need; see §Formal Grounding.
- Lindley, S., McBride, C. & McLaughlin, C. (2017). "Do be do be do." *POPL '17*, pp. 500–514. — Frank language. Notes Haskell-style laziness prevents direct-style effectful programming with handlers.
- Kiselyov, O. (2012). "Iteratees." In *FLOPS '12*, LNCS 7294, pp. 166–181. Springer. — Motivates avoiding lazy I/O. tinct's `lines` uses per-step strict I/O (each `tail` is a strict readline) to avoid the same equational reasoning violations, though its finalization model is `Rc`/`Drop` rather than Kiselyov's fold-based guarantee.
- Bernardy, J.-P., Boespflug, M., Newton, R.R., Peyton Jones, S. & Spiwack, A. (2018). "Retrofitting linear types." *POPL '18*. — Linear types for GHC. Considered for handle resource safety; rejected. `Rc`/`Drop` is the pragmatic substitute.
- Mycroft, A. (1981). *Abstract interpretation and optimising transformations for applicative programs*. Ph.D. thesis, University of Edinburgh. — Per-argument strictness annotations. All I/O builtins are S (strict) in all arguments.
