# What If: Tinct Stream Format — Stdlib-Closed Normal Form

**State:** Proposal

What would it take to give tinct a native streaming format where records carry computational structure — not just ground values — so that two tinct programs connected by a pipe remain as lazy and composable as a single program?

## Current State

tinct uses JSON as the intermediary format for structured data moving between Rust and tinct programs. The profiling pipeline illustrates the pattern:

```sh
# Collect
tinct run --profile spans.json program.llt

# Analyze — requires jq to collect the stream before tinct can read it
jq -s '.' spans.json | tinct run -i json scripts/profile/materialize.llt
```

`src/profiling.rs` uses `#[derive(serde::Serialize)]` on `SpanRecord` and `serde_json::to_string_pretty`. The `describe` command uses `serde_json::json!()` macros. Between tinct programs, there is no streaming mode at all: connecting two programs requires full serialization to JSON and full deserialization back, buffering the entire dataset before the downstream program sees any of it.

### What's Missing

1. **A streaming input mode.** `-i json` requires the full input to be a single JSON value. Long-running processes, pipes, and TCP connections require `jq -s '.'` to collect the full stream first.
2. **A streaming output mode.** There is no way to emit records lazily as they are produced for downstream consumption.
3. **Preservation of computational structure.** JSON serialization forces every value to a ground scalar. A filter predicate, a partially applied function, a range expression — all are reduced to their output values. The downstream program cannot see or exploit the structure of how those values were produced.
4. **Composable tinct pipelines.** `tinct run filter.llt | tinct run analyze.llt` has no efficient mode that keeps both sides lazy.
5. **A Rust-side serializer with zero dependencies.** Writing structured data from Rust currently requires `serde` + `serde_json`.

## Why Stream Format Matters for tinct

**Records carry structure, not just values.** A stream record is a tinct expression — potentially containing stdlib function calls, lazy sequences, reconstructed closures — that the consumer evaluates in their own context. Two programs connected by a stream pipe are as composable as a single program.

**Computational structure survives the pipe.** A filter predicate with an inlined threshold arrives at the consumer as `[filter [fn [let x] [> x 42]] items]` — a call they can compose lazily. JSON collapses this to a pre-computed list.

**Output formatters are just tinct programs.** `-o stream` is `stdlib/cli/out/stream.llt` — a concurrent tinct task that drains the emit channel, forces the lazy return value, and serializes both. Users write their own output formatters following the same three-step contract. No hardcoded magic in `emit` or the evaluator.

**serde_json disappears from the profiling path.** The background flush thread writes span records with `format_span_tinct` — fixed-schema string formatting with no derive macros and no dependencies. `Arc<EvalContext>` does not need to be shared with the thread.

**External tools are not required.** The profiling pipeline becomes:

```sh
tinct run --profile spans.llt program.llt
tinct run -i stream scripts/profile/materialize.llt < spans.llt
```

**Any readable or writable source works.** `BufRead`/`Write` are the only requirements: stdin, a file, a named pipe, or a TCP connection.

## Design

### Stdlib-Closed Normal Form

The central concept of the stream format is the **stdlib-closed normal form** (SCN) of a value.

An expression is *stdlib-closed* if every free variable in it refers to a stdlib definition — a name bound in prelude or any stdlib module. The SCN of a value V is the minimal stdlib-closed tinct expression that evaluates (in any tinct environment with the standard library) to the same value as V.

The SCN algorithm is defined by cases:

| Value | SCN |
|-------|-----|
| `Int(n)` | `n` |
| `Float(f)` | `f` |
| `Bool(b)` | `true` / `false` |
| `String(s)` | `"s"` with standard escapes (`\\` `\"` `\n` `\t`) |
| `Dict([])` | `[]` |
| `Dict(entries)` | `[k: SCN(v)  ...]` for each entry |
| `Seq { head, tail }` | `[SCN(head) \| SCN(tail)]` |
| `Builtin(name)` | `name` — builtins are always stdlib |
| `Function(params, body, env)` | `[fn [let params] body']` where body' substitutes each non-stdlib free variable `x` with `SCN(env.lookup(x))` |
| `Handle` / `*Cap` | `[]` with stderr warning |

