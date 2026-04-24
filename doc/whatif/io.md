# What If: General I/O for tinct

What would it take to add a principled general I/O model — file reads and writes, network requests, environment variables, stdin — to tinct, consistent with its lazy call-by-need semantics?

## Current State

tinct has exactly one I/O operation: `$include`, which reads a `.llt`, `.json`, or `.yaml` file into the evaluation environment. It is strict (the file is fully read and parsed before evaluation continues), cached (repeated includes of the same file hit the cache), and sandboxed (path allowlists, Landlock on Linux). No other I/O exists.

```tinct
# Current: all I/O goes through $include
$include "users.json"
---
[call $filter [fn [u] [call $< 30 $u.age]] $$]
```

For output, tinct JSON-serializes the final pipeline value to stdout. There is no way to write a string to stdout, write a file, or make a network request from inside a tinct program.

### What's Missing

1. **`$emit`** — write a string or formatted value to stdout (planned in `doc/whatif/templating.md` Phase 1, not yet implemented)
2. **File slurping** — read a file's text as a string, distinct from `$include` (which evaluates)
3. **File writing** — write a string to a file path
4. **Network I/O** — make TCP connections and HTTP requests
5. **`$stdin`** — access stdin as a readable handle (for shell pipeline integration)
6. **`$env`** — read environment variables
7. **`$sql-open` / `$sql-exec`** — database connectivity (designed separately in `doc/whatif/sql-translation.md`)

## Why General I/O Matters for tinct

tinct is a configuration language that lives at system boundaries — it generates YAML for Kubernetes, reads secrets from files, calls APIs to validate schemas. Forcing all I/O to happen externally (pipe data in as JSON, read output as JSON) breaks the pipeline model for multi-step workflows:

- **Shell pipelines become awkward.** A tinct program that needs to read two config files, fetch a schema from a URL, and emit YAML requires three external shell steps to wire up.
- **`$emit` unlocks formatters.** Without `$emit`, tinct cannot produce YAML, TOML, or custom text formats — only JSON. The formatter model in `doc/whatif/templating.md` depends entirely on `$emit`.
- **`$stdin` enables Unix-style composability.** `llt eval process.llt` reading from stdin composes naturally with grep, jq, and other Unix tools.
- **File slurping enables config overlay patterns.** Reading a file as text (not evaluating it) is common for base64-encoding secrets, embedding certs, or passing raw content downstream.
- **TCP primitives enable `$fetch` as library code.** With `$tcp-connect` and `$tls-connect` as Rust primitives, `$fetch` is just tinct code that speaks HTTP — no dedicated Rust HTTP builtin needed.

## Design

### The Pragmatic Model: Strict I/O Builtins

tinct adopts the same model as Nix and Dhall: **I/O builtins are strict functions that execute immediately when forced, return pure values, and do not require monadic infrastructure**. This is the correct model for a lazy call-by-need configuration language. The formal justification is in §Formal Grounding below.

Each I/O builtin:
- Is **strict in its arguments** — arguments are materialized before the I/O operation executes
- **Returns a pure tinct value** — the result (string, dict, seq, or an opaque handle) is a first-class value with no residual I/O structure
- **Executes at force time** — in a lazy binding, it runs when the binding is first forced; in a pipeline stage, it runs when the stage is evaluated
- **Is sandboxed** — subject to the same allowlist and seccomp restrictions as `$include`

### Uniform Handle Abstraction

All byte-stream I/O — files, TCP sockets, stdin — shares one opaque type: `Value::Handle`. A handle wraps a `Box<dyn io::Read + io::Write>` in Rust. At the tinct level it is an unforgeable token: it can be passed to `$slurp`, `$write`, and `$lines`, but cannot be inspected or constructed from arbitrary data.

This is the Unix "everything is a file descriptor" model applied to tinct. From user code, there is no difference between a file handle and a socket handle — both respond to the same three operations.

**Opening handles (Rust builtins):**

