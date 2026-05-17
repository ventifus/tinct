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

**`with-log-handler` — redirect and filter:**

```tinct
# JSON Lines format, Info and above only
[with-log-handler [fn [let level@Level message@Str kv@Dict]
  [if [>= level Info]
    [emit [to-json [merge [level: [level-name level]  message: message] kv]]]
    null]]
  [my-program]]
```

The handler receives the raw `Level`, `Str` message, and `Dict` of KV pairs. It can:
- Filter by level (discard Debug in production)
- Enrich with metadata (timestamp, request-id, caller) before formatting
- Choose the output format (JSON Lines, logfmt, human-readable text)
- Route to different sinks (stdout, file, network)

**Default handler** (no `with-log-handler`): formats as `LEVEL  key=value ...` text and calls `emit`. Goes to `=== out`.

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