For an unevaluated thunk with expression E and environment env: substitute each non-stdlib free variable in E with `SCN(env.lookup(var))`. Stdlib references are left as-is; non-stdlib function calls are rewritten by inlining, not by forcing.

**Example — the filter case:**

```tinct
[
  threshold: 42
  items:     [0: 1  1: 2  2: 3  3: 4  4: 5]
]
[filter [fn [let x] [> x threshold]] items]
```

SCN computation:

- `filter` → stdlib, left as reference
- `[fn [let x] [> x threshold]]` → user closure, env has `threshold = 42`:
  - body `[> x threshold]`: `>` is stdlib (left), `x` is param (left), `threshold` → inline `42`
  - SCN: `[fn [let x] [> x 42]]`
- `items` → non-stdlib → `[0: 1  1: 2  2: 3  3: 4  4: 5]`

Result:

```tinct
[filter [fn [let x] [> x 42]] [0: 1  1: 2  2: 3  3: 4  4: 5]]
```

The consumer receives a call to `filter` they can compose, `take 10`, or pass to further analysis. `-o json` would have forced this to a pre-computed list.

### `emit` — the new prelude implementation

`emit` currently aliases `builtin-emit` directly (a Rust function that writes a String to stdout). The new implementation replaces this with a channel send:

```tinct
# stdlib/prelude.llt (replacing line 2147)
emit@[doc: "Emit a value to the output channel"]:
  [fn@Null [let v@Any] [send %emit v]]
```

`%emit` is injected by the CLI into every program's environment before evaluation. `emit v` sends `v` to that channel; the formatter on the other end decides how to serialize it. Call sites are agnostic to the serializer — they just `emit`.

The CLI wires the user program's `%emit` to the output program's `%emit`, and injects `%stdout` (a writable handle) into the output program:

```text
user program  ──%emit──▶  output program  ──%stdout──▶  actual stdout
  (producer)                (consumer)
```

- User program calls `emit v` = `[send %emit v]`. It is an **emit producer**.
- Output program calls `[recv %emit ...]` to consume values. It is an **emit consumer**.
- Output program writes to `%stdout` (a writable handle). It never calls `emit`.

The CLI injects `%stdout` as a writable handle into every output program. Output programs are **emit consumers** — they receive from `%emit` and write to `%stdout`. They never call `emit` themselves.

When no `-o` flag is given, the default output program is `stdlib/cli/out/none.llt`: it drains `%emit` discarding all values, and forces `%` discarding all elements. Programs evaluate fully (side effects and `emit` calls fire normally) but nothing is printed. Output requires an explicit `-o` flag.

### `to-tinct` — the SCN function

`to-tinct` is a new prelude function backed by a Rust implementation (`value_to_tinct` in `src/stream.rs`). It takes any tinct value and returns its SCN as a String:

```tinct
[to-tinct [filter [fn [let x] [> x 42]] items]]
# → "[filter [fn [let x] [> x 42]] [0: 1  1: 2  2: 3  3: 4  4: 5]]"
```

`to-tinct` is a regular stdlib function. Any tinct program or output formatter can call it directly. It is not magic — it is the serializer, exposed.

### Streaming Input — `stdlib/codecs/stream.llt`

The stream codec is implemented entirely in tinct, composing three existing primitives:

- `str-chars s` — already in prelude; returns a lazy Seq of single-character strings
- `lines handle` — already in prelude; returns a lazy Seq of line strings from a readable handle
- `eval string` — already a builtin; evaluates a tinct string in the stdlib environment

The codec provides two functions:

**`bracket-count`** — net open bracket depth in a string, accounting for string literals and comments:

```tinct
bracket-count: [fn@Int [let s@String]
  [reduce
    [fn [let st ch]
      [if st.done  st
        [if st.escape  [st | [escape: false]]
          [if st.in-string
            [if [= ch "\\"]  [st | [escape: true]]
            [if [= ch "\""]  [st | [in-string: false]]
                             st]]
            [if [= ch "#"]   [st | [done: true]]
            [if [= ch "["]   [st | [depth: [+ st.depth 1]]]
            [if [= ch "]"]   [st | [depth: [- st.depth 1]]]
            [if [= ch "\""]  [st | [in-string: true]]
                             st]]]]]]]]
    [depth: 0  in-string: false  escape: false  done: false]
    [str-chars s]].depth]
```