```tinct
# File handle (modes: "r", "w", "a")
[fh: [call $open-file "config/secrets.txt" "r"]]

# Plain TCP socket
[conn: [call $tcp-connect "db.internal" 5432]]

# TLS socket (for HTTPS and TLS-wrapped protocols)
[tls: [call $tls-connect "api.example.com" 443]]

# stdin — a pre-opened handle to fd 0
$stdin
```

`$stdin` is a handle value bound at startup, not a pre-slurped string. Users `$slurp` it to get a string, or `$lines` it to stream lines. This is distinct from `$$` pipeline input, which carries piped JSON.

**Handle operations (Rust builtins):**

```tinct
# Read all bytes to string (like Clojure's slurp)
[content: [call $slurp $fh]]

# Write string to handle — returns the handle for chaining
[_ : [call $write $conn "PING\r\n"]]

# Lazy Seq of lines (iteratee pattern — lines read on demand)
[log-lines: [call $lines [call $open-file "app.log" "r"]]]
```

### `$write` Returns the Handle: Sequencing via Data Dependency

`$write` returns the handle it wrote to (not `Null`). This is the key design decision for sequencing in a lazy language. Because `$write` returns the handle and subsequent operations take it as an argument, a data dependency chain is created that enforces evaluation order without monadic sequencing:

```tinct
# connect → write request → read response
# Each step cannot be forced until the previous one returns
[call $slurp
  [call $write
    [call $tcp-connect "host" 80]
    "GET / HTTP/1.0\r\nHost: host\r\n\r\n"]]
```

`$slurp` cannot be forced until `$write` returns the handle. `$write` cannot be forced until `$tcp-connect` returns the handle. The lazy evaluator gets correct sequencing for free from the dependency graph — no monad, no pipeline stage, no sequential construct needed.

For multiple sequential writes, nest or use pipeline stages:

```tinct
# Nested writes (sequence enforced by data dependency)
[call $write
  [call $write $conn "line1\n"]
  "line2\n"]

# Pipeline stages (cleaner for many writes)
[call $write $conn "line1\n"]
---
[call $write $$ "line2\n"]
```

### The Stdlib Layer

High-level convenience functions are tinct code in `stdlib/io.llt`, built from the primitives above. This keeps the Rust surface minimal:

```tinct
# stdlib/io.llt

read-file:  [fn [path]    [call $slurp [call $open-file $path "r"]]]
write-file: [fn [path s]  [call $write [call $open-file $path "w"] $s]]
read-lines: [fn [path]    [call $lines [call $open-file $path "r"]]]
```

`$fetch` is similarly stdlib — it constructs an HTTP/1.0 request, sends it over a TCP or TLS connection, and parses the response. No dedicated HTTP Rust builtin is needed:

```tinct
# stdlib/net.llt (simplified)

fetch: [fn [url]
  [parsed: [call $parse-url $url]]
  [conn: [call $if [call $= $parsed.scheme "https"]
    [call $tls-connect $parsed.host $parsed.port]
    [call $tcp-connect $parsed.host $parsed.port]]]
  [req: [call $str
    "GET " $parsed.path " HTTP/1.0\r\n"
    "Host: " $parsed.host "\r\n"
    "Connection: close\r\n\r\n"]]
  [call $http-parse-response
    [call $slurp [call $write $conn $req]]]]
```

`$http-parse-response` (split headers/body, decode status code) is also stdlib tinct. The only Rust required for `$fetch` is `$tcp-connect` and `$tls-connect` — TLS cannot be implemented in tinct.

### Effect Ordering via the Pipeline Model

The `---` separator is tinct's explicit sequencing primitive for cases where data dependency alone is insufficient:

```tinct
[call $slurp $stdin]
---
[call $parse-json $$]
---
[call $filter [fn [item] [call $> $item.score 0.5]] $$]
---
[call $emit [call $to-yaml $$]]
```

Each stage completes before the next begins. For multiple independent writes, pipeline stages guarantee order:

```tinct
[call $write-file "out/a.txt" $content-a]
---
[call $write-file "out/b.txt" $content-b]
```

### `$emit`: Special-Cased Stdout

