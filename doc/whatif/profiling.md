# What If: Program Profiling and Call Tracing for tinct

**State:** Proposal

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
| `builtin_loop_select` | one PendingCall per iteration | unbounded |

`include` is not in this table — per `include-decomposition.md`, it is defined in `prelude.llt` using the eight Rust primitives (`load`, `expand`, `eval`, `blake3`, etc.). It appears in profiling output at its source location in prelude, not as a Rust builtin.

### The Lazy Evaluation Attribution Problem

In a strict language, "who called this function?" is unambiguous. In a lazy language, a thunk is created in one context and forced in another. A program like:

```tinct
result: [map expensive-fn items]
[emit [str-join ", " result]]
```

creates the map thunk at the `result` binding, but `expensive-fn` is not called until `emit` forces it. A profiler that attributes cost to creation sites would report `result: [map ...]` as expensive — but `emit` is the one demanding the work.

The correct attribution model for "why is this slow?" is **forcing-context attribution**: time is charged to the span that forced a thunk, not the span that created it. This is the model tinct's profiler uses.

## Design

### Rust/Tinct Boundary

The profiling system divides cleanly into two responsibilities:

**Rust** — collect raw timing data. The evaluator runs in Rust and is the only place where `Instant::now()` can bracket a thunk force. Rust accumulates a flat `Vec<SpanRecord>` during evaluation and, when evaluation completes, converts it to a tinct `Seq` of plain dicts — no JSON, no formatting.

**Tinct** — format all output. Three stdlib formatters in `stdlib/profiling/` each take the span `Seq` as input and produce a `String`. The CLI runs the chosen formatter exactly as it runs any output formatter. No JSON writing happens in Rust.

This boundary means no `serde_json` dependency in the profiling implementation. The Perfetto trace file is JSON written by `codecs/json.llt` in tinct, not by Rust.

### Span Collection (Rust)

Every thunk force is a **span**: a timed interval identified by source location. The Rust evaluator records one `SpanRecord` per forced thunk:

```rust
struct SpanRecord {
    id:             u64,
    parent_id:      Option<u64>,      // force parent span ID
    source_file:    Arc<str>,         // from the thunk's Span; empty for Rust builtins
    source_start:   u32,              // byte offset
    source_end:     u32,              // byte offset
    source_text:    String,           // leading characters of source at this span
    builtin_name:   Option<&'static str>,   // set for Rust builtins; None for tinct exprs
    origin_builtin: Option<&'static str>,   // set when scheduled by a Rust builtin
    start_us:       u64,              // microseconds since program start
    end_us:         u64,              // microseconds since program start
}
```

`source_file`/`source_start`/`source_end` are taken directly from the thunk's existing `Span` — no new span infrastructure. `source_text` is a fixed-length snippet extracted from the source text already held by the include cache.

When evaluation completes, `ProfilingContext::into_value()` converts the `Vec<SpanRecord>` into a `Value::Seq` of plain dicts. Each dict uses kebab-case keys matching tinct conventions:

```tinct
# One dict per span — the schema stdlib/profiling/ formatters operate on:
{
  id:             Int           # unique span ID
  parent:         Int | []      # parent span ID, or [] if toplevel
  source-file:    Str           # e.g. "versions.llt"; "" for Rust builtins
  source-start:   Int           # byte offset into source-file
  source-end:     Int           # byte offset into source-file
  source-text:    Str           # source snippet for display
  builtin:        Str | []      # builtin name if this is a Rust builtin
  origin-builtin: Str | []      # originating Rust builtin if cross-boundary
  start-us:       Int           # wall-clock microseconds since program start
  end-us:         Int           # wall-clock microseconds since program start
}
```

Tinct spans are identified by `(source-file, source-start, source-end)`. Rust builtin spans are identified by `builtin`. No naming layer is needed.

### Cross-Boundary Attribution

When a Rust builtin creates a `PendingCall` to invoke a tinct function, the call is tagged with the originating builtin name:

```rust
PendingCall {
    func: callback_thunk,
    args: vec![element_thunk],
    named_args: IndexMap::new(),
    origin_builtin: Some("map"),
}
```

The child span inherits `origin_builtin` as its `origin-builtin` field. This makes the Rust→tinct re-entry cost visible in every formatter without any formatter needing special knowledge of the evaluator.

### Tinct Stdlib Formatters

Three formatters in `stdlib/profiling/`:

**`stdlib/profiling/table.llt`** — aggregates the span Seq by source location, computes `self-ms`/`total-ms`/`calls`, sorts by total time descending, produces a formatted table as a `Str`:

```tinct
# stdlib/profiling/table.llt
# Input: %  — Seq of span dicts
# Output: Str — formatted profile table

[
  spans:      %
  grouped:    [group-by span-key spans]
  aggregated: [map aggregate-group grouped]
  sorted:     [sort-by [fn [let r] [- 0 r.total-ms]] aggregated]

  header: "────────────────────────────────────────────────────────────\n  calls    self_ms   total_ms   location\n────────────────────────────────────────────────────────────\n"
  rows:   [map format-row sorted]
  footer: "────────────────────────────────────────────────────────────"

  [str header [str-join "\n" rows] "\n" footer]
]
```

**`stdlib/profiling/perfetto.llt`** — converts the span Seq to Perfetto Chrome Trace Event Format and serializes with `codecs/json.llt`:

```tinct
# stdlib/profiling/perfetto.llt
# Input: %  — Seq of span dicts
# Output: Str — Perfetto-compatible JSON (Chrome Trace Event Format)

[
  net: [include %libdir "codecs/json.llt"]

  span-name: [fn [let s]
    [if [= [] s.builtin]
      [str s.source-file ":" s.source-start "-" s.source-end]
      [str "[builtin: " s.builtin "]"]]]

  span-to-event: [fn [let s]
    {ph:   "X"
     ts:   s.start-us
     dur:  [- s.end-us s.start-us]
     name: [span-name s]
     pid:  1
     tid:  1
     args: {source:  s.source-text
            parent:  s.parent
            origin:  s.origin-builtin}}]

  [net.to-json {traceEvents: [map span-to-event %]}]
]
```

**`stdlib/profiling/spans.llt`** — emits the raw span Seq as JSON for programmatic analysis, also using `codecs/json.llt`.

### CLI Integration

After evaluation, the CLI retrieves the span `Seq` from `ProfilingContext` and runs the chosen formatter, writing its `String` output to the appropriate destination:

```sh
tinct --profile program.llt              # → table.llt → stderr
tinct --profile-format=json program.llt  # → spans.llt → stderr
tinct --trace out.json program.llt       # → perfetto.llt → out.json
```

The CLI runs each formatter the same way it runs any output formatter — no JSON logic in Rust. The formatter receives the span `Seq` as `%`.

### Sample Output

`--profile` (via `table.llt`):

```text
────────────────────────────────────────────────────────────────────────────────
  calls    self_ms   total_ms   location
────────────────────────────────────────────────────────────────────────────────
      1      800.0    1234.0    <toplevel>
      3      400.0     610.0    [builtin: map]
   8432      190.0     190.0    versions.llt:12:3-12:45    [fn [let crate] ...
      1       60.0     200.0    [builtin: reduce]
     12       30.0      30.0    versions.llt:31:5-31:62    [fn [let v] [split ...
      1       20.0      20.0    [builtin: connect]
────────────────────────────────────────────────────────────────────────────────
```