**`balanced-exprs`** — groups a Seq of strings into complete balanced tinct expressions, skipping blank lines, comment lines, and `---` separators:

```tinct
balanced-exprs: [fn [let lines]
  [letrec [
    scan: [fn [let ls acc depth]
      [if [= [] ls]
        [if [= "" [trim acc]] []
          [cons acc []]]
        [let line [head ls]
        [let rest [tail ls]
        [let t    [trim line]
        [if [or [= "" t] [starts-with? "#" t] [= "---" t]]
          [scan rest acc depth]
          [let d [+ depth [bracket-count line]]
          [let a [str acc line "\n"]
          [if [and [<= d 0] [not [= "" [trim a]]]]
            [cons a [scan rest "" 0]]
            [scan rest a d]]]]]]]]]
  ]
  [scan lines "" 0]]]
```

The complete `stdlib/codecs/stream.llt` exports both:

```tinct
[
  bracket-count: ...   # as above
  balanced-exprs: ...  # as above
]
```

### Streaming Input Formatter — `stdlib/cli/in/stream.llt`

```tinct
# Stream input formatter
# Reads %stdin as a lazy Seq of tinct expressions, evaluated in stdlib env.
[
  stream: [include %libdir "codecs/stream.llt"]
  [map eval [stream.balanced-exprs [lines %stdin]]]
]
```

`---` separators are skipped by `balanced-exprs`. EOF terminates the Seq. Each element is evaluated by `eval` in the stdlib environment — full tinct, no restricted subset.

### Streaming Output: `emit`, `%emit`, and the Output Program Contract

The CLI runs the user program and the output program as **concurrent tasks**. The user program's `%emit` is wired to the output program's `%emit`. The CLI injects `%stdout` into the output program and passes the user program's lazy return value as the output program's `%`.

```text
user program  ──%emit──▶  output program  ──%stdout──▶  actual stdout
 (emit producer)           (emit consumer)
```

`emit v` in user code sends `v` to `%emit`. Call sites are agnostic to the serializer — the output program decides how to write each received value to `%stdout`.

#### Output Program Contract

Every output program receives two inputs:

- **`%`** — the lazy return value of the previous pipeline stage. Forcing this kicks off the lazy evaluation cascade, driving any `map`/`filter`/`each` computation. The output program is responsible for this forcing — without it, a program that returns a filtered Seq never evaluates.
- **`%emit`** — the emit channel. Values arrive here as the user program evaluates and calls `emit`.

These must be handled **concurrently**: forcing `%` may itself trigger `emit` calls (e.g., `each` with `emit` inside), so the channel drain must run simultaneously or deadlock.

The three responsibilities:

1. **Drain `%emit`** — receive emitted values as they arrive; serialize each to stdout.
2. **Force `%`** — materialize the lazy return value to drive the evaluation cascade; serialize any non-nil Seq elements or scalar return value.
3. **Await both** before exiting.

#### `stdlib/cli/out/stream.llt`

```tinct
[
  # 1. Drain the emit channel concurrently.
  #    Values arrive here as the user program evaluates and calls [emit v].
  drain: [task
    [loop-select
      [recv %emit [fn [let v]
        [write %stdout [to-tinct v]]]]]]

  # 2. Force the return value to drive the lazy evaluation cascade.
  #    Non-nil elements of a Seq return value are also serialized.
  [if [seq? %]
    [each [fn [let x]
      [if [not [= [] x]]
        [write %stdout [to-tinct x]]
        []]] %]
    [if [not [= [] %]]
      [write %stdout [to-tinct %]]
      []]]

  # 3. Wait for drain to finish consuming any emits triggered during forcing.
  [await drain]
]
```

#### `stdlib/cli/out/json.llt` (updated for NDJSON)

```tinct
[
  drain: [task
    [loop-select
      [recv %emit [fn [let v]
        [write %stdout [to-json v]]]]]]

  [if [seq? %]
    [each [fn [let x]
      [if [not [= [] x]]
        [write %stdout [to-json x]]
        []]] %]
    [if [not [= [] %]]
      [write %stdout [to-json %]]
      []]]

  [await drain]
]
```

