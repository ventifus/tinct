# What If: General I/O for tinct

What would it take to add a principled general I/O model — file reads and writes, network requests, environment variables, stdin — to tinct, consistent with its lazy call-by-need semantics and the object capability model (Miller 2006)?

## Current State

tinct has exactly one I/O operation: `$include`, which reads a `.llt`, `.json`, or `.yaml` file into the evaluation environment. It is strict (the file is fully read and parsed before evaluation continues), cached (repeated includes of the same file hit the cache), and sandboxed (path allowlists, Landlock on Linux). No other I/O exists.

```tinct
# Current: all I/O goes through $include
[call $include "users.json"]
---
[call $filter [fn [u] [call $< 30 $u.age]] $$]
```

For output, tinct JSON-serializes the final pipeline value to stdout. There is no way to write a string to stdout, write a file, or make a network request from inside a tinct program.

### What's Missing

1. **`$emit`** — write a string or formatted value to stdout (planned in `doc/whatif/templating.md` Phase 1, not yet implemented)
2. **File I/O** — read and write files as strings; stream files as lazy sequences of lines
3. **Network I/O** — open TCP and TLS connections; compose HTTP requests from tinct
4. **`$stdin`** — access stdin as a readable handle (for shell pipeline integration)
5. **`$env`** — read environment variables, with appropriate sandboxing
6. **`$sql-open` / `$sql-exec`** — database connectivity (designed separately in `doc/whatif/sql-translation.md`)

## Why General I/O Matters for tinct

tinct is a configuration language that lives at system boundaries — it generates YAML for Kubernetes, reads secrets from files, calls APIs to validate schemas. Forcing all I/O to happen externally (pipe data in as JSON, read output as JSON) breaks the pipeline model for multi-step workflows:

- **Shell pipelines become awkward.** A tinct program that needs to read two config files, fetch a schema from a URL, and emit YAML requires three external shell steps to wire up.
- **`$emit` unlocks formatters.** Without `$emit`, tinct cannot produce YAML, TOML, or custom text formats — only JSON. The formatter model in `doc/whatif/templating.md` depends entirely on `$emit`.
- **`$stdin` enables Unix-style composability.** `llt eval process.llt` reading from stdin composes naturally with grep, jq, and other Unix tools.
- **TCP primitives enable `$fetch` as library code.** With `$connect` and `$tls` as Rust primitives, `$fetch` is tinct code — no dedicated Rust HTTP builtin needed.

## Design

### The Pragmatic Model: Strict I/O Builtins

tinct adopts the same model as Nix and Dhall: **I/O builtins are strict functions that execute immediately when forced, return pure values, and do not require monadic infrastructure**. This is the correct model for a lazy call-by-need configuration language. The formal justification is in §Formal Grounding below.

Each I/O builtin:
- Is **strict in its arguments** — arguments are materialized before the I/O operation executes
- **Returns a pure tinct value** — the result (string, dict, seq, or an opaque handle/cap) is a first-class value with no residual I/O structure
- **Executes at force time** — in a lazy binding, it runs when the binding is first forced; in a pipeline stage, it runs when the stage is evaluated

### Capability-Based I/O

All file and network I/O flows through **capability values** — opaque, unforgeable tinct values that represent authority over a resource. There is no ambient `$open-file path` that any code can call: opening a resource requires a capability, and capabilities must be explicitly received (passed as arguments or injected by the CLI). This is the object capability model (Miller 2006) applied to tinct's I/O layer.

Three capability types plus a revocable wrapper:

**`Value::DirCap`** — authority to open files within a directory tree. Wraps `cap_std::fs::Dir`. On Linux 5.6+, `cap_std` uses the `openat2(RESOLVE_BENEATH)` syscall, making path traversal (`../`) and symlink escapes structurally impossible at the kernel level. On older kernels and macOS, `cap_std` falls back to a userspace emulation that validates each path component individually; the security property holds in both paths.

**`Value::NetCap`** — authority to open TCP/TLS connections to a specified set of hosts and subnets. The allowlist may contain exact hostnames, hostname:port pairs, and IPv4/IPv6 CIDR ranges. See §NetCap Allowlist Specification.

**`Value::Handle`** — authority to read from and write to one specific open resource (a file or socket). Created by `$open`, `$connect`, or `$tls`; received from the runtime as `$stdin`. A `Handle` is itself a capability — more narrowly scoped than a `DirCap` (one file vs. a whole directory tree).

**`Value::RevocableDirCap`** — a `DirCap` wrapper that can be invalidated after the fact. See §Handle Revocation.