`$emit` is a Rust builtin rather than a stdlib wrapper over `$write` because stdout has special semantics in tinct's CLI: when `$emit` is called, the CLI suppresses the default JSON serialization of the final pipeline value. A stdlib `write-file` wrapper cannot set this flag. `$emit` also always refers to stdout — unlike `$write`, it does not take a handle argument.

### Streaming I/O: The Iteratee Pattern

`$lines` returns a lazy `Seq` backed by the handle. Each `$tail` forces one `readline()` call. The handle closes via Rust's `Drop` when the Seq is fully consumed or garbage collected — the same pattern as SQL cursor Seqs in `doc/whatif/sql-translation.md`.

```tinct
$include "stdlib/io.llt"

[call $read-lines "large-log.txt"]
---
[call $filter [fn [line] [call $str-contains $line "ERROR"]] $$]
---
[call $take 100 $$]
# Only the first 100 matching lines are ever read from disk
```

This is the correct model for streaming I/O in a lazy language: per-step strict I/O embedded in the Seq tail thunk, guaranteed finalization via `Drop`, without lazy-IO's equational-reasoning violations.

### Formal Grounding

Why not other I/O models?

**IO monad (Haskell, Moggi 1991, Peyton Jones & Wadler 1993).** Provides total ordering and referential transparency outside IO via monadic bind (`>>=`). Requires type classes or higher-kinded types, do-notation, and a distinguished `IO` type. For tinct — a configuration language without type classes or HKTs — this is disproportionate infrastructure. The data dependency chain provided by `$write` returning the handle covers tinct's sequencing needs without monadic infrastructure.

**Algebraic effects + handlers (Plotkin & Pretnar 2009, Leijen/Koka 2014).** Koka's row-polymorphic effect types are closest to tinct's type system (both use HM + row polymorphism). However, algebraic effects fundamentally assume effects occur at well-defined call sites. In call-by-need, a thunk may or may not perform effects depending on whether it is forced — the effect type of a binding depends on whether it is used, which is undecidable in general. Frank (Lindley & McBride 2017) explicitly notes that Haskell's laziness prevents direct-style effectful programming with handlers. Without a value/computation type distinction (CBPV, Levy 2003), effect typing for lazy languages is unsound.

**Lazy I/O (Haskell `hGetContents`, `unsafeInterleaveIO`).** Formally unsound. Kiselyov showed lazy I/O breaks equational reasoning: evaluation order becomes observable, file handles stay open for unbounded time, exceptions are unpredictable. The iteratee pattern (Kiselyov 2012) is the sound replacement — per-step strict I/O embedded in `$lines`' Seq tail thunks, guaranteed finalization via `Drop`.

**Linear types (Clean uniqueness types, Bernardy et al. 2018).** Ensure handles are used exactly once. Composability with lazy evaluation is deeply problematic: a linear value in a thunk might never be forced (leaked) or shared via memoization (used more than once). Adding linearity to tinct's HM type system requires multiplicity annotations on every function arrow — disproportionate for a configuration language. `Value::Handle`'s `Rc` reference counting is the pragmatic substitute: the handle closes when all references drop, not when it is "linearly consumed."

**Capability-based I/O (Miller 2006, WASI).** Authority as explicit unforgeable references. tinct's `Value::Handle` is already capability-like: it is an opaque token that can be passed and used but cannot be forged from a path string without going through an allowlist-checked open. OS-level ambient authority restriction (Landlock/seccomp + allowlists) provides the enforcement layer; `Value::Handle` provides the language-level token model. Full ocap (every operation takes an explicit capability argument) is not needed for tinct's threat model.

**The pragmatic model (Nix, Dhall).** Restricted strict I/O primitives in an otherwise pure lazy language. This is tinct's current model (`$include`) and the natural extension. Nix is lazy; `builtins.readFile` is eager. The key insight: I/O builtins are always strict (materializing), so lazy evaluation does not affect their ordering — they execute when forced, and they are always forced because their results are needed. No type system extensions required.

### Strictness Annotation

I/O builtins are documented with Mycroft (1981) strictness annotations in `doc/08-evaluation.md` §Selective Materialization:

| Builtin | Strictness | Returns | Notes |
|---------|-----------|---------|-------|
| `$emit` | S | `Null` | Writes to stdout; sets `emitted` flag |
| `$open-file` | S, S | `Handle` | Opens file; strict in path and mode |
| `$tcp-connect` | S, S | `Handle` | Opens TCP socket |
| `$tls-connect` | S, S | `Handle` | Opens TLS socket |
| `$slurp` | S | `Str` | Reads handle to EOF |
| `$write` | S, S | `Handle` | Writes to handle; returns handle |
| `$lines` | S | `Seq` | Opens iteratee; lazy after first force |
| `$env` | S | `Str\|Null` | Reads environment variable |
| `$stdin` | — | `Handle` | Pre-opened handle to fd 0 |

`$lines` is strict in its argument (the handle is opened immediately) but returns a lazy `Seq` — lines are read incrementally as the Seq is forced.

### Type System Integration

Phase 1: all I/O builtins infer as returning `Any`. The type checker does not distinguish effectful from pure functions.

Phase 2 (future, no commitment): add an `IO` type annotation for documentation purposes. `$emit` infers as `Fn@Null [Str]`. No runtime enforcement — the annotation is informational. This enables future lint rules ("don't put `$emit` inside a lazy binding") without requiring type class machinery.

Phase 3 (future, no commitment): if type classes arrive, `IO` becomes an enforced effect type. Speculative — tinct may never need this.

## What Would Change

### New Rust Builtins (`src/builtins.rs`)

**`$emit`:** Write `String` to stdout. Strict. Returns `Null`. Sets `emitted: bool` in `EvalContext` so the CLI suppresses default JSON output.

**`$open-file path mode`:** Open file (resolved via cap-std `Dir`) as `Value::Handle`. Strict in both args. Subject to `--allow-path` allowlist. Mode `"r"` opens for reading, `"w"` creates/truncates for writing, `"a"` opens for appending.

**`$tcp-connect host port`:** Open a TCP socket, return `Value::Handle`. Strict. Requires `--allow-network`; blocked by default via seccomp.

**`$tls-connect host port`:** Open a TLS-wrapped socket, return `Value::Handle`. Strict. Requires `--allow-network`. Uses `rustls` internally — TLS is not expressible in tinct.

**`$slurp handle`:** Read `Value::Handle` to EOF; return `String`. Strict. Equivalent to Clojure's `slurp`.

**`$write handle str`:** Write `String` to `Value::Handle`; return the same `Value::Handle`. Strict in both args. Returns handle to enable data-dependency sequencing.

**`$lines handle`:** Open an iteratee over `Value::Handle`; return `Value::Seq`. Strict in handle (opened immediately). Each `$tail` forces one `readline()`. Rust `Drop` closes the handle when the Seq is exhausted or dropped.

**`$env name`:** Read environment variable by name. Returns `String` or `Null`. Strict.

**`$stdin`:** A `Value::Handle` to fd 0, bound at startup and available as a named value (not a function call). Separate from `$$` pipeline input, which carries piped JSON.

**Impact:** Moderate — nine new builtins, one new value variant (`Value::Handle` wrapping `Box<dyn Read + Write>`), `EvalContext` gains `emitted: bool`.

### New Stdlib (`stdlib/io.llt`, `stdlib/net.llt`)

**`stdlib/io.llt`:** Convenience wrappers over the handle primitives:

```tinct
read-file:  [fn [path]    [call $slurp [call $open-file $path "r"]]]
write-file: [fn [path s]  [call $write [call $open-file $path "w"] $s]]
append-file:[fn [path s]  [call $write [call $open-file $path "a"] $s]]
read-lines: [fn [path]    [call $lines [call $open-file $path "r"]]]
println:    [fn [s]       [call $emit [call $str $s "\n"]]]
```

**`stdlib/net.llt`:** HTTP over TCP/TLS:

```tinct
fetch:      [fn [url]      ...]   # HTTP GET — returns response body string
fetch-opts: [fn [url opts] ...]   # POST/PUT/PATCH with method/body/headers
```

