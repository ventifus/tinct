# What If: Tinct Streaming Mode (`-i stream` and `-o stream`)

**State:** Proposal

What would it take to give tinct composable streaming pipelines — programs chained together with lazy record-by-record data flow — without requiring JSON or external tools?

## Current State

tinct uses JSON as the intermediary format for structured data moving between Rust and tinct programs. The profiling pipeline illustrates the pattern:

```sh
# Collect
tinct run --profile spans.json program.llt

# Analyze — requires jq to convert NDJSON before tinct can read it
jq -s '.' spans.json | tinct run -i json scripts/profile/materialize.llt
```

`src/profiling.rs` uses `#[derive(serde::Serialize)]` on `SpanRecord` and `serde_json::to_string_pretty` to write the span file. `src/main.rs` uses `serde_json::json!()` macros in the `describe` command. Wherever Rust needs to hand structured data to a tinct program, `serde_json` appears.

Between tinct programs, there is no streaming mode at all. The output of one tinct program can be piped to another only via `-o json | -i json`, which requires the full output to be collected before the next program sees any of it.

### What's Missing

1. **A streaming input mode.** `-i json` requires the entire input to be a single JSON value. Reading from a long-running process, pipe, or TCP connection requires `jq -s '.'` to collect the full stream before tinct sees any of it.
2. **Lazy record-by-record consumption.** An analysis script that needs only the first ten spans buffers the entire file. There is no input mode that delivers a lazy Seq driven by the source.
3. **A streaming output mode.** `emit` writes raw strings; there is no way to emit structured records lazily. `-o json` collects and serializes the entire return value; it does not produce NDJSON line-by-line as records arrive.
4. **Composable tinct pipelines.** Two tinct programs cannot be connected with a streaming handoff: `tinct run filter.llt | tinct run analyze.llt` has no efficient mode that keeps both sides lazy.
5. **A Rust-side serializer with zero dependencies.** Writing structured data from Rust requires `serde` + `serde_json`; redesigning the profiling flush path to avoid it has required multiple iterations.

## Why Streaming Mode Matters for tinct

**tinct programs compose lazily.** `-i stream | -o stream` connects two tinct programs with a lazy channel: the upstream program emits one record at a time; the downstream program processes each as it arrives. Neither side buffers the full dataset.

```sh
tinct run -i stream -o stream filter.llt < raw.llt \
  | tinct run -i stream summarize.llt
```

**serde_json disappears from the profiling path.** The background flush thread writes span records with `format_span_tinct(&span, &mut buf)` — fixed-schema string formatting with no derive macros and no dependencies. `Arc<EvalContext>` does not need to be shared with the thread.

**External tools are not required.** The profiling pipeline becomes:

```sh
tinct run --profile spans.llt program.llt
tinct run -i stream scripts/profile/materialize.llt < spans.llt
```

No jq. No intermediate format conversion.

**Streaming input is truly lazy.** `%` in `-i stream` mode is a lazy Seq: each `tail` force reads the next expression from the source. `take 10` from a million-record file reads exactly ten records. A live analysis script running against `tail -f spans.llt` consumes spans as they arrive.

**Analysis scripts are already prepared.** Scripts requiring two passes already call `collect` explicitly — `materialize.llt` opens with `spans: [collect %]`. Single-pass scripts stay fully lazy with no changes.

**Any readable or writable source works.** `BufRead`/`Write` are the only requirements: stdin, a file, a named pipe, or a TCP connection.

## Design

### Streaming Input Mode (`-i stream`)

When `--input stream` (or `-i stream`) is specified, `%` is bound to a lazy Seq backed by a `StreamReader`. The Seq is not pre-collected: each `tail` force reads and evaluates the next tinct expression from the source.

The input is **full tinct** — any valid tinct expression is accepted, evaluated in the standard library environment with the same capability sandboxing as any tinct program. There is no restricted subset. A stream record can call stdlib functions, use `include`, or perform any operation the surrounding capability grants permit.

```llt
# spans.llt — written by profiling; each line is a plain tinct dict literal
[id: 0  source-file: "foo.llt"  start-us: 100  end-us: 200  stall-us: 0  stall-kind: []]
[id: 1  source-file: "foo.llt"  start-us: 210  end-us: 350  stall-us: 0  stall-kind: []]
[id: 2  builtin: "builtin-map"  start-us: 355  end-us: 900  stall-us: 0  stall-kind: []]
```