`--trace` (via `perfetto.llt`) — load the resulting JSON in [Perfetto UI](https://ui.perfetto.dev/) for an interactive flame graph.

### Rust-Level Profiling

For Rust-specific hotspots, the standard `cargo flamegraph` workflow is used without code changes:

```sh
cargo install flamegraph
cargo flamegraph --bin tinct -- samples/versions.llt
```

| Tool | Answers |
|---|---|
| `cargo flamegraph` | Which Rust functions are hot? Allocator pressure, O(n) loops in Rust. |
| `--profile` | Which source locations are forced most? Which builtins drive the most re-entry? |
| `--trace` | What is the full execution sequence? How do lazy thunks nest? |

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
just profile-test tests/corpus/foo.llt
just trace-test   tests/corpus/foo.llt
```

### Overhead Model

Profiling and tracing are off by default — a single thread-local flag check per thunk force, eliminated by branch prediction. No compile-time feature flag is needed.

When enabled:

| Mode | Per-span overhead | Acceptable for |
|---|---|---|
| `--profile` | ~50ns (two `Instant::now()` + `Vec::push`) | All programs |
| `--trace` | ~50ns (same — serialization happens after eval, not per-span) | All programs |

Serialization of the span `Seq` by the tinct formatter runs after the program completes, not during evaluation.

## What Would Change

### `src/eval.rs`

**Current:** thunk forces have no instrumentation.
**Proposed:** `ProfilingCollector` (a `Vec<SpanRecord>` behind an `Option`) in `EvalContext`; `SpanRecord` struct (8 plain fields, no serde); span open/close bracketing each `materialize()` call using the thunk's existing `Span`. `ProfilingCollector::into_value()` converts the completed `Vec` to a `Value::Seq` of plain dicts at eval end.
**Impact:** Moderate — affects the hot eval path, gated behind a flag check.

### `src/builtins.rs` and related

**Current:** `Thunk.origin` is already set at some cross-boundary sites (`"sort"`, `"str-map-chars"`, `"apply"`, etc.) but missing from the main higher-order builtins.
**Proposed:** Set `origin: Some(Arc::from("map"))` etc. at the PendingCall creation sites in `builtin_map`, `builtin_filter`, `builtin_reduce`, `builtin_loop_select`. `builtin_include` was deleted (see `include-decomposition.md`); `include` is now tinct in prelude and profiles naturally. No other changes.
**Impact:** Minor — one pointer-sized field per deferred call.

### `src/main.rs`

**Current:** No profiling CLI flags.
**Proposed:** `--profile`, `--profile-format=<table|json>`, `--trace <file.json>`; each initializes `ProfilingCollector` before eval, retrieves the span `Seq` after eval, and runs the appropriate tinct stdlib formatter. No JSON writing in Rust.
**Impact:** Minor — ~40 lines of flag parsing and formatter dispatch.

### `stdlib/profiling/` (new)

Three new tinct stdlib formatters:

- `table.llt` — aggregates spans, formats profile table as `Str`
- `perfetto.llt` — formats as Perfetto Chrome Trace JSON via `codecs/json.llt`
- `spans.llt` — raw span Seq as JSON via `codecs/json.llt`

All JSON output goes through `codecs/json.llt`. No serde_json anywhere in the profiling stack.

### `Cargo.toml`

**Current:** No dev-dependencies for benchmarking.
**Proposed:** `criterion = "0.5"` under `[dev-dependencies]`; `[[bench]] name = "eval"` entry. No new runtime dependencies.
**Impact:** Minor — build-time only.

### `justfile`

**Current:** `just bench` runs `cargo bench` against no benchmark files.
**Proposed:** `just bench` runs the criterion suite; `just profile-test <file>` runs one file with `--profile`; `just trace-test <file>` emits the Perfetto trace.
**Impact:** Minor.

## Prerequisites

- **`stdlib-conformance-bugs`** — fixes the `>=i` tokenization bug in `codecs/json.llt:227`. Required before `perfetto.llt` and `spans.llt` can use `codecs/json.llt` to serialize trace output.
- **`json-native-from-json`** or equivalent — `codecs/json.llt` must be the canonical `from-json`/`to-json` implementation, exercised and tested, before the profiling formatters depend on it.

The Criterion benchmarks and `--profile` table mode (`table.llt`) have no JSON dependency and can be implemented before the above. The `--trace` and `--profile-format=json` modes require `codecs/json.llt` to be stable.

The Criterion benchmarks produce the most stable data after the `runtime-v2` migration stabilizes — benchmarks should target the post-migration evaluator architecture. The `--profile`/`--trace` flags work on the current evaluator and do not block on `runtime-v2`.

## References

- Google (2012). "Trace Event Format." Chrome DevTools documentation. — The JSON format consumed by `chrome://tracing` and Perfetto UI; the output format for `--trace` mode.
- The Perfetto Project (2019–). *Perfetto Trace Viewer*. <https://ui.perfetto.dev/> — Modern trace visualization; loads Chrome trace JSON and supports flow events for async causal chains.
- Criterion contributors (2014–). *criterion.rs*. — Rust microbenchmark harness with paired t-test significance testing; the standard Rust tool for before/after comparison.
- Launchbury, J. (1993). "A natural semantics for lazy evaluation." In *POPL '93*, pp. 144–154. ACM. — The thunk-forcing semantics that defines what a "span" corresponds to in a lazy evaluator; justifies forcing-context attribution.
- Wadler, P. & Hughes, J. (1987). "The concatenate vanishes." Technical report, Glasgow. — Semantic equivalence of lazy and strict evaluation for always-demanded arguments; justifies attributing cost to the forcing context rather than the creation context.