A user writing a custom output formatter follows the same contract: start a `drain` task on `%emit`, force `%`, await the task. Only the serializer changes.

#### Programs emit records lazily

```tinct
# filter.llt — emit matching spans; forcing % drives the each+emit cascade
[each [fn [let s]
  [if [> s.stall-us 0]
    [emit s]
    []]] %]
```

```sh
tinct run -i stream -o stream filter.llt < spans.llt \
  | tinct run -i stream analyze.llt
```

### Pipeline Composition

```sh
# Lazy tinct → tinct pipeline
tinct run -i stream -o stream filter.llt < spans.llt \
  | tinct run -i stream analyze.llt

# Stream → jq: use -o json for NDJSON-compatible output
tinct run -i stream -o json filter.llt < spans.llt | jq .stall-us

# Custom output formatter: user writes their own
tinct run -i stream -o my-formatter filter.llt < spans.llt

# Profiling: Rust writer → tinct reader
tinct run --profile spans.llt program.llt
tinct run -i stream scripts/profile/materialize.llt < spans.llt
```

Each `-o stream | -i stream` stage receives stdlib-closed expressions, evaluates them, and returns a new Seq for the next formatter to process. Structure flows through the pipeline rather than being collapsed at each boundary.

### Rust-Side Serializer

For Rust programs writing stream records (profiling, describe), `format_span_tinct` is the degenerate case of SCN: a dict of scalar values has no free variables and is trivially stdlib-closed.

```rust
// src/stream.rs

fn write_tinct_str(s: &str, buf: &mut String) {
    buf.push('"');
    for ch in s.chars() {
        match ch {
            '"'  => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\t' => buf.push_str("\\t"),
            c    => buf.push(c),
        }
    }
    buf.push('"');
}

fn write_tinct_opt(val: Option<impl Display>, none: &str, buf: &mut String) {
    match val {
        Some(v) => buf.push_str(&v.to_string()),
        None    => buf.push_str(none),
    }
}

// General SCN serializer for Value — used by `to-tinct` builtin.
// Closure reconstruction (Value::Function) is handled by walking the body
// expression and substituting non-stdlib env bindings.
pub fn value_to_tinct(val: &Value, ctx: &Arc<EvalContext>) -> String { ... }

// Schema-specific fast path for profiling — no Value allocation.
pub fn format_span_tinct(s: &SpanRecord) -> String {
    let mut buf = String::new();
    buf.push_str("[id: ");
    buf.push_str(&s.id.to_string());
    buf.push_str("  materialize-parent: ");
    write_tinct_opt(s.materialize_parent, "[]", &mut buf);
    // ... all 14 fields ...
    buf.push(']');
    buf
}
```

`format_span_tinct` requires no `EvalContext` — the background flush thread calls it directly and appends to the open file handle.

## What Would Change

### `stdlib/codecs/stream.llt` (new)

**Current:** Nothing.

**Proposed:** `bracket-count` and `balanced-exprs` as defined above. Pure tinct — no new Rust builtins. Both functions are independently useful for REPL-like tools, syntax highlighters, and custom stream parsers.

**Impact:** New file, ~40 lines.

### `stdlib/cli/in/stream.llt` (new)

**Current:** Nothing.

**Proposed:** `[map eval [stream.balanced-exprs [lines %stdin]]]` — one expression, no new builtins. Activated by `-i stream`.

**Impact:** New file, ~4 lines.

### `stdlib/cli/out/` — all formatters rewritten

Every existing output formatter follows the old contract: receive `%`, compute a String, return it; the CLI materializes and prints. All must be rewritten to the new contract: drain `%emit`, force `%`, emit each serialized value, await.

The shared rewrite pattern — substituting only the serializer. Output programs are emit consumers: they receive from `%emit` and write to `%stdout`. They never call `emit`:

```tinct
[
  drain: [task
    [loop-select
      [recv %emit [fn [let v]
        [write %stdout [SERIALIZER v]]]]]]

  [if [seq? %]
    [each [fn [let x]
      [if [not [= [] x]]
        [write %stdout [SERIALIZER x]]
        []]] %]
    [if [not [= [] %]]
      [write %stdout [SERIALIZER %]]
      []]]

  [await drain]
]
```