This produces a lazy Seq of three dicts, indistinguishable from the Seq produced by `-i json` reading the equivalent JSON array. Existing analysis scripts require no changes.

`---` document separators are a no-op in stream mode. All expressions across all documents are collected into one flat Seq. `---` may be used as a visual section separator without affecting evaluation.

EOF produces `Value::Dict(IndexMap::new())` — the tinct nil sentinel — terminating the Seq.

### Streaming Output Mode (`-o stream`)

`emit` is the primitive for streaming output. When `-o stream` is active, each call to `emit value` serializes `value` as a tinct expression and writes it as one line to stdout — instead of writing `value` as a raw string. This extends `emit`'s existing role as the side-effect output mechanism:

| flag | `emit value` behaviour |
|------|------------------------|
| `-o raw` (default) | writes `value` as a string (current behaviour) |
| `-o stream` | writes `value_to_tinct(value)` as one expression per line |
| `-o json` | writes `value_to_json(value)` as one JSON object per line (NDJSON) |

A program emits records as it produces them — lazily, one at a time. No Seq required.

```llt
# filter.llt — emit only stalled spans; consumer sees records as they arrive
[each [fn [let s]
  [if [> s.stall-us 0]
    [emit s]
    []]] %]
```

```sh
# Lazy pipeline: filter feeds analyze record-by-record
tinct run -i stream -o stream filter.llt < spans.llt \
  | tinct run -i stream scripts/profile/materialize.llt

# NDJSON output: compatible with jq downstream
tinct run -i stream -o json filter.llt < spans.llt | jq .stall-us
```

`-o stream` is the exact complement of `-i stream`: a stream written by one tinct program can be read by another unchanged.

For programs that return a value rather than using `emit`, `-o stream` treats the return value as the only record and writes it as a single expression (same as `-o json` writes a single JSON object for non-emit programs). If the return value is a Seq, each element is written as a separate record.

If a value is not serializable (a function, handle, or capability), that record is skipped with a warning to stderr.

### StreamReader

`StreamReader<R: BufRead>` wraps any `BufRead` source and parses one balanced tinct expression at a time. It reuses `bracket_count` from `src/repl.rs` — the same bracket-depth counter the REPL uses to detect complete input:

```rust
struct StreamReader<R: BufRead> { inner: R }

impl<R: BufRead> StreamReader<R> {
    fn next_expr(&mut self) -> Option<Result<Value, EvalError>> {
        let mut buf = String::new();
        let mut depth: i32 = 0;
        loop {
            let mut line = String::new();
            match self.inner.read_line(&mut line) {
                Ok(0) if buf.trim().is_empty() => return None,   // clean EOF
                Ok(0) => return Some(Err(/* incomplete expression */)),
                Ok(_) => {}
                Err(e) => return Some(Err(e.into())),
            }
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed == "---" {
                continue;
            }
            depth += bracket_count(&line);
            buf.push_str(&line);
            if depth <= 0 && !buf.trim().is_empty() {
                return Some(eval_stream_expr(&buf, &stdlib_env, &ctx));
            }
        }
    }
}
```

`eval_stream_expr` runs the full tinct evaluation pipeline on `buf` — parse, expand, desugar, resolve, eval — using the shared `EvalContext` and stdlib environment. Each stream record is evaluated within the same context, so records accumulate into the thunk arena and can reference stdlib bindings.

The reader is wrapped in `Arc<Mutex<StreamReader<R>>>` so it can be shared across the lazy Seq spine.

### Value-to-Tinct Serializer (`value_to_tinct`)

`-o stream` requires serializing a `Value` back to tinct source syntax. This is a new Rust function, not covered by the existing `value_to_json` or `value_to_display_string`:

| Value | Output |
|-------|--------|
| `Int(42)` | `42` |
| `Float(3.14)` | `3.14` |
| `Bool(true)` | `true` |
| `String("hello\nworld")` | `"hello\nworld"` |
| `Dict([])` | `[]` |
| `Dict([k: v, ...])` | `[k: v  k2: v2]` with string keys; int keys as `0: v` |
| `Seq { head, tail }` | not valid in element position — error |
| `Function`, `Handle`, `*Cap` | skipped with stderr warning |