### Capabilities Are Bound at Open Time

The capability check happens exactly once: when a resource is opened. After that, the returned `Value::Handle` embodies the authority to use that specific resource. Subsequent operations — `$slurp`, `$write`, `$lines` — take a `Handle` and do not need the original cap. This is not a security gap; it is more precise attenuation:

- A function that receives a `DirCap` can open any file within the directory
- A function that receives a `Handle` can only read or write that one open resource
- A function that receives neither cannot access anything

This is identical to the Unix model: `open(2)` checks permissions and returns a file descriptor; subsequent `read(fd)` and `write(fd)` calls do not re-check because the descriptor IS the authority. Unix file descriptors are capabilities in the Miller sense (Dennis & Van Horn 1966).

```tinct
# Cap check at open time — $fs is a DirCap for /var/data
[fh: [call $open $fs "secrets/key" "r"]]

# Handle IS the capability — $fs not needed again
# $fh has authority over this one file; it cannot open others
[secret: [call $slurp $fh]]
```

The auditable access points are `$open`, `$connect`, and `$tls` — grep these to find every place new authority is acquired. `$slurp`, `$write`, and `$lines` consume an existing `Handle` and never acquire new authority.

**Handle aliasing and write ordering:** A `Handle` is backed by `Rc<RefCell<...>>`. If the same handle is referenced from two independent lazy bindings, both of which call `$write`, the write order depends on which thunk is forced first — exactly the kind of observable effect ordering a lazy language normally avoids. For deterministic ordering, create an explicit data dependency by nesting writes or using pipeline stages (see §Sequencing).

**Handle non-revocability:** Handles are non-revocable once issued. If you pass a `Handle` to untrusted code, there is no mechanism to invalidate it before the resource closes naturally. For cases where revocation is needed, use `$revocable` at the `DirCap` level to prevent future `$open` calls; handles already in flight are unaffected. This is acceptable for tinct's single-shot config evaluation model — long-lived interactive programs would need Miller's caretaker pattern, which `$revocable` implements.

**Delegation and confinement:** Capabilities are transferable — a function that receives a `DirCap` can pass it to any function it calls. This is by design; transferability is a fundamental property of the capability model (Miller 2006, Ch. 9). `$narrow` is the delegation mechanism: to give a callee access to only a subdirectory, narrow the cap before passing it. Confinement (preventing a cap from being communicated to third parties) is not supported and is not needed for configuration evaluation.

### Handle Revocation

Miller's *caretaker pattern* wraps a capability in a forwardable proxy with an on/off switch. `$revocable` implements this for `DirCap`:

```tinct
# $revocable wraps a DirCap in a revocable proxy
# Returns a dict with two fields: the attenuated cap and a revoke function
[pair: [call $revocable $fs]]

# Pass the attenuated cap to untrusted library code
[call $process-config $pair.cap $$]

# Later: revoke future opens via this cap
# (handles already issued are unaffected)
[call $pair.revoke null]

# Any subsequent $open on $pair.cap now fails with "capability revoked"
```

`$pair.cap` is a `Value::RevocableDirCap` — it wraps the original `DirCap` and an `Rc<Cell<bool>>` revoked flag. `$pair.revoke` is a Rust builtin closure that closes over the same `Rc<Cell<bool>>` and sets it to `true` when called. Every `$open` on a `RevocableDirCap` checks the flag before delegating to the inner `DirCap`.

Important: `$revocable` does not revoke handles already opened through the cap. Its effect is prospective — it prevents future `$open` calls from succeeding. This is the standard caretaker semantics (Miller 2006, Ch. 8).

`$revocable` applies only to `DirCap`. Network connections (`$connect`, `$tls`) are inherently single-use — the Handle IS the connection and closes when dropped. Revocation at the `NetCap` level is not provided.

### Opening Resources: The Cap Primitives

```tinct
# Open a file within a DirCap
# $narrow calls Dir::open(subpath) — RESOLVE_BENEATH applies to the subpath,
# so "../.." fails at narrow time, not at first $open
[fh:      [call $open $fs "config/settings.yaml" "r"]]
[out:     [call $open $fs "output/result.yaml"    "w"]]
[log:     [call $open $fs "logs/app.log"          "a"]]

# Narrow to a subdirectory (attenuation)
[log-cap: [call $narrow $fs "logs/app"]]
[call $process-logs $log-cap $$]
# $process-logs can open files under /var/data/logs/app — nothing else in /var/data

# Revocable cap for untrusted callee
[pair:    [call $revocable $fs]]
[call $third-party-plugin $pair.cap $$]
[call $pair.revoke null]   # future opens via $pair.cap fail

# TCP and TLS connections (see doc/whatif/tls.md for TLS configuration)
[conn:    [call $connect $net "db.internal" 5432]]
[secure:  [call $tls $net "api.example.com" 443]]
```

