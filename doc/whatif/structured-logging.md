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

### Settled Design

**Unified `%log: Channel@LogMessage` channel.** Both user code and the runtime send structured log entries to the same channel. The runtime sends type warnings, evaluator diagnostics, and informational notes. User code sends application-level logging. Both use identical `LogMessage` values.

**`LogLevel` — extensible open record, not a closed nominal type:**

```tinct
# stdlib/log.llt
# LogLevel is a record shape, not a closed enum.
# Standard levels are named constants; users add custom levels as records.
LogLevel: [type [ordinal: Int  name: String]]

Trace: [ordinal: 0  name: "TRACE"]
Debug: [ordinal: 1  name: "DEBUG"]
Info:  [ordinal: 2  name: "INFO"]
Warn:  [ordinal: 3  name: "WARN"]
Error: [ordinal: 4  name: "ERROR"]

# User-defined custom levels follow the same shape:
# Audit:   [ordinal: 5  name: "AUDIT"]
# Verbose: [ordinal: -1 name: "VERBOSE"]
```

Level comparison uses ordinal arithmetic: `[>= level.ordinal Warn.ordinal]`. No typeclass needed.

**`LogMessage` — the canonical structured log entry:**

```tinct
# Richer than "log line" — carries structured fields, not just text.
LogMessage: [type [LogMessage
  level:  LogLevel    # {ordinal: Int  name: String}
  parts:  [Seq Any]   # positional message parts — strings, values, anything; formatter decides how to join
  source: Span        # source-code location of the log call (captured by [call-site])
  ...kv]]             # arbitrary structured fields (request-id, component, user-id, etc.)
```

`parts` is a `[Seq Any]` — the raw positional arguments to the log call, preserving their types. The formatter decides how to serialize them: text formatters join with spaces, JSON formatters keep them as typed values, structured formatters inspect them individually. Named fields are merged alongside `level`, `parts`, and `source`.

**`[call-site]` — macro-context primitive that captures the invocation span:**

`trace`, `debug`, `info`, `warn`, `error` are **macros**, not functions or partial applications. Partial application breaks call-site capture because by the time the underlying function runs, the stack says "called from partial application machinery" rather than from the user's source location. Macros expand at the call site so the span is correct.

```tinct
# stdlib/log.llt — log macros, not functions
# Variadic positional parts + variadic named fields.
[macro trace [let ...parts ...kv]
  [send %log [LogMessage level: Trace parts: parts source: [call-site] ...kv]]]

[macro debug [let ...parts ...kv]
  [send %log [LogMessage level: Debug parts: parts source: [call-site] ...kv]]]

[macro info  [let ...parts ...kv]
  [send %log [LogMessage level: Info  parts: parts source: [call-site] ...kv]]]

[macro warn  [let ...parts ...kv]
  [send %log [LogMessage level: Warn  parts: parts source: [call-site] ...kv]]]

[macro error [let ...parts ...kv]
  [send %log [LogMessage level: Error parts: parts source: [call-site] ...kv]]]
```

`[call-site]` is a new zero-arg builtin available only inside macro expansion. It returns the Span of the macro invocation — the `[trace "foo"]` expression's location in source. Implementation: the `call_span` already present in `expand_macro_call_surface` is threaded into EvalContext and read by the builtin. Low-complexity Rust addition.

Note: `[macro-enclosing-fn]` (the name of the enclosing function) is **not implemented** — `SurfaceNode` has no parent pointers, so walking up the AST to find the enclosing `[fn ...]` binding is not feasible without significant infrastructure changes. Callers who want the function name pass it explicitly: `[info "processing" function: "handle-request"]`.

**Usage:**

```tinct
# Multiple positional parts — strings, values, anything. Named fields alongside.
[trace "received value:" foo "with status" bar   pid: 42  cycle-count: 6]
[warn  "disk almost full"                        used: 0.95  path: "/var"  component: "storage"]
[info  "request completed"                       status: 200  duration-ms: 42  request-id: rid]
[error "connection refused to" peer "after" retries "retries"]
```

Parts are preserved as typed values. The text formatter joins them with spaces; the JSON formatter serializes each part individually.

**Corpus/literate routing — level determines section, span determines provenance:**

```text
ordinal >= Error.ordinal  →  === error
ordinal >= Warn.ordinal   →  === warn
else                      →  === out
```

User code calling `[error "db down"]` goes to `=== out` (user output), not `=== error` — the source section reflects the SOURCE of the message, not the level. Runtime T003 warning goes to `=== warn` (runtime warning) because the runtime sends it with `level: Warn`. The `source.file` span tells you exactly where the message originated — user file, stdlib file, or runtime.

