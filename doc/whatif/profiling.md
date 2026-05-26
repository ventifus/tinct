# What If: Program Profiling and Call Tracing for tinct

**State:** Accepted — 2026-05-25

What would it take to answer "why is this tinct program slow?" and "how does this program flow?" — with attribution that crosses the Rust/tinct boundary transparently?

## Current State

tinct has no profiling or tracing infrastructure. A `just bench` target runs `cargo bench`, but there are no benchmark files. `Cargo.toml` has no dev-dependencies for profiling crates. Known hot paths — `Environment::get` O(depth) chain walk, `IndexMap::new()` per PendingBuiltin creation, `Rc::new(Thunk)` at 113 sites in `eval.rs` — are documented from prior code review but unmeasured on real workloads.

Two accepted whatifs gate explicitly on profiling data:

- `string-interning.md` — "profiling confirms `String` allocation/comparison is top-5 hotspot"
- `union-find-substitution.md` — "profiling confirms average TypeVar chain depth ≥4"

Without a profiling harness, these decisions cannot be made soundly.

### What's Missing

1. No way to answer "why is this program slow?" — Rust or tinct hotspot is unknown.
2. No way to answer "how does this program flow?" — call chains across the Rust/tinct boundary are invisible.
3. No benchmarks for before/after comparison when optimizations land.
4. Performance-gated proposals (`string-interning`, `union-find-substitution`) blocked on missing measurement.

## Why Profiling Matters for tinct

### The Rust/tinct Boundary Problem

tinct executes Rust builtins that schedule tinct callbacks as `PendingCall`/`PendingBuiltin`. A Rust flamegraph shows:

```text
eval() → builtin_map() → eval() → eval() → eval() → ...
```

Each individual `eval()` invocation looks cheap. That `builtin_map` drove 10,000 tinct function evaluations consuming 400ms is invisible — attribution is lost at every boundary crossing. The question "which Rust builtin is responsible for the most tinct re-entry?" cannot be answered by standard Rust profiling tools.

The key cross-boundary call patterns where this matters:

| Rust builtin | Tinct re-entry | Cost structure |
|---|---|---|
| `builtin_map` | one PendingCall per element | O(n) re-entries |
| `builtin_filter` | one PendingCall per element | O(n) re-entries |
| `builtin_reduce` | one PendingCall per accumulation step | O(n) re-entries |
| `builtin-select-once` | one PendingCall per iteration of `loop-select` | unbounded |

`include` is not in this table — per `include-decomposition.md`, it is defined in `prelude.llt` using the eight Rust primitives (`load`, `expand`, `eval`, `blake3`, etc.). It appears in profiling output at its source location in prelude, not as a Rust builtin.

### The Lazy Evaluation Attribution Problem

In a strict language, "who called this function?" is unambiguous. In a lazy language, a thunk is created in one context and forced in another. A program like:

```tinct
result: [map expensive-fn items]
[emit [str-join ", " result]]
```

creates the map thunk at the `result` binding, but `expensive-fn` is not called until `emit` materializes it. A profiler that attributes cost to creation sites would report `result: [map ...]` as expensive — but `emit` is the one demanding the work.

Both attribution contexts are useful for different questions, and the profiler tracks both:

- **Materialization-context** (`scripts/profile/materialize.llt`): time is charged to the span that materialized a thunk. Answers "what demanded this work?" Useful for identifying what to make lazier.
- **Creation-context** (`scripts/profile/create.llt`): time is charged to the span that created the thunk. Answers "what allocated this work?" Useful for finding what to eliminate or restructure. (GHC's profiler uses this exclusively, on the grounds that the materializing context is non-deterministic — it changes with evaluation order. Both views are provided here.)

## Design

### Rust/Tinct Boundary

The profiling system divides cleanly into two responsibilities:

**Rust** — collect raw timing data. The evaluator runs in Rust and is the only place where `Instant::now()` can bracket a thunk force. Rust accumulates a flat `Vec<SpanRecord>` during evaluation and, when evaluation completes, converts it to a tinct `Seq` of plain dicts — no JSON, no formatting.

**Tinct** — format all output. Three analysis scripts in `scripts/profile/` each take the span `Seq` as input. No JSON writing happens in Rust.

This boundary means no `serde_json` dependency in the profiling implementation. The Perfetto trace is produced by `scripts/profile/trace.llt` returning a tinct dict serialized via `-o json`, not by Rust.

### Span Collection (Rust)

Every thunk force is a **span**: a timed interval identified by source location. The Rust evaluator records one `SpanRecord` per forced thunk:

```rust
struct SpanRecord {
    id:                  u64,
    materialize_parent:  Option<u64>,         // span that materialized this thunk
    create_parent:       Option<u64>,         // span active when this thunk was created
    create_time_us:      u64,                 // wall-clock when thunk was created (for flow arrows)
    source_file:         Arc<str>,            // from the thunk's Span; empty for Rust builtins
    source_start:        u32,                 // byte offset
    source_end:          u32,                 // byte offset
    source_text:         String,              // leading characters of source at this span
    builtin_name:        Option<&'static str>,// set for Rust builtins; None for tinct exprs
    origin_builtin:      Option<Arc<str>>,    // from Thunk.origin (Arc<str>)
    start_us:            u64,                 // microseconds since program start
    end_us:              u64,                 // microseconds since program start
    stall_us:            u64,                 // time blocked in I/O or async wait; 0 for compute
    stall_kind:          Option<&'static str>,// "io" | "net" | "channel" | "timer"
}
```

`create_parent` is recorded at thunk construction time by reading the `ProfilingCollector`'s current-span stack. `materialize_parent` is recorded when the thunk is materialized. The `ProfilingCollector` maintains a stack of open span IDs; `open_span` pushes, `close_span` pops. Both fields are `None` for toplevel spans.

`source_file`/`source_start`/`source_end` are taken directly from the thunk's existing `Span` — no new span infrastructure. `source_text` is a fixed-length snippet extracted from the source text already held by the include cache.

When evaluation completes, `ProfilingContext::into_value()` converts the `Vec<SpanRecord>` into a `Value::Seq` of plain dicts. Each dict uses kebab-case keys matching tinct conventions:

```tinct
# One dict per span — the schema analysis scripts operate on:
[
  id:                 Int           # unique span ID
  materialize-parent: Int | []      # ID of the span that materialized this thunk; [] if toplevel
  create-parent:      Int | []      # ID of the span active when this thunk was created; [] if toplevel
  create-time-us:     Int           # wall-clock microseconds when thunk was created (for flow arrows)
  source-file:        Str           # e.g. "versions.llt"; "" for Rust builtins
  source-start:       Int           # byte offset into source-file
  source-end:         Int           # byte offset into source-file
  source-text:        Str           # source snippet for display
  builtin:            Str | []      # builtin name if this is a Rust builtin
  origin-builtin:     Str | []      # originating Rust builtin if cross-boundary
  start-us:           Int           # wall-clock microseconds since program start
  end-us:             Int           # wall-clock microseconds since program start
  stall-us:           Int           # microseconds blocked in I/O or async wait; 0 for compute
  stall-kind:         Str | []      # "io" | "net" | "channel" | "timer"; [] for compute spans
]
```

Tinct spans are identified by `(source-file, source-start, source-end)`. Rust builtin spans are identified by `builtin`. No naming layer is needed.

`materialize-parent` and `create-parent` are independent: a thunk created in one context may be materialized by a completely different context later. The gap between them is the lazy decoupling — visible as flow arrows in the Perfetto trace.

### Stall Attribution

Wall-clock time alone conflates CPU work with waiting. A span showing 450ms for `builtin-connect` could mean slow code or network latency — indistinguishable without a breakdown.

`stall-us` records the time a span spent blocked, not computing. `stall-kind` records why:

| `stall-kind` | Cause | Builtins |
|---|---|---|
| `"io"` | OS filesystem syscall | `builtin-slurp`, `builtin-write-handle`, `builtin-open` |
| `"net"` | Network wait | `builtin-connect`, `builtin-http-request` |
| `"channel"` | Async channel wait | `builtin-recv`, `builtin-select-once` |
| `"timer"` | Deliberate sleep or deadline | `builtin-sleep`, `builtin-timeout` |

I/O builtins self-instrument their stall by bracketing the blocking call:

```rust
// In builtin_connect, builtin_slurp, builtin_write_handle, etc.
let stall_start = Instant::now();
let result = the_blocking_io_call().await;
ctx.profiling_record_stall(stall_start.elapsed().as_micros() as u64, "net");
```

For the async evaluator (runtime-v2), a single builtin may suspend and resume multiple times — `loop-select` waiting between channel messages, for example. The task scheduler signals `span_suspend` and `span_resume`; the collector accumulates the differences into `stall_us` across all cycles before the span closes.

The `cpu-ms` column in the profile table is derived: `cpu-ms = (end-us - start-us - stall-us) / 1000`. Formatters compute it from the raw fields; it is not stored separately.

### Cross-Boundary Attribution

When a Rust builtin creates a `PendingCall` to invoke a tinct function, the call is tagged with the originating builtin name:

```rust
PendingCall {
    func: callback_thunk,
    args: vec![element_thunk],
    named_args: IndexMap::new(),
    origin_builtin: Some("builtin-map"),
}
```

The child span inherits `origin_builtin` as its `origin-builtin` field. This makes the Rust→tinct re-entry cost visible in every formatter without any formatter needing special knowledge of the evaluator.

### Collection and Analysis

**Collection** (`--profile`) writes raw span data to a file. The file is the lossless archival record — all fields preserved, all attribution data present.

**Analysis** is just running tinct programs against the span file. No special subcommand. Scripts live in `scripts/profile/` alongside `docgen.llt`.

```sh
# Collect
tinct run --profile spans.json program.llt

# Analyze — standard tinct pipeline
tinct run -i json                scripts/profile/materialize.llt  < spans.json           # materialization table
tinct run -i json                scripts/profile/create.llt < spans.json           # creation table
tinct run -i json -o json        scripts/profile/trace.llt  < spans.json > trace.json  # Perfetto JSON
tinct run -i json                my-analysis.llt            < spans.json           # custom
```

The raw span file (`spans.json`) is JSON — a standard tinct program input via `-i json`. Perfetto trace format is a derived output from `scripts/profile/trace.llt`; it is lossy (cannot round-trip back to spans), which is fine because `spans.json` is the archive. The trace is just one view.

### Analysis Scripts

**`scripts/profile/materialize.llt`** — materialization-context hotspot table. Self-time requires two passes: first build a `children-wall` index (total wall time of direct children per span ID), then compute `self_us = wall_us - children_wall`. The Seq must be materialized with `collect` for two-pass traversal. Stall-kind omitted from the table (too coarse to aggregate); visible per-span in the Perfetto trace.

```tinct
# scripts/profile/materialize.llt
# Input: %  — Seq of span dicts (via tinct run -i json scripts/profile/materialize.llt < spans.json)
# Output: emitted to stdout directly

[
  strings: [include %libdir "strings.llt"]

  # Materialize once — two passes required
  spans: [collect %]

  # Pass 1: children-wall index — total wall time of direct children per span ID
  child-groups:  [group-by [fn [let s] s.materialize-parent]
                   [filter [fn [let s] [not [= [] s.materialize-parent]]] spans]]

  children-wall: [build-dict
                   [map [fn [let g]
                          [key:   g.key
                           value: [reduce [fn [let acc s] [+ acc [- s.end-us s.start-us]]]
                                          0 g.value]]]
                        [each-kv child-groups]]]

  span-key: [fn [let s]
    [if [= [] s.builtin]
      [str s.source-file ":" s.source-start "-" s.source-end]
      s.builtin]]

  # get-or returns default 0 for leaf spans with no children
  self-us: [fn [let s]
    [- [- s.end-us s.start-us]
       [get-or children-wall s.id 0]]]

  cpu-us: [fn [let s] [- [self-us s] s.stall-us]]

  aggregate: [fn [let key group]
    [location: key
     calls:    [length group]
     cpu-ms:   [/ [reduce [fn [let acc s] [+ acc [cpu-us s]]]  0 group] 1000]
     wait-ms:  [/ [reduce [fn [let acc s] [+ acc s.stall-us]]  0 group] 1000]
     total-ms: [/ [reduce [fn [let acc s] [+ acc [- s.end-us s.start-us]]] 0 group] 1000]]]

  grouped: [group-by span-key spans]
  rows:    [sort-by [fn [let a b] [> a.total-ms b.total-ms]]
              [map [fn [let g] [aggregate g.key g.value]] [each-kv grouped]]]

  # / always returns Float; to-int truncates to Int; mod for remainder
  fmt-ms: [fn [let ms]
    [tenths: [round [* ms 10]]]
    [whole:  [to-int [/ tenths 10]]]
    [frac:   [mod tenths 10]]
    [strings.pad-left [str whole "." frac] 9 " "]]

  fmt-loc: [fn [let r]
    [str [strings.pad-left [str r.calls] 7 " "]
         [fmt-ms r.cpu-ms] [fmt-ms r.wait-ms] [fmt-ms r.total-ms]
         "   " r.location]]

  sep: "────────────────────────────────────────────────────────────────────────────────"
  hdr: "  calls   cpu_ms   wait_ms  total_ms   location"

  [emit [str sep "\n" hdr "\n" sep "\n" [str-join "\n" [map fmt-loc rows]] "\n" sep]]
]
```

**`scripts/profile/create.llt`** — creation-context hotspot table. Groups spans by their creator (the `create-parent` span's location) rather than their materializer. Answers "what allocated this work?"

**`scripts/profile/trace.llt`** — Perfetto Chrome Trace Event Format. `cname` color coding works in `chrome://tracing` (legacy viewer); `ui.perfetto.dev` ignores `cname` but the `args.stall-kind` field can be used to filter. Flow events connect each span's creation site to its materialization site.

```tinct
# scripts/profile/trace.llt
# Input: %  — Seq of span dicts (tinct run -i json -o json scripts/profile/trace.llt < spans.json)
# Output: tinct dict — serialized to JSON by -o json flag; no codecs import needed

[
  span-name: [fn [let s]
    [if [= [] s.builtin]
      [str s.source-file ":" s.source-start "-" s.source-end]
      s.builtin]]

  stall-color: [fn [let s]
    [match s.stall-kind
      "net":     "terrible"
      "io":      "bad"
      "channel": "yellow"
      "timer":   "olive"
      _:         "good"]]

  span-to-event: [fn [let s]
    [ph:   "X"
     ts:   s.start-us
     dur:  [- s.end-us s.start-us]
     name: [span-name s]
     cname: [stall-color s]
     pid:  1
     tid:  1
     args: [source:             s.source-text
            materialize-parent: s.materialize-parent
            create-parent:      s.create-parent
            origin:             s.origin-builtin
            cpu-us:             [- [- s.end-us s.start-us] s.stall-us]
            stall-us:           s.stall-us
            stall-kind:         s.stall-kind]]]

  # Flow events link creation site (create-time-us) to materialization site (start-us).
  # Use explicit int-keyed dict [0: a  1: b] — [[a] [b]] would be parsed as a function call.
  flow-start: [fn [let s]
    [if [= [] s.create-parent] []
      [ph: "s"  id: s.id  ts: s.create-time-us  name: "lazy"  pid: 1  tid: 1]]]

  flow-end: [fn [let s]
    [if [= [] s.create-parent] []
      [ph: "f"  id: s.id  ts: s.start-us  name: "lazy"  pid: 1  tid: 1  bp: "e"]]]

  flows: [filter [fn [let e] [not [= [] e]]]
           [flat-map [fn [let s] [0: [flow-start s]  1: [flow-end s]]] %]]

  # Return tinct dict — -o json serializes it to the Perfetto JSON format
  [traceEvents: [concat [map span-to-event %] flows]]
]
```

### Sample Output

`tinct run -i json scripts/profile/materialize.llt < spans.json` (materialization-context):

```text
────────────────────────────────────────────────────────────────────────────────────────────
  calls   cpu_ms   wait_ms  total_ms   location
────────────────────────────────────────────────────────────────────────────────────────────
      1    789.0       0.0   1234.0    <toplevel>
      3      2.0       0.0    610.0    builtin-map
   8432    190.0       0.0    190.0    versions.llt:12:3-12:45    [fn [let crate] ...
      1     60.0       0.0    200.0    builtin-reduce
     12     30.0       0.0     30.0    versions.llt:31:5-31:62    [fn [let v] [split ...
      1      5.0     445.0    450.0    builtin-connect
────────────────────────────────────────────────────────────────────────────────────────────
```

`cpu_ms` = self-CPU time (span wall time minus direct children, minus stall). `wait_ms` = self-stall time. `total_ms` = full wall time including all descendants. `builtin-connect` shows 445ms of network wait — immediately distinguishable from the 5ms of actual work. Stall-kind (`"net"`, `"io"`, etc.) is not shown in the table; the Perfetto trace communicates it via color coding per individual span.

`tinct run -i json -o json scripts/profile/trace.llt < spans.json > trace.json` — load `trace.json` in [Perfetto UI](https://ui.perfetto.dev/) or `chrome://tracing` for an interactive flame graph with flow arrows between creation and materialization sites.

### Rust-Level Profiling

For Rust-specific hotspots, the standard `cargo flamegraph` workflow is used without code changes:

```sh
cargo install flamegraph
cargo flamegraph --bin tinct -- samples/versions.llt
```

| Tool | Answers |
|---|---|
| `cargo flamegraph` | Which Rust functions are hot? Allocator pressure, O(n) loops in Rust. |
| `scripts/profile/materialize.llt` | Which source locations are forced most? Which builtins drive the most re-entry? |
| `scripts/profile/trace.llt` | What is the full execution sequence? How do lazy thunks connect creation to materialization? |

### Criterion Benchmarks

A `benches/eval.rs` suite provides before/after comparison for optimizations:

```rust
use criterion::{criterion_group, criterion_main, Criterion};

fn bench_map_10k(c: &mut Criterion) {
    c.bench_function("map_10k", |b| {
        b.iter(|| eval_str("[take 5 [map [fn [let x] [+ x 1]] [range 0]]]"))
    });
}

fn bench_dict_1k(c: &mut Criterion) {
    c.bench_function("dict_1k", |b| {
        b.iter(|| eval_str(DICT_1K_PROGRAM))
    });
}

fn bench_deep_scope(c: &mut Criterion) {
    // Measures Environment::get traversal on deeply nested scopes.
    // Provides the chain-depth data for string-interning and union-find decisions.
}
```

### Corpus Test Integration

```sh
# Run program with profiling, then analyze
just profile       tests/corpus/foo.llt   # tinct run --profile spans.json + materialize.llt
just profile-trace tests/corpus/foo.llt   # tinct run --profile spans.json + trace.llt
```

### Overhead Model

Profiling is off by default — a single thread-local flag check per thunk force, eliminated by branch prediction. No compile-time feature flag is needed.

When enabled:

| Mode | Per-span overhead | Note |
|---|---|---|
| `--profile` | ~60-100ns (two `Instant::now()` + String alloc + `Vec::push`) | Serialization runs after eval |

Serialization of the span `Seq` by the analysis script runs after the program completes, not during evaluation.

## What Would Change

### `src/eval.rs`

**Current:** thunk forces have no instrumentation.
**Proposed:** `ProfilingCollector` (a `Vec<SpanRecord>` + current-span stack) in `EvalContext`; `SpanRecord` struct (14 plain fields, no serde, including `create_time_us`); span open/close bracketing each `materialize()` call using the thunk's existing `Span`; `create_parent` and `create_time_us` recorded at thunk construction by reading the collector's current-span stack. `ProfilingCollector::into_value()` converts the completed `Vec` to a `Value::Seq` of plain dicts, serialized to the output file via `codecs/json.llt`.
**Impact:** Moderate — affects the hot eval path, gated behind a flag check.

### `src/builtins.rs` and related

**Current:** `Thunk.origin` is already set at cross-boundary sites including `builtin_map` (`"map"`, `"map head"`, `"call $map"`), `builtin_filter` (`"call $filter"`, `"filter-dict pred"`), `builtin_reduce` (`"reduce"`), `builtin_sort` (`"sort"`), `str-map-chars`, `apply`, and others.
**Proposed:** Rename existing origin strings to match the `builtin-*` naming convention (e.g., `"map"` → `"builtin-map"`, `"call $filter"` → `"builtin-filter"`). This is part of the `builtin-privacy-primary-names` sprint. Add origin to `builtin_select_once` (the Rust builtin underlying tinct's `loop-select`) if not already set. `builtin_include` was deleted (see `include-decomposition.md`); `include` is now tinct in prelude and profiles naturally.
**Impact:** Minor — renaming existing string literals.

### `src/main.rs`

**Current:** No profiling CLI flags.
**Proposed:** `--profile <file.json>` — initializes `ProfilingCollector` before eval, serializes the span `Seq` to the named file after eval using `codecs/json.llt`. No JSON writing in Rust beyond invoking the tinct serializer. No `--trace` flag — trace output is generated by running `scripts/profile/trace.llt` on the span file.
**Impact:** Minor — ~30 lines of flag parsing and file write.

### `scripts/profile/` (new)

Three analysis scripts:

- `materialize.llt` — materialization-context hotspot table (emits to stdout)
- `create.llt` — creation-context hotspot table (emits to stdout)
- `trace.llt` — Perfetto Chrome Trace JSON (returns dict; use `-o json`)

All JSON output goes through `codecs/json.llt`. No serde_json anywhere in the profiling stack.

### `Cargo.toml`

**Current:** No dev-dependencies for benchmarking.
**Proposed:** `criterion = "0.5"` under `[dev-dependencies]`; `[[bench]] name = "eval"` entry. No new runtime dependencies.
**Impact:** Minor — build-time only.

### `justfile`

**Current:** `just bench` runs `cargo bench` against no benchmark files.
**Proposed:** `just bench` runs the criterion suite; `just profile <file>` runs one file with `--profile` and pipes to `materialize.llt`; `just profile-trace <file>` pipes to `trace.llt`.
**Impact:** Minor.

## Prerequisites

- **`stdlib-conformance-bugs`** — fixes the `>=i` tokenization bug in `codecs/json.llt:227`. Required before `scripts/profile/trace.llt` can use `-o json` reliably, since `-o json` depends on `codecs/json.llt`.
- **`json-native-from-json`** or equivalent — `codecs/json.llt` must be the canonical `from-json`/`to-json` implementation, exercised and tested, before the `-i json` input pipeline that feeds the analysis scripts depends on it.

The Criterion benchmarks and `scripts/profile/materialize.llt` have no JSON dependency and can be implemented before the above. `scripts/profile/trace.llt` (which uses `-o json`) requires `codecs/json.llt` to be stable.

The Criterion benchmarks produce the most stable data after the `runtime-v2` migration stabilizes — benchmarks should target the post-migration evaluator architecture. The `--profile` flag works on the current evaluator and does not block on `runtime-v2`.

## References

- Google (2012). "Trace Event Format." Chrome DevTools documentation. — The JSON format consumed by `chrome://tracing` and Perfetto UI; the output format produced by `scripts/profile/trace.llt`.
- The Perfetto Project (2019–). *Perfetto Trace Viewer*. <https://ui.perfetto.dev/> — Modern trace visualization; loads Chrome trace JSON and supports flow events for async causal chains.
- Criterion contributors (2014–). *criterion.rs*. — Rust microbenchmark harness with paired t-test significance testing; the standard Rust tool for before/after comparison.
- Launchbury, J. (1993). "A natural semantics for lazy evaluation." In *POPL '93*, pp. 144–154. ACM. — The thunk-forcing semantics that defines what a "span" corresponds to in a lazy evaluator; justifies forcing-context attribution.
- Wadler, P. (1987). "The concatenate vanishes." Technical report, University of Glasgow. — Shows that `++` can be replaced without loss of expressiveness; establishes that always-demanded arguments in lazy evaluation are semantically equivalent to strict evaluation, informing the choice of forcing-context attribution.
- Jones, S.L.P., Ramsey, N., and Reif, F. (2002). "A principled approach to operating system construction in Haskell." — GHC's cost-centre profiling system (creation-context attribution) and the rationale for choosing it over forcing-context.