**`stdlib/cli/out/stream.llt` (new)** — `SERIALIZER = to-tinct`. One new file, ~15 lines.

**`stdlib/cli/out/json.llt` (rewrite)** — `SERIALIZER = to-json`. Currently `[call $builtin-to-json %]`, returning a String. Rewritten to the concurrent contract. Produces NDJSON: one JSON value per emit, one JSON value per Seq element from the return value.

**`stdlib/cli/out/json-pretty.llt` (rewrite)** — same as json but with `to-json-pretty`. Produces pretty-printed NDJSON.

**`stdlib/cli/out/raw.llt` (rewrite)** — currently: if String return → write it; if Seq → `[join "\n" %]`; else error. New: `SERIALIZER = [fn [let v] [if [str? v] v [str v]]]` — emit each received value as its string representation. The explicit Seq-error is removed; Seq elements arrive naturally through the drain loop.

**`stdlib/cli/out/llt.llt` (rewrite)** — `SERIALIZER = llt-repr`. Currently `[call $llt-repr %]`. Rewritten to emit `[llt-repr v]` for each record.

**`stdlib/cli/out/yaml.llt` (rewrite)** — currently inline formatter returning `[yaml %]` string. Rewritten: inline `yaml` function definition remains; `SERIALIZER = yaml`. Each emitted/returned value is formatted as YAML and written.

**`stdlib/cli/out/csv.llt` (rewrite)** — currently `[csv %]`. Note: CSV has no natural per-record form (the header depends on knowing all rows). The rewrite emits a header line from the first received record, then data lines for all subsequent records — enabling true streaming CSV from a Seq. SERIALIZER is replaced by a stateful header-tracking approach.

**`stdlib/cli/out/toml.llt` (rewrite)** — currently `[toml %]`. Rewritten to `SERIALIZER = toml`.

**`stdlib/cli/out/env.llt` (rewrite)** — currently `[env %]`. Rewritten to `SERIALIZER = env`.

**`stdlib/cli/out/none.llt` (rewrite)** — currently returns `""` and is only invoked by explicit `-o none`. New role: **the default output program** when no `-o` flag is given. Drains `%emit` discarding all values, forces `%` discarding all elements (driving the lazy evaluation cascade for side effects), writes nothing to `%stdout`.

```tinct
# stdlib/cli/out/none.llt
[
  drain: [task
    [loop-select
      [recv %emit [fn [let v] []]]]]

  [each [fn [let x] []] %]   # force % to drive evaluation cascade

  [await drain]
]
```

**Impact:** All ten formatters rewritten. Old "return a String" contract deleted entirely.

### `to-tinct` in prelude

**Current:** No `to-tinct` function.

**Proposed:** `to-tinct: builtin-to-tinct` — prelude wrapper over `value_to_tinct` Rust function in `src/stream.rs`. Returns the SCN of any value as a String. No mode magic — it is a plain function callable anywhere.

**Impact:** New builtin registration + prelude entry.

### `src/stream.rs` (new)

**Current:** Nothing.

**Proposed:** `value_to_tinct(val: &Value, ctx: &Arc<EvalContext>) -> String` (the general SCN serializer), `format_span_tinct(s: &SpanRecord) -> String` (the profiling fast path), and string helper functions (`write_tinct_str`, `write_tinct_opt`). `bracket_count` in `src/repl.rs` becomes `pub(crate)` for reference but is not used by this module — `bracket-count` is implemented in tinct.

**Impact:** New file, ~150 lines.

### `src/main.rs` — emit channel wiring and deletion of special-case output paths

**Current:** Three separate code paths all do materialization and serialization in Rust:

1. **`run_eval` output path (~line 2252):** When `-o` is present, materializes the formatter's return value, asserts it is a `Value::String`, and prints it directly. Comment: *"the pipeline's last expression is an output formatter that returns a String."* The formatter is expected to return a fully-rendered String.

