# What If: Structured Logging for tinct

**State:** Proposal

What would it take to give tinct programs a structured diagnostic output channel — distinct from `emit` (final result) and from type-checker warnings — with well-defined semantics for literate documentation, application tracing, and log redirection?

## Current State

tinct has two output mechanisms:

- **`emit`** — writes a string to stdout and suppresses the default JSON serialization of the final expression. Used for deliberate program output.
- **Final expression** — evaluated and serialized as JSON to stdout when `emit` is not called.

There is no mechanism for programs to emit diagnostic or trace output separately from the final result. `log` is the natural logarithm (`ln x`), not a print function. Debug output and intermediate state are invisible during evaluation.

### What's Missing

1. No way for a tinct program to emit diagnostic messages without affecting the final result.
2. No structured log entries (level, message, data) — only the final JSON value.
3. No defined output channel for `=== info` in the literate corpus format.
4. No way to separate trace output from final output in literate weave mode.

## Why Structured Logging Matters for tinct

- **Literate documentation.** Examples that narrate their own execution (`=== info` section) need a way to emit trace messages that are captured separately from the evaluated result.
- **Application tracing.** Servers, pipelines, and long-running programs need to emit diagnostics without polluting their data output.
- **Debugging.** Seeing intermediate values during evaluation without changing what the program emits.

## Design

### Output Channel Model

The corpus/literate section labels reflect the SOURCE of the output, not a level chosen by the program:

| Section | Source | Examples |
|---------|--------|---------|
| `=== out` | The program (user code) | `emit` output, final JSON result, user `trace`/log output |
| `=== warn` | The runtime/type-checker (unexpected) | Inferred `Unknown`, arity warnings |
| `=== info` | The runtime/type-checker (informational) | Explicit `@Unknown` annotation note, deprecation notices |
| `=== error` | The runtime | Evaluation errors, type errors |