String values use the same four escape sequences as the input (`\\`, `\"`, `\n`, `\t`). The serializer is depth-limited (matching the existing `value_to_json` limit) to prevent runaway output for deeply nested structures.

`value_to_tinct` is the generic form. For Rust-side writing (profiling, describe), schema-specific formatters like `format_span_tinct` are more efficient — ~40 lines of `buf.push_str` calls with no `Value` allocation:

```rust
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
```

## What Would Change

### `src/main.rs`

**Current:** `-i json` reads stdin as a JSON value and binds to `%`. `-o json` serializes the result to JSON. No streaming modes.

**Proposed:** `-i stream` creates a `StreamReader` wrapping the source, builds the initial lazy Seq, and binds to `%`. `-o stream` and `-o json` make `emit value` write structured records (`value_to_tinct` and `value_to_json` respectively) rather than raw strings. For programs that return a value without using `emit`, `-o stream` serializes the return value as a tinct expression; `-o json` behaviour is unchanged. The `--profile` output path writes tinct dict literals via `format_span_tinct` rather than `serde_json::to_string_pretty`.

**Impact:** Minor — ~50 lines for the two new modes; existing `-i json` / `-o json` paths unchanged.

### `src/profiling.rs`

**Current:** `#[derive(serde::Serialize)]` on `SpanRecord`; `snapshot_to_json_string` calls `serde_json::to_string_pretty`; the background flush thread requires `Arc<EvalContext>` to reach tinct's JSON serializer.

**Proposed:** Remove `#[derive(serde::Serialize)]`. Replace `snapshot_to_json_string` with `format_span_tinct(s: &SpanRecord) -> String`. Background flush thread needs no `Arc<EvalContext>`. The `profiling-sigint-flush` sprint simplifies substantially.

**Impact:** Minor — contained to `src/profiling.rs` and the flush thread in `src/main.rs`.

### `src/stream.rs` (new)

**Current:** `bracket_count` and `is_balanced` are private to `src/repl.rs`.

**Proposed:** New `src/stream.rs` containing `StreamReader<R: BufRead>`, `value_to_tinct`, `write_tinct_str`, and related helpers. `bracket_count` in `src/repl.rs` becomes `pub(crate)`.

**Impact:** Minor — new file, no behavioral change to REPL.

### `scripts/profile/` and `justfile`

**Current:** Profile targets pipe through `jq -s '.'` before `tinct run -i json`. Analysis script headers reference `-i json`.

**Proposed:**

```sh
tinct run --profile spans.llt program.llt
tinct run -i stream scripts/profile/materialize.llt < spans.llt
```

Analysis script headers updated to reference `-i stream`. `jq` dependency removed from all profile targets.

**Impact:** Minor — justfile target changes only; script logic is unchanged.

### `doc/12-tooling.md`

**Current:** Profiling section references `spans.json` and the `jq -s '.'` pipeline.

**Proposed:** Updated to use `.llt` extension and `-i stream` directly. NDJSON/jq workaround removed.

**Impact:** Minor — documentation update.

## Prerequisites

None. The full tinct evaluation pipeline already handles any expression that would appear in a stream record. `bracket_count` already exists in `src/repl.rs`. `StreamReader` and `value_to_tinct` are new Rust code with no new dependencies beyond `std`.

The `json-remove-serde-dep` sprint depends on this: `src/profiling.rs` is the last significant serde_json user after the other json-* sprints complete.

## References

- Peyton Jones, S. (1987). *The Implementation of Functional Programming Languages.* Prentice Hall. — Lazy I/O: a Seq whose spine is driven by the consumer, not the producer; the model for the lazy `tail` thunk backing `-i stream`.
- ndjson.org (2014). *Newline Delimited JSON.* — The streaming JSON convention this replaces for tinct data handoffs; one record per line, readable incrementally.
- The Rust standard library. `BufRead::read_line`. — The primitive used by `StreamReader::next_expr` to accumulate lines until bracket balance is achieved.