**The log router program** (draining `%log` → formatting → routing to sinks) follows the same output program contract as `%emit` formatters: drain the channel, format each `LogMessage`, write to appropriate sinks (`%stderr`, syslog, file). App code assembles the router from building blocks in `stdlib/log.llt` and `stdlib/syslog.llt`.

```tinct
# Logger constructor: (formatter, router) → LogMessage → Null
make-logger: [fn [let formatter router]
  [fn [let msg@LogMessage]
    [formatted: [formatter msg]]
    [router msg formatted]]]

# Formatters: LogMessage → String
# text-formatter: joins parts with spaces, appends named fields
text-formatter: [fn [let msg@LogMessage]
  [fields: [dissoc msg "level" "parts" "source"]]
  [str msg.level.name "  "
       [join " " [map str msg.parts]]
       [if [empty? fields] "" [str "  " [format-kv fields]]]]]

# json-formatter: parts as array, named fields alongside — preserves types
json-formatter: to-json

json-formatter: to-json

# Default router: errors → %stderr, rest → %log channel (emit)
default-router: [fn [let msg@LogMessage formatted@String]
  [if [>= msg.level.ordinal Error.ordinal]
    [write %stderr formatted]
    [send %log-out formatted]]]  # %log-out: a separate string-output channel
```

Because the router gets the full `LogLine`, it can dispatch on any app-controlled field:

```tinct
# Route by component AND level — not possible when only Level is visible
app-router: [fn [let line@LogLine]
  [if [= [get? line "component"] "storage"]
    [partial write-handle %stderr]              # storage component → stderr always
    [match line.level
      [case [let _: Error] [partial write-handle %stderr]]   # other errors → stderr
      [case [let _]        [partial write-handle log-file]]]]]  # rest → log file
```

```text

### Bound Loggers — First-Class Anonymous Macro Values

A common pattern is to create a logger with fields already set — a component name, request ID, service name — so they don't have to be repeated at every call site:

```tinct
storage: [logger component: "storage"  service: "myapp"]

[storage Info "nearing disk exhaustion"  used: 0.95]
[storage Warn "write latency elevated"   latency-ms: 340]
```

`storage` is a **bound logger**: a callable value that pre-merges fields and still captures `[call-site]` at each invocation. This requires **first-class anonymous macro values** — a general mechanism that is part of this whatif.

**Anonymous macro values.** `[macro [let params] body]` without a name creates a macro value, parallel to how `[fn [let params] body]` creates a function value. When the expander encounters a Call where the function resolves to a macro value (not just a registered macro name), it applies macro expansion semantics — the body runs at expansion time, `[call-site]` is available, arguments are passed as quoted expressions.

This is a generic feature, not specific to logging. Any user can create anonymous macro values for other purposes: assertion libraries, tracing, DSLs.

**`logger` returns an anonymous macro value:**

```tinct
# logger is a regular function that closes over base-kv
# and returns an anonymous macro value
logger: [fn [let ...base-kv]
  [macro [let level ...parts ...kv]
    [send %log [LogMessage
      level:  level
      parts:  parts
      source: [call-site]   # span of [storage Info "..."] — captured at invocation
      ...base-kv            # closed over from logger's enclosing scope
      ...kv]]]]             # call-site fields (override bound fields with same key)
```

`base-kv` is captured in the anonymous macro's closure at the time `[logger ...]` is called. When `[storage Info "..."]` is expanded, `[call-site]` captures the span of that expression, not of `[logger ...]` or anything inside `logger`'s body.

**Expander changes required (part of this whatif):**

Currently the expander only recognises macros by name (strings registered in `MacroEnv`). Two additions are needed:

1. `[macro [let params] body]` without a name produces a `Value::Macro` — an anonymous macro value that can be stored in any binding, returned from functions, or passed as arguments. It captures its defining environment as a closure.

2. When processing a Call node, the expander evaluates the function position against the current environment. If the result is a `Value::Macro`, it applies macro expansion (calling the transformer with quoted arguments, making `[call-site]` available, applying the expansion result). This is the generalisation of "macro by name" to "macro by value."

**The calling convention for bound loggers:**

The first positional argument is always the level; remaining positional args are `parts`; named args are merged with the bound fields (call-site named args override bound fields of the same key):

```tinct
[storage Info "disk full"]                        # level + one part
[storage Warn "high latency" latency-ms: 340]     # level + part + named field
[storage Error "failed after" retries "retries"]  # level + multiple parts
[storage Debug "state:" x "→" y  step: n]         # level + mixed parts + named field
```

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
make-syslog-formatter: [fn [let facility@Integer appname@String clock@ClockCap]
  [fn [let level@Level message@String kv@Dict]
    [str "<" [+ [* facility 8] [syslog-severity level]] ">"
         [format-timestamp [now clock]] " "
         appname ": " message
         [if [empty? kv] "" [str " " [format-kv kv]]]]]]

# Sink factory — caller provides the bound socket and destination
make-syslog-sink: [fn [let sock host@String port@Port]
  [fn [let level@Level message@String kv@Dict formatted-syslog@String]
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

  # Fan-out router: full LogLine → handler fn
  # Each arm returns a fn that writes to the appropriate sinks
  log-router: [fn [let line@LogLine]
    [if [>= line.level Error]
      [fn [let fmt@String]               # Error: console stderr + syslog
        [write-handle %stderr fmt]
        [syslog-sink line.level line.message
          [dissoc line "level" "message"] [syslog-fmt line]]]
      [fn [let fmt@String]               # Other: console stdout + syslog
        [emit fmt]
        [syslog-sink line.level line.message
          [dissoc line "level" "message"] [syslog-fmt line]]]]]

  logger: [make-logger text-formatter log-router]

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

```text
INFO  server starting port=8080 workers=4
WARN  config missing, using default key=timeout default=30
```

Console stderr:

```text
ERROR database connection failed host=db.internal err=timeout
```

Syslog UDP packets to `syslog.internal:514`:

```text
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