**User-level logging always goes to `=== out`.** It is program output, regardless of level. A user calling `[trace "hello"]` or `[log-info "starting"]` produces output in `=== out`, interleaved with the final result. The `=== info` section is never populated by user programs — it is exclusively for informational messages from the tinct runtime itself (the type-checker's Info tier, deprecation notices, etc.).

This mirrors the three-tier type diagnostic design: Info-level type notes (e.g., "you explicitly annotated `@Unknown` — that's fine") go to `=== info`; Warn-level type warnings go to `=== warn`. User programs have no access to the `=== info` channel.

### Design

**Keep the log entry as a dict for as long as possible.** Only serialize to a string at the last moment — in the handler, just before output. This preserves the structured data for filtering, enrichment, and routing. Serialization format is the handler's concern, not the call site's.

**Log levels as nominal variants:**

```tinct
# stdlib/log.llt
[union Level [Debug] [Info] [Warn] [Error]]
```

Level values are ordered (`Debug < Info < Warn < Error`) for filtering.

**Base `log` function — level, message, then variadic named KV pairs:**

```tinct
log: [fn [let level@Level message@Str ...kv]
  [emit-log level message kv]]    # emit-log: Rust builtin, dispatches to current handler
```

`message` is a positional `Str` — the human-readable summary, distinct from structured
metadata. The variadic `...kv` collects all named arguments into a dict. The entry stays
as `(Level, Str, Dict)` until `emit-log` passes it to the handler. No string formatting
at the call site. This matches every major structured logging API: Python
`logger.warning("msg", **extra)`, Rust `warn!("msg", k=v)`, Go `slog.Warn("msg", "k", v)`.

**Convenience functions via `partial`:**

```tinct
debug: [partial log Debug]
info:  [partial log Info]
warn:  [partial log Warn]
error: [partial log Error]
```

**Usage:**

```tinct
[warn  "disk almost full"    used: 0.95  path: "/var"]
[info  "request completed"   status: 200  duration-ms: 42]
[debug "computed"            x: x]
[error "connection refused"  peer: addr   retries: 3]
```

**Logger — a bound (formatter, policy) pair:**

A logger is a value that combines a formatter (structures → string) with a policy (string → output sink). The `make-logger` constructor returns a function with the same signature as `log`, so any logger is directly callable or partially applicable:

```tinct
# Formatter: Level Str Dict → Str
text-formatter: [fn [let level@Level message@Str kv@Dict]
  [str [level-name level] "  " message " " [format-kv kv]]]

json-formatter: [fn [let level@Level message@Str kv@Dict]
  [to-json [merge [level: [level-name level]  message: message] kv]]]

# Policy: Level Str Dict Str → Null
# Receives structured data AND pre-formatted string; routes to output sinks.
# Errors → %stderr, everything else → %stdout (via emit).
default-policy: [fn [let level@Level message@Str kv@Dict formatted@Str]
  [if [>= level Error]
    [write-handle %stderr formatted]
    [emit formatted]]]

# Logger constructor: (formatter, policy) → log fn
make-logger: [fn [let formatter@[Fn@Str [Level Str Dict]]
                      policy@[Fn@Null [Level Str Dict Str]]]
  [fn [let level@Level message@Str ...kv]
    [policy level message kv [formatter level message kv]]]]

# Default logger: text format, errors → %stderr, rest → %stdout
default-logger: [make-logger text-formatter default-policy]

# Convenience partial applications on the default logger
debug: [partial default-logger Debug]
info:  [partial default-logger Info]
warn:  [partial default-logger Warn]
error: [partial default-logger Error]
```

Usage — the default `warn`, `error` etc. route appropriately:

```tinct
[warn  "disk almost full"    used: 0.95  path: "/var"]   # → stdout
[error "connection refused"  peer: addr  retries: 3]      # → %stderr
```

**Custom loggers** are created with `make-logger` and used directly or via partial application:

```tinct
# JSON Lines, filtering out Debug, to a log file
file-logger: [make-logger
  json-formatter
  [fn [let level message kv formatted]
    [if [> level Debug]
      [write-handle log-file formatted]
      null]]]

debug-file:  [partial file-logger Debug]
info-file:   [partial file-logger Info]
warn-file:   [partial file-logger Warn]
error-file:  [partial file-logger Error]
```

**`with-log-handler` — scope-local logger rebinding (future):**

A `with-log-handler` combinator that rebinds `debug`/`info`/`warn`/`error` for a scope requires a dynamic binding mechanism. This is deferred — for now, custom loggers are used explicitly. The design is compatible with a future `with-log-handler` that swaps the ambient logger.

### Worked Example: Dual-sink logging (console + syslog)

`stdlib/syslog.llt` provides pure building blocks — formatters and sink factories. It makes no configuration decisions. `myapp.llt` assembles the entire logging stack: syslog host, port, facility, appname, console routing, everything.

```tinct
# stdlib/syslog.llt — building blocks only, no configuration

syslog-severity: [fn [let level@Level]
  [match level
    [case [let _: Debug] 7]
    [case [let _: Info]  6]
    [case [let _: Warn]  4]
    [case [let _: Error] 3]]]

# Formatter factory — caller chooses facility, appname, clock
make-syslog-formatter: [fn [let facility@Int appname@Str clock@ClockCap]
  [fn [let level@Level message@Str kv@Dict]
    [str "<" [+ [* facility 8] [syslog-severity level]] ">"
         [format-timestamp [now clock]] " "
         appname ": " message
         [if [empty? kv] "" [str " " [format-kv kv]]]]]]

# Sink factory — caller provides the bound socket and destination
make-syslog-sink: [fn [let sock host@Str port@Port]
  [fn [let level@Level message@Str kv@Dict formatted-syslog@Str]
    [udp-send sock host port formatted-syslog]]]
```

```tinct
# myapp.llt — the app assembles the full logging stack

[
  net-cap:  %net-cap
  syslog-sock: [udp-bind net-cap]

  # Every configuration decision lives here
  syslog-fmt:  [make-syslog-formatter facility: 1  appname: "myapp"  clock: %clock]
  syslog-sink: [make-syslog-sink syslog-sock "syslog.internal" [@Port 514]]

  # Fan-out policy: assemble it here, not in the library
  log-policy: [fn [let level@Level message@Str kv@Dict formatted@Str]
    [if [>= level Error]                  # console: text-formatted, split by level
      [write-handle %stderr formatted]
      [emit formatted]]
    [syslog-sink level message kv         # syslog: separately formatted
      [syslog-fmt level message kv]]]

  logger: [make-logger text-formatter log-policy]

  debug: [partial logger Debug]
  info:  [partial logger Info]
  warn:  [partial logger Warn]
  error: [partial logger Error]
]

[info  "server starting"              port: 8080  workers: 4]
[warn  "config missing, using default"  key: "timeout"  default: 30]
[error "database connection failed"   host: "db.internal"  err: "timeout"]
```

Console stdout (`=== out`):
```
INFO  server starting port=8080 workers=4
WARN  config missing, using default key=timeout default=30
```

Console stderr:
```
ERROR database connection failed host=db.internal err=timeout
```

Syslog UDP packets to `syslog.internal:514`:
```
<14>May 17 14:23:01 myapp: server starting port=8080 workers=4
<12>May 17 14:23:01 myapp: config missing, using default key=timeout default=30
<11>May 17 14:23:01 myapp: database connection failed host=db.internal err=timeout
```

Design points:
- **Library provides building blocks, app assembles the stack.** `syslog.llt` never decides a host, port, facility, or appname.
- **Capabilities are explicit and app-owned.** `syslog-sock`, `%net-cap`, `%stderr` are all in `myapp.llt`'s dict. The factories receive what they need as parameters.
- **Policy is app logic.** The fan-out routing, the level split between stdout/stderr, whether syslog gets Debug entries — all app decisions, all in one place.
- **`logger` is a value**: passable to library functions, storable in dicts, partially applicable, replaceable per-scope.

In literate weave — log entries and the final result both in `=== out`, runtime info in `=== info`:

```tinct
[warn msg: "using fallback config"]
[port: 8080]
=== info
[T009] @Unknown annotation on result: type is Int
=== out
WARN  msg="using fallback config"
{"port": 8080}
```

### Open Questions

**`emit` suppression — resolved.** The `emitted` flag that currently suppresses final
JSON serialization predates the tinct-native output formatter (`-o` flag / `json.llt`)
and will be removed (`remove-emitted-flag` sprint). After that sprint, `emit` is purely
additive: log calls and the final result both appear in `=== out`. `stdlib/log.llt`
depends on `remove-emitted-flag`.

**Redirect mechanism.** In a running application (not literate mode), where does `emit` output go when used for logging? Currently stdout. Options for log filtering/routing:
- A `with-log-handler` combinator: `[with-log-handler handler body]` — handler receives each `emit` call
- A `LogCap` capability controlling the log destination
- CLI flags like `--log-output path`

**Log level filtering.** How does a program suppress DEBUG messages in production? The level is in the formatted string currently. A structured entry form (Dict) would enable runtime filtering:
```tinct
[log [level: "debug"  msg: "computing"  x: val]]
```
But this requires the runtime to understand the `level:` key, or a `with-log-handler` to filter.

## Prerequisites

- `literate-flags` — defines `=== info` section label and corpus-in-markdown format; the logging design must be consistent with how `=== info` is captured and displayed.
- Decision on the output model (Option A vs B above) before implementation.

## References

- Python `logging` module — hierarchical loggers, handlers, formatters, levels (DEBUG/INFO/WARNING/ERROR/CRITICAL)
- Rust `tracing` crate — structured spans and events; subscriber-based redirection
- Go `slog` — structured logging with `Handler` interface for redirection
- `tinct literate` — the primary consumer of the `=== info` section