2. **`run_literate_eval` (~line 2941):** Always materializes the return value, calls `visit_value(&val, &eval_ctx, 0, &JsonVisitor, ...)`, then `serde_json::to_string_pretty`. Comment: *"Always serialize to JSON (emit is purely additive)."*

3. **`run_literate_weave` per-block (~line 3305):** Same `visit_value` + `JsonVisitor` + `serde_json::to_string` per block. Comment: *"Always serialize the result to JSON (emit is additive)."*

All three embed the assumption that the CLI is responsible for output serialization and that `emit` is a side-effect bolted on top. All three must be deleted.

**Proposed:** The CLI creates `%emit` and `%emit` channels, injects them into both the user program and the output formatter, and spawns both as concurrent tasks. The CLI's job after launching is to wait for both tasks to complete — nothing more. No materialization, no serialization, no String assertion.

Specifically deleted:

- The `--eval` flag and its handling (`force_eval` branch in `run_eval`) — redundant with the default `none.llt` output program
- The `Value::String` match + `print!` in `run_eval` (and the associated error for non-String formatter return)
- The `materialize` + `visit_value` + `serde_json::to_string_pretty` block in `run_literate_eval`
- The `visit_value` + `serde_json::to_string` block in `run_literate_weave`
- The `JsonVisitor` and `visit_value` imports in `main.rs` (no longer needed in CLI paths)
- The `serde_json::to_string_pretty` / `serde_json::to_string` calls in all output paths

`run_literate_eval` and `run_literate_weave` gain the same channel-wiring treatment as `run_eval`.

**Impact:** Moderate — replaces the sequential "eval → materialize → print" model with "eval + formatter as concurrent tasks". The output formatters (`cli/out/*.llt`) become the sole owners of serialization and stdout writing.

**Note on `--eval`:** The `--eval` flag is made redundant by this model — running without `-o` already uses `none.llt`, which drains `%emit` and forces `%` without writing anything. `--eval` must be removed from the CLI and all documentation.

### `src/profiling.rs`

**Current:** `#[derive(serde::Serialize)]` on `SpanRecord`; `snapshot_to_json_string` calls `serde_json::to_string_pretty`.

**Proposed:** Remove `#[derive(serde::Serialize)]`. Replace `snapshot_to_json_string` with `format_span_tinct`. Background flush thread is self-contained.

**Impact:** Minor — contained to `src/profiling.rs` and the flush thread in `src/main.rs`.

### `scripts/profile/` and `justfile`

**Current:** Profile targets pipe through `jq -s '.'` before `tinct run -i json`.

**Proposed:**

```sh
tinct run --profile spans.llt program.llt
tinct run -i stream scripts/profile/materialize.llt < spans.llt
```

`jq` dependency removed. Analysis script headers updated.

**Impact:** Minor — justfile only; script logic unchanged.

### `doc/12-tooling.md`

**Current:** References `spans.json` and `jq -s '.'`.

**Proposed:** Updated to `.llt` extension and `-i stream`. NDJSON/jq workaround removed.

**Impact:** Minor — documentation update.

## Prerequisites

Every primitive used by the input formatter (`str-chars`, `lines`, `eval`, `map`, `filter`, `starts-with?`, `trim`, `str`, `reduce`) already exists. The stream codec is pure tinct. The output formatter uses `loop-select`, `recv`, and channel primitives from the async concurrency layer; `send` and `channel` are already registered builtins.

The `json-remove-serde-dep` sprint depends on this: `src/profiling.rs` is the last significant serde_json user after the other json-* sprints complete.

## References

- Peyton Jones, S. (1987). *The Implementation of Functional Programming Languages.* Prentice Hall. — Lazy I/O: a Seq whose spine is driven by the consumer; the model for the lazy `tail` thunk in the stream reader. Partial evaluation by specialization: the formal model for the SCN closure case.
- Jones, N.D., Gomard, C.K., and Sestoft, P. (1993). *Partial Evaluation and Automatic Program Generation.* Prentice Hall. — Binding-time analysis; the formal basis for the SCN algorithm's treatment of closures and free variable substitution.
- ndjson.org (2014). *Newline Delimited JSON.* — The streaming JSON convention; `-o json` with `[emit [to-json v]]` produces NDJSON as a bridge to JSON-aware downstream tools.