## What Would Change

### `src/expand.rs` — first-class anonymous macro values

**Current:** Macros are registered by name in `MacroEnv::macros`. The expander checks names only.

**Proposed:**

1. `[macro [let params] body]` without a name → produces `Value::Macro { transformer: Arc<Thunk>, params: Arc<SurfaceNode> }` — an anonymous macro value. Works like `[fn ...]` for functions.
2. When expanding a Call, after checking registered macro names, the expander evaluates the function position against the stdlib env. If the result is `Value::Macro`, it applies expansion: passes args as `Value::Expression`, makes `[call-site]` available (from `call_span`), substitutes the expansion result.

**Impact:** Moderate. Macros become genuinely first-class; any user can create, store, and pass anonymous macro values. The "logger returning a macro value" pattern works as a consequence.

### `src/builtins_meta.rs` — `[call-site]` builtin

**Current:** Nothing.

**Proposed:** A new zero-arg builtin available only during macro expansion. Returns the `Span` of the current macro invocation (the `call_span` from `expand_macro_call_surface`). Represented as a tinct Dict: `[file: "path.llt" line: 42 col: 8 offset: 1234]`. Errors if called outside macro expansion context.

**Impact:** Minor. One new builtin, one new EvalContext field (`current_macro_call_span: Option<Span>`).

### `stdlib/log.llt` — new file

**Current:** Nothing.

**Proposed:** `LogLevel` constants (`Trace`, `Debug`, `Info`, `Warn`, `Error`), `LogMessage` type declaration, `trace`/`debug`/`info`/`warn`/`error` macros, `logger` function, `make-logger` (router + formatter constructor), `text-formatter`, `json-formatter`, `format-kv` helper.

**Impact:** New file, ~80 lines.

### Runtime — `%log: Channel@LogMessage` injection

**Current:** No `%log` channel.

**Proposed:** `eval-programs` injects `%log` into every program's scope alongside `%emit`. The type checker and evaluator send runtime diagnostics (type warnings, errors, info notes) into `%log` with appropriate `level`. A log router tinct program (the last in the pipeline or a separate task) drains `%log` and routes by level.

**Impact:** Moderate. Requires threading `log-ch` through `eval-program` and `builtin-eval`, similar to `emit-ch`. The type checker must send `LogMessage` values into `log-ch` rather than returning `Vec<TypeDiagnostic>` — significant change to the type checker entry points.

### `remove-emitted-flag` sprint

**Current:** `emit` suppresses final JSON serialization. Log output (which calls `emit`) prevents the final result from appearing.

**Proposed:** Remove the `emitted` flag. `emit` is purely additive — log output and the final result both appear. Literate `=== out` contains both.

**Impact:** Moderate. Behaviour change; existing programs using `emit` for logging get the final result in output too. The output formatter (`none.llt`, `json.llt`) already drives this correctly via the new `%emit`/`%stdout` model from `data-streaming`.

## Prerequisites

`data-streaming` acceptance (S-795–S-799) — the `%emit`/`%stdout` output program model that `remove-emitted-flag` depends on. Both sprints must complete before `stdlib/log.llt` can be fully wired.

## References

- Python `logging` module — hierarchical loggers, handlers, formatters, levels (DEBUG/INFO/WARNING/ERROR/CRITICAL)
- Rust `tracing` crate — structured spans and events; subscriber-based redirection
- Go `slog` — structured logging with `Handler` interface for redirection
- `tinct literate` — the primary consumer of the `=== info` section