### Using Handles: The Three Handle Operations

Handles are consumed by three Rust builtins that take no cap argument:

```tinct
# Read all bytes to string (like Clojure's slurp)
[content: [call $slurp $fh]]

# Write string to handle — returns the handle for chaining
[conn-written: [call $write $conn "GET / HTTP/1.0\r\n\r\n"]]

# Lazy coinductive stream of lines — each line read on demand
[log-lines: [call $lines [call $open $log-cap "access.log" "r"]]]
```

### `$write` Returns the Handle: Sequencing via Data Dependency

`$write` returns the handle it wrote to. This creates a data dependency chain that enforces evaluation order in the lazy language without monadic sequencing or pipeline stages:

```tinct
# connect → write request → read response
# Each step cannot be forced until the previous one completes —
# data dependency is the sequencing mechanism
[call $slurp
  [call $write
    [call $connect $net "host" 80]
    "GET / HTTP/1.0\r\nHost: host\r\n\r\n"]]
```

For multiple sequential writes, nesting enforces order via the same data dependency. Each `$write` takes the handle returned by the previous one:

```tinct
# Three writes in sequence — a full HTTP/1.0 request line by line
# Innermost write happens first, outermost last
[call $slurp
  [call $write                          # 3. write final CRLF
    [call $write                        # 2. write Host header
      [call $write                      # 1. write request line
        [call $connect $net "host" 80]
        "GET /index.html HTTP/1.0\r\n"]
      "Host: host\r\n"]
    "\r\n"]]
```

For more than two or three sequential writes, pipeline stages are cleaner — the `$$` value carries the handle between stages:

```tinct
[call $connect $net "host" 80]
---
[call $write $$ "GET /index.html HTTP/1.0\r\n"]
---
[call $write $$ "Host: host\r\n"]
---
[call $write $$ "\r\n"]
---
[call $slurp $$]
```

Both forms have explicit data dependencies that enforce the write order. Independent writes to aliased handles (two bindings both holding the same handle) do not have this guarantee — their order depends on evaluation order, which is unspecified in a lazy letrec.

### NetCap Allowlist Specification

A `NetCap`'s allowlist is a list of entries. Each entry is one of:

| Entry form | Matches |
|-----------|---------|
| `"api.internal"` | Exact hostname (case-insensitive), any port |
| `"api.internal:5432"` | Exact hostname and port |
| `"*.internal"` | Hostname glob — prefix wildcard only |
| `"10.42.0.0/16"` | IPv4 CIDR range |
| `"fd00::/8"` | IPv6 CIDR range |

Matching at `$connect`/`$tls` time:
1. Check the target hostname against all hostname and glob entries (exact, pre-DNS check)
2. Resolve the hostname to one or more IP addresses
3. Check each resolved IP against all CIDR entries
4. The connection is **allowed if step 1 or step 3 produces a match**; denied otherwise