`$http-parse-response`, `$parse-url`, and `$http-format-request` are helper functions defined entirely in `stdlib/net.llt`.

**Impact:** Moderate — two new stdlib files; no changes to `stdlib/prelude.llt`.

### Evaluator (`src/eval.rs`)

**`Value::Handle`:** New variant wrapping `Rc<RefCell<Box<dyn io::Read + io::Write>>>`. Field access is an error. `Value::Seq` tails backed by a `Handle` read one line per force via `BufRead::read_line`. `Drop` on the inner `Rc` closes the underlying OS resource when the reference count reaches zero.

**`EvalContext`:** Add `emitted: bool` field (set by `$emit`).

**Impact:** Minor — new value variant, one new context field, one new Seq tail type.

### CLI (`src/main.rs`)

**`--allow-network` flag:** Global flag (like `--allow-path`). Default: network blocked. Enables `$tcp-connect`, `$tls-connect`, and (via stdlib) `$fetch`.

**`--allow-write-path <dir>` flag:** Separate from `--allow-path` (read). Grants write access to a directory tree. Default: no write access. `$write` on a file handle obtained from `$open-file "w"` checks this allowlist.

**`$emit` suppresses JSON output:** After evaluation, check `eval_context.emitted`; if true, skip JSON serialization.

**Impact:** Minor — two new flags, output-mode check.

### Sandbox (`doc/12-tooling.md`)

**`$open-file` "r":** Subject to `--allow-path` (read allowlist).

**`$open-file` "w" / "a":** Subject to `--allow-write-path` (write allowlist). Read and write are separate permissions — `--allow-path` does not grant write access.

**`$tcp-connect` / `$tls-connect`:** Subject to `--allow-network`. Blocked by seccomp by default.

**`--no-fs`:** Disables `$open-file` entirely (returns error). `$include` already errors under `--no-fs`. `$tcp-connect` / `$tls-connect` are unaffected by `--no-fs` (they are network, not filesystem).

### Type Checker (`src/typecheck.rs`)

**Phase 1:** All I/O builtins infer as returning `Any`. No changes to inference logic.

**Phase 2 (future):** Add `Type::IO(Box<Type>)` variant; annotation-only, no enforcement.

**Impact:** None in Phase 1. Minor in Phase 2.

## Phased Adoption

### Phase 1: `$emit`, `$stdin`, `$env`, `$open-file`, `$slurp`, `$write`, `$lines`

The full handle abstraction plus `$emit` and `$env`. Enables: formatters (via `$emit`), stdin pipeline integration (via `$stdin` + `$slurp`), secret injection from files (via `$open-file` + `$slurp`), streaming file processing (via `$lines`), and the `stdlib/io.llt` convenience wrappers.

```tinct
$include "stdlib/io.llt"

# stdin as JSON filter
[call $slurp $stdin]
---
[call $parse-json $$]
---
[call $filter [fn [item] [call $> $item.score 0.5]] $$]
---
[call $emit [call $to-yaml $$]]
```

```tinct
$include "stdlib/io.llt"

# Secret injection from file
[db-pass: [call $read-file "/run/secrets/db-password"]]
[config: [db: [host: "db.internal"  password: $db-pass]]]
---
[call $emit [call $to-yaml $$]]
```

**Prerequisites:** `eval-sandbox-flags` sprint, `include-fd-hardening` sprint (fd-based access pattern reused by `$open-file`).

### Phase 2: `$tcp-connect`, `$tls-connect`, `stdlib/net.llt`

Network handles and the `$fetch` / `$fetch-opts` stdlib. Enables HTTP API calls, schema validation, and arbitrary TCP protocol implementations in tinct.

```tinct
$include "stdlib/net.llt"

[schema: [call $fetch "https://schema.internal/v2/deployment"]]
[parsed: [call $parse-json $schema]]
---
[call $validate $parsed $$]
```

**Prerequisites:** Phase 1 complete; `rustls` dependency added to `Cargo.toml`.