No ranges are denied by default. Developer and microservice environments legitimately connect to RFC1918 addresses (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`) and link-local addresses (`169.254.0.0/16`). Blocking these by default would break common use cases. The operator specifies exactly what is permitted.

**DNS rebinding caveat:** Hostname-only allowlists (`"api.external.com"`) check the hostname before DNS resolution. An attacker who controls DNS can change the hostname's resolved IP after the hostname check passes. To prevent this, include target CIDR ranges in the allowlist alongside the hostname — the resolved IP is then also checked. When both hostname and CIDR are present, both must match for a connection to proceed.

**Multiple entries at the CLI:** `--cap-net` uses the same name to accumulate entries into one cap:

```bash
# $net allows connections to api.internal (any port) and 10.42.0.0/16 (any host)
llt eval --cap-net net=api.internal --cap-net net=10.42.0.0/16 script.llt
```

**From tinct code:**

```tinct
[net: [call $net-cap ["api.internal" "db.internal:5432" "10.42.0.0/16"]]]
```

> **Research needed:** Full specification of NetCap security properties — DNS pinning, allowlist precedence, interaction with IPv4-mapped IPv6 addresses, and behavior when DNS resolution returns multiple addresses — is deferred. See TODO.md for the pending research item.

### TLS Configuration

`$tls net-cap host port` opens a TLS connection. Certificate validation, CA root selection, client certificates, mutual TLS, and HTTP/3 (QUIC) are a substantial design space addressed separately. See `doc/whatif/tls.md` for the full proposal.

For Phase 2 implementation: `$tls` uses `rustls` with the system CA store. Full chain validation and hostname verification are always enabled with no skip-verify option.

### Ambient Handles: `$stdin` and `$emit`

`$stdin` and `$emit` are ambient — they do not require caps. stdin and stdout are file descriptors the process inherits from its parent; requiring a capability to write to stdout would add verbosity with no meaningful security benefit (the parent process already granted stdout when it spawned `llt eval`).

`$stdin` is a `Value::Handle` for fd 0, bound at startup. `$emit` writes to stdout — it is a special Rust builtin that also sets the `emitted` flag so the CLI suppresses default JSON serialization. Note: in embedded contexts where `llt` is used as a library or language server, fd 1 may be an IPC socket rather than a terminal; `$emit` writes to whatever fd 1 is.

```tinct
# stdin: slurp all and parse as JSON
[call $parse-json [call $slurp $stdin]]

# Or: stream stdin line by line
[call $filter [fn [line] [call $str-contains $line "ERROR"]] [call $lines $stdin]]

# emit: write to stdout
[call $emit [call $to-yaml $$]]
```

### `$env`: Environment Variable Access

`$env name` reads one environment variable by name and returns its value as a `String`, or `Null` if unset. Environment variables are a standard channel for secrets in CI and container environments (`DATABASE_URL`, `AWS_SECRET_ACCESS_KEY`, `GITHUB_TOKEN`, etc.), so `$env` is gated:

- Under `--no-caps`: `$env` returns `Null` for all names. A sandboxed invocation cannot read env vars.
- Under `--allow-env NAME` (Phase 1): only the named variable(s) are readable; all others return `Null`. Multiple `--allow-env` flags accumulate.
- Default (neither flag): all env vars readable. Appropriate for trusted programs run by the user.

```bash
# Fully sandboxed — no env access
llt eval --no-fs --no-caps --timeout 5s script.llt

# Specific variable allowlist
llt eval --allow-env DATABASE_URL --allow-env APP_ENV script.llt
```

Future: `Value::EnvCap` as a language-level capability for env access, injectable via `--cap-env NAME=VARPATTERN` alongside `--cap-fs` and `--cap-net`.

### Capability Creation: The Trust Boundary

**CLI injection (recommended for untrusted programs):**

```bash
llt eval --cap-fs fs=/var/data --cap-net net=api.internal script.llt
```

Inside `script.llt`, `$fs` and `$net` are the only available capabilities. The program cannot open files outside `/var/data` or connect to hosts other than `api.internal`. `--no-caps` disables `$dir-cap` and `$net-cap` builtins; programs in that mode must receive all caps from CLI flags.

Note: `--no-caps` does not disable `$include`. For full filesystem isolation, use `--no-caps --no-fs` together. A fully sandboxed invocation:

```bash
llt eval --no-fs --no-caps --allow-env APP_ENV --timeout 5s --max-memory 64M script.llt
```

**Explicit creation (for trusted programs):**

```tinct
# $dir-cap is allowlist-checked against --allow-path (same as $include)
[fs:  [call $dir-cap "/var/data"]]

# $net-cap accepts a list of allowlist entries
[net: [call $net-cap ["api.internal" "10.42.0.0/16"]]]
```

### The Stdlib Layer

```tinct
# stdlib/io.llt
read-file:  [fn [cap path]    [call $slurp [call $open $cap $path "r"]]]
write-file: [fn [cap path s]  [call $write [call $open $cap $path "w"] $s]]
read-lines: [fn [cap path]    [call $lines [call $open $cap $path "r"]]]
println:    [fn [s]           [call $emit [call $str $s "\n"]]]
```

```tinct
# stdlib/net.llt
# Note: multi-expression fn bodies require doc/whatif/let-binding.md Phase 1
fetch: [fn [net-cap url]
  [parsed: [call $parse-url $url]]
  [conn: [call $if [call $= $parsed.scheme "https"]
    [call $tls     $net-cap $parsed.host $parsed.port]
    [call $connect $net-cap $parsed.host $parsed.port]]]
  [req: [call $str
    "GET " $parsed.path " HTTP/1.0\r\n"
    "Host: " $parsed.host "\r\n"
    "Connection: close\r\n\r\n"]]
  [call $http-parse-response
    [call $slurp [call $write $conn $req]]]]

fetch-opts: [fn [net-cap url opts] ...]
```

### Streaming I/O: Coinductive Line Streams

`$lines handle` returns a lazy coinductive stream (a `Value::Seq`) where each `$tail` forces one `readline()` call on the underlying handle. The handle closes via Rust's `Drop` when the last `Rc` reference to it is dropped — when the Seq is fully consumed or goes out of scope.

This avoids lazy I/O's equational reasoning violations: each line-read is a strict I/O operation triggered by the observable force of a `$tail`, not deferred via `unsafeInterleaveIO`. The finalization guarantee is weaker than Kiselyov (2012)'s fold-based iteratees — `Drop` timing depends on when the `Rc` reference count hits zero, which in a lazy language is unpredictable if the Seq is kept alive in a long-lived binding. For tinct's single-shot config evaluation model this is acceptable; the process exits shortly after evaluation completes, releasing all handles.

```tinct
[call $include "stdlib/io.llt"]

[call $read-lines $fs "large-log.txt"]
---
[call $filter [fn [line] [call $str-contains $line "ERROR"]] $$]
---
[call $take 100 $$]
# Only the first 100 matching lines are ever read from disk
```

### Formal Grounding

Why not other I/O models?

**IO monad (Haskell, Moggi 1991, Peyton Jones & Wadler 1993).** Provides total ordering and referential transparency outside IO via monadic bind (`>>=`). Requires type classes or higher-kinded types, do-notation, and a distinguished `IO` type. For tinct — a configuration language without type classes or HKTs — this is disproportionate infrastructure. The data dependency chain from `$write` returning the handle, combined with the pipeline model, covers tinct's sequencing needs.

**Algebraic effects + handlers (Plotkin & Pretnar 2009, Leijen/Koka 2014).** Koka's row-polymorphic effect types are closest to tinct's type system (both use HM + row polymorphism). However, algebraic effects require knowing at each call site which effects may occur. In call-by-need, a thunk may or may not perform effects depending on whether it is forced — the effect type of a lazy binding is undecidable in general (it depends on whether the binding is used). This problem is unsolved for full call-by-need and would be disproportionate infrastructure for a configuration language. Frank (Lindley & McBride 2017) explicitly notes that Haskell-style laziness prevents direct-style effectful programming with handlers.

**Lazy I/O (Haskell `hGetContents`, `unsafeInterleaveIO`).** Formally unsound. Kiselyov showed lazy I/O breaks equational reasoning: evaluation order becomes observable, file handles stay open for unbounded time, exceptions are unpredictable. `$lines` avoids this by making each line-read strict (triggered by an observable `$tail` force) rather than interleaved with pure computation.

**Linear types (Clean uniqueness types, Bernardy et al. 2018).** Ensure handles are used exactly once. Composability with lazy evaluation is deeply problematic: a linear value in a thunk might never be forced (leaked) or shared via memoization (used more than once). The cap model achieves a weaker but sufficient resource guarantee via `Rc` reference counting and `Drop`.

**Capability-based I/O (Miller 2006, WASI).** This is tinct's chosen model. `Value::DirCap`, `Value::NetCap`, `Value::RevocableDirCap`, and `Value::Handle` are all capabilities: opaque, unforgeable, passable. The key properties:

- *No ambient authority*: `$open` without a `DirCap` is a type error; there is no `$open-file path`
- *Attenuation*: `$narrow` produces a strictly narrower `DirCap`; handles are narrower still; `$revocable` adds prospective revocation
- *No confused deputy*: `RESOLVE_BENEATH` (via cap-std) is the mitigation — even if a function is passed a user-controlled path string alongside a `DirCap`, the path cannot escape the cap's root, so the confused deputy cannot access files outside the cap's scope
- *Auditability*: grep `$open`, `$connect`, `$tls` to find all resource acquisition sites
- *Transferability*: capabilities are first-class values; a function that receives a `DirCap` can pass it to callees. `$narrow` is the mechanism for limiting what is delegated.

### Strictness Annotation

I/O builtins documented with Mycroft (1981) strictness annotations in `doc/08-evaluation.md` §Selective Materialization:

| Builtin | Args strict | Returns | Notes |
|---------|------------|---------|-------|
| `$emit` | S | `Null` | Writes to stdout; sets `emitted` flag |
| `$dir-cap` | S | `DirCap` | Creates directory capability; allowlist-checked |
| `$net-cap` | S | `NetCap` | Creates network capability; requires `--allow-network` |
| `$open` | S, S, S | `Handle` | Opens file within DirCap (RESOLVE_BENEATH) |
| `$connect` | S, S, S | `Handle` | Opens TCP socket within NetCap |
| `$tls` | S, S, S | `Handle` | Opens TLS socket within NetCap |
| `$narrow` | S, S | `DirCap` | Attenuates DirCap to subdirectory via `Dir::open(subpath)` |
| `$revocable` | S | `Dict` | Wraps DirCap in revocable proxy; returns `[cap: RevocableDirCap  revoke: fn]` |
| `$slurp` | S | `Str` | Reads Handle to EOF |
| `$write` | S, S | `Handle` | Writes to Handle; returns same Handle |
| `$lines` | S | `Seq` | Opens coinductive stream; each `$tail` forces one readline |
| `$env` | S | `Str\|Null` | Reads env var; returns Null under `--no-caps` or if not in `--allow-env` list |
| `$stdin` | — | `Handle` | Pre-opened handle to fd 0 |

### Type System Integration

Phase 1: all I/O builtins infer as returning `Any`. The type checker does not distinguish cap types from other values.

Phase 2 (future): `Type::DirCap`, `Type::NetCap`, `Type::Handle` as distinct types. `$open` infers as `Fn@Handle [DirCap Str Str]`. Passing a `Handle` where a `DirCap` is expected becomes a type error.

Phase 3 (future, no commitment): if type classes arrive, `IO` becomes an enforced effect type.

## What Would Change

### New Rust Builtins (`src/builtins.rs`)

**`$emit`:** Write `String` to stdout. Strict. Returns `Null`. Sets `emitted: bool` in `EvalContext`.

**`$dir-cap path`:** Create `Value::DirCap` wrapping `cap_std::fs::Dir`. Strict. Allowlist-checked against `--allow-path`. Fails under `--no-caps`.

**`$net-cap entries`:** Create `Value::NetCap` from a Seq or single String of allowlist entries (exact hostnames, host:port, IPv4/IPv6 CIDR). Strict. Requires `--allow-network`. Fails under `--no-caps`.

**`$open dir-cap path mode`:** Open file at path relative to `DirCap`. On Linux 5.6+: `openat2(RESOLVE_BENEATH)`. On older kernels/macOS: cap-std userspace emulation. Mode: `"r"`, `"w"`, `"a"`. Returns `Value::Handle`.

**`$connect net-cap host port`:** Resolve hostname, check against NetCap allowlist (hostname entries pre-DNS, CIDR entries post-DNS), open TCP socket. Returns `Value::Handle`.

**`$tls net-cap host port`:** Same allowlist check as `$connect`. Opens TLS socket using `rustls`. Full chain and hostname verification always enabled. Returns `Value::Handle`. See `doc/whatif/tls.md` for CA root configuration and client certificates.

**`$narrow dir-cap subpath`:** Calls `cap_std::fs::Dir::open(subpath)` — RESOLVE_BENEATH applies to `subpath`, so `"../../etc"` fails at narrow time. Returns attenuated `Value::DirCap`.

**`$revocable dir-cap`:** Wraps `DirCap` in `Value::RevocableDirCap { inner: DirCap, revoked: Rc<Cell<bool>> }`. Returns `[cap: Value::RevocableDirCap  revoke: Value::Builtin(set-flag)]`. The revoke builtin takes one argument (ignored; pass `null`) and sets the flag. Subsequent `$open` on the RevocableDirCap returns an error.

**`$slurp handle`:** Read `Value::Handle` to EOF; return `String`. Strict.

**`$write handle str`:** Write `String` to `Value::Handle`; return the same handle. Strict in both args.

**`$lines handle`:** Return a coinductive `Value::Seq` backed by the handle. Strict in handle. Each `$tail` forces one `BufRead::read_line()`. `Drop` on the last `Rc` closes the underlying OS fd.

**`$env name`:** Read environment variable. Returns `String` or `Null`. Under `--no-caps`: always `Null`. Under `--allow-env NAME`: returns value only for allowed names; `Null` otherwise. Under neither flag: reads freely.

**`$stdin`:** `Value::Handle` for fd 0, bound at startup.

**Impact:** Significant — thirteen new builtins, four new value variants (`Value::DirCap`, `Value::NetCap`, `Value::RevocableDirCap`, `Value::Handle`), `EvalContext` gains `emitted: bool`.

### New Stdlib (`stdlib/io.llt`, `stdlib/net.llt`)

`stdlib/io.llt`: `read-file`, `write-file`, `append-file`, `read-lines`, `println`.
`stdlib/net.llt`: `fetch`, `fetch-opts`, `$http-parse-response`, `$parse-url`, `$http-format-request`.

**Impact:** Moderate — two new stdlib files; no changes to `stdlib/prelude.llt`.

### Evaluator (`src/eval.rs`)

**`Value::DirCap`:** Wraps `cap_std::fs::Dir`. Cloning dups the underlying fd (cheap).

**`Value::NetCap`:** Wraps `Vec<NetCapEntry>` where `NetCapEntry` is an enum of hostname, host:port, IPv4 CIDR, IPv6 CIDR.

**`Value::RevocableDirCap`:** Wraps `Value::DirCap` + `Rc<Cell<bool>>`. `$open` checks the flag before delegating.

**`Value::Handle`:** Wraps `Rc<RefCell<Box<dyn io::Read + io::Write>>>`. Note: `Rc` is `!Send` — handles cannot cross thread boundaries, consistent with tinct's single-threaded evaluator. Parallelism would require migrating to `Arc<Mutex<...>>`. `$lines` Seq tails hold an `Rc` clone; `Drop` on the last clone closes the fd.

**`EvalContext`:** Add `emitted: bool`, `env_allowlist: Option<HashSet<String>>` (populated from `--allow-env` flags; `None` means unrestricted, `Some([])` means none allowed).

**Impact:** Moderate — four new value variants, two new context fields, one new Seq tail type.

### CLI (`src/main.rs`)

**`--cap-fs NAME=PATH`:** Create `DirCap`, bind as `$NAME`. Allowlist-checked. Repeatable.

**`--cap-net NAME=ENTRY`:** Accumulate entries into `NetCap` for `$NAME`. Repeatable; multiple uses of same name accumulate into one cap.

**`--no-caps`:** Disable `$dir-cap` and `$net-cap` builtins. Also silences `$env` (returns `Null` for all names). Does **not** disable `$include` — use `--no-fs` for that.

**`--allow-env NAME`:** Add `NAME` to the env variable allowlist. Repeatable. When any `--allow-env` flag is present, `$env` returns `Null` for unlisted names.

**Impact:** Moderate — four new flags, cap injection into root environment, env allowlist in `EvalContext`.

### Sandbox (`doc/12-tooling.md`)

The cap model supplements the OS-level sandbox. `--allow-path` gates `$dir-cap` and `$include`. `--allow-network` gates `$net-cap`. Landlock and seccomp remain as defense-in-depth. The `--no-caps` flag adds a language-level restriction on top of the OS layer.

**Impact:** Minor — new flags documented; existing sandbox layers unchanged.

### Type Checker (`src/typecheck.rs`)

Phase 1: all cap/handle types infer as `Any`. No changes.
Phase 2 (future): distinct `Type::DirCap`, `Type::NetCap`, `Type::Handle`.

**Impact:** None in Phase 1.

## Phased Adoption

### Phase 1: File Caps, `$emit`, `$stdin`, `$env`

`$dir-cap`, `$open`, `$narrow`, `$revocable`, `$slurp`, `$write`, `$lines`, `$emit`, `$stdin`, `$env` (with `--no-caps`/`--allow-env` gating), and `stdlib/io.llt`. CLI `--cap-fs` injection.

```tinct
[call $include "stdlib/io.llt"]

# llt eval --cap-fs fs=/var/data --allow-env DATABASE_URL script.llt

[db-url:  [call $env "DATABASE_URL"]]
[db-pass: [call $read-file $fs "secrets/db-password"]]
[config:  [db: [url: $db-url  password: $db-pass]]]
---
[call $emit [call $to-yaml $$]]
```

**Prerequisites:** `eval-sandbox-flags` sprint, `include-fd-hardening` sprint (cap-std already pulled in; `$open` reuses the same `Dir`).

### Phase 2: Network Caps, `stdlib/net.llt`

`$net-cap`, `$connect`, `$tls` (basic), CLI `--cap-net` injection, `stdlib/net.llt`. Enables HTTP and arbitrary TCP protocol implementations in tinct.

```tinct
[call $include "stdlib/io.llt"]
[call $include "stdlib/net.llt"]

# llt eval --cap-fs fs=/var/data --cap-net net=schema.internal script.llt

[schema: [call $fetch $net "https://schema.internal/v2/deployment"]]
---
[call $validate [call $parse-json $schema] $$]
---
[call $emit [call $to-yaml $$]]
```

**Prerequisites:** Phase 1 complete; `rustls = "0.23"` in `Cargo.toml`; see `doc/whatif/tls.md` for full TLS configuration design.

### Phase 3: Atomic Writes, Streaming Fetch, `--no-caps`

Atomic file writes (write-to-temp + rename). Streaming fetch response body via `$lines` over the socket handle. `--no-caps` enforcement hardened. `$stdin` streaming for large inputs.

**Prerequisites:** Phase 2 complete.

### Phase 4: Cap Types in the Type Checker (Optional)

`Type::DirCap`, `Type::NetCap`, `Type::Handle` as distinct inferred types. Wrong-cap-type errors become static.

**Prerequisites:** Phase 3 complete; type system stability.

### Prerequisites

- Phase 1: `eval-sandbox-flags` sprint, `include-fd-hardening` sprint
- Phase 2: Phase 1 complete; `rustls = "0.23"` added; `doc/whatif/tls.md` design accepted
- Phase 3: Phase 2 complete
- Phase 4: Phase 3 complete

### Trigger

- When `doc/whatif/templating.md` is promoted to a sprint — Phase 1 (`$emit`) is a hard dependency
- When a tinct pipeline needs to read secrets from files with fine-grained authority
- When a tinct pipeline needs to call an HTTP API inside a pipeline stage
- When tinct programs need to compose with stdin/stdout as Unix filters

## References

- Miller, M.S. (2006). *Robust Composition: Towards a Unified Approach to Access Control and Concurrency Control*. PhD thesis, Johns Hopkins University. — Object capability model. `Value::DirCap`, `Value::NetCap`, `Value::RevocableDirCap`, and `Value::Handle` are capabilities in Miller's sense. `$revocable` implements Miller's caretaker pattern (Ch. 8). Transferability and non-confinement are standard properties (Ch. 9).
- Dennis, J.B. & Van Horn, E.C. (1966). "Programming semantics for multiprogrammed computations." *Communications of the ACM*, 9(3), 143–155. — Introduces capability-based addressing. Unix file descriptors are capabilities in this sense; the `open(2)`→fd→`read(fd)` model used to justify Handle-as-capability is traceable to this paper.
- Moggi, E. (1991). "Notions of computation and monads." *Information and Computation*, 93(1), 55–92. doi:10.1016/0890-5401(91)90052-4 — Foundation for the Haskell IO monad. Considered and rejected: requires HKTs and type classes disproportionate to a configuration language.
- Peyton Jones, S.L. & Wadler, P. (1993). "Imperative functional programming." *POPL '93*, pp. 71–84. doi:10.1145/158511.158524 — IO monad in Haskell. Rejected for tinct; see §Formal Grounding.
- Peyton Jones, S.L. (2001). "Tackling the awkward squad: monadic input/output, concurrency, exceptions, and foreign-function calls in Haskell." In *Engineering Theories of Software Construction*, NATO ASI Series, IOS Press, pp. 47–96. — Definitive treatment of why lazy I/O (`hGetContents`) is unsound.
- Plotkin, G.D. & Pretnar, M. (2009). "Handlers of algebraic effects." *ESOP '09*, LNCS 5502, pp. 80–94. — Algebraic effects. Requires CBPV; the effect-typing problem for call-by-need is unsolved.
- Leijen, D. (2014). "Koka: Programming with row polymorphic effect types." *arXiv:1406.2061*. — Row-polymorphic effect types with HM inference. Unsolved for call-by-need; see §Formal Grounding.
- Lindley, S., McBride, C. & McLaughlin, C. (2017). "Do be do be do." *POPL '17*, pp. 500–514. — Frank language. Notes Haskell-style laziness prevents direct-style effectful programming with handlers.
- Kiselyov, O. (2012). "Iteratees." In *FLOPS '12*, LNCS 7294, pp. 166–181. Springer. — Motivates avoiding lazy I/O. tinct's `$lines` uses per-step strict I/O (each `$tail` is a strict readline) to avoid the same equational reasoning violations, though its finalization model is `Rc`/`Drop` rather than Kiselyov's fold-based guarantee.
- Bernardy, J.-P., Boespflug, M., Newton, R.R., Peyton Jones, S. & Spiwack, A. (2018). "Retrofitting linear types." *POPL '18*. — Linear types for GHC. Considered for handle resource safety; rejected. `Rc`/`Drop` is the pragmatic substitute.
- Mycroft, A. (1981). *Abstract interpretation and optimising transformations for applicative programs*. Ph.D. thesis, University of Edinburgh. — Per-argument strictness annotations. All I/O builtins are S (strict) in all arguments.