### Phase 3: `$write` Atomicity, Streaming Fetch, `$stdin` Streaming

Atomic file writes (`write-to-temp + rename` for crash safety in `$write-file`). Streaming HTTP response body as `$lines` over the socket handle. `$stdin` as a streaming `$lines` source for large inputs. `$fetch-opts` POST/PUT/PATCH support.

**Prerequisites:** Phase 2 complete.

### Phase 4: Effect Annotation (Optional)

Add `Type::IO(inner)` as an informational annotation on I/O builtins. Lint rule: warn when `$emit` appears in a lazy binding. No enforcement. Deferred until there is a concrete need.

**Prerequisites:** Phase 3 complete; type system stability.

### Prerequisites

- Phase 1: `eval-sandbox-flags` sprint, `include-fd-hardening` sprint
- Phase 2: Phase 1 complete; `rustls = "0.23"` in `Cargo.toml`
- Phase 3: Phase 2 complete
- Phase 4: Phase 3 complete; type class design (if any)

### Trigger

- When `doc/whatif/templating.md` is promoted to a sprint — Phase 1 (`$emit`) is a hard dependency
- When a tinct pipeline needs to read secrets from files
- When a tinct pipeline needs to call an HTTP API inside a pipeline stage
- When a tinct program needs to process stdin as a Unix filter

## References

- Moggi, E. (1991). "Notions of computation and monads." *Information and Computation*, 93(1), 55–92. doi:10.1006/inco.1996.2613 — Categorical semantics for monads as models of computation. Foundation for the Haskell IO monad. Considered and rejected for tinct: requires HKTs and type classes disproportionate to a configuration language.
- Peyton Jones, S.L. & Wadler, P. (1993). "Imperative functional programming." *POPL '93*, pp. 71–84. doi:10.1145/158511.158524 — IO monad in Haskell. Rejected for tinct; see §Formal Grounding.
- Peyton Jones, S.L. (2001). "Tackling the awkward squad: monadic input/output, concurrency, exceptions, and foreign-function calls in Haskell." In *Engineering Theories of Software Construction*, NATO ASI Series, IOS Press, pp. 47–96. — Definitive treatment of why lazy I/O (`hGetContents`) is unsound. Motivates tinct's rejection of lazy I/O and adoption of the iteratee pattern.
- Plotkin, G.D. & Pretnar, M. (2009). "Handlers of algebraic effects." *ESOP '09*, LNCS 5502, pp. 80–94. — Algebraic effects as composable alternative to monads. Requires CBPV; incompatible with call-by-need.
- Leijen, D. (2014). "Koka: Programming with row polymorphic effect types." *arXiv:1406.2061*. — Row-polymorphic effect types with HM inference. Rejected: algebraic effects require well-defined call sites, which call-by-need does not guarantee.
- Lindley, S., McBride, C. & McLaughlin, C. (2017). "Do be do be do." *POPL '17*, pp. 500–514. — Frank language. Explicitly notes Haskell-style laziness prevents direct-style effectful programming with handlers.
- Kiselyov, O. (2012). "Iteratees." In *FLOPS '12*, LNCS 7294, pp. 166–181. Springer. — Fold-based stream processing with guaranteed finalization. tinct's `$lines` Seq and SQL cursor Seq follow this pattern.
- Bernardy, J.-P., Boespflug, M., Newton, R.R., Peyton Jones, S. & Spiwack, A. (2018). "Retrofitting linear types." *POPL '18*. — Linear types for GHC. Considered for handle resource safety; rejected due to multiplicity annotation burden. `Rc` reference counting is the pragmatic substitute.
- Miller, M.S. (2006). *Robust Composition*. PhD thesis, Johns Hopkins University. — Object capability model. tinct's `Value::Handle` is capability-like (opaque, unforgeable, passable); OS-level Landlock/seccomp provides ambient enforcement.
- Mycroft, A. (1981). *Abstract interpretation and optimising transformations for applicative programs*. Ph.D. thesis, University of Edinburgh. — Per-argument strictness annotations. All I/O builtins are S (strict) in all arguments.
