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

```
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
| `builtin_include` | full eval of included file | one deep re-entry |

### The Lazy Evaluation Attribution Problem

In a strict language, "who called this function?" is unambiguous. In a lazy language, a thunk is created in one context and forced in another. A program like:

```tinct
result: [map expensive-fn items]
[emit [str-join ", " result]]
```

creates the map thunk at the `result` binding, but `expensive-fn` is not called until `emit` forces it. A profiler that attributes cost to creation sites would report `result: [map ...]` as expensive — but `emit` is the one demanding the work.

The correct attribution model for "why is this slow?" is **forcing-context attribution**: time is charged to the span that forced a thunk, not the span that created it. This is the model tinct's profiler uses.

## Design

Two modes: **profiling** (`--profile`) and **tracing** (`--trace`). Both are built on the same span model. Either can be enabled independently.

### The Span Model

Every thunk force is a **span**: a timed interval identified by source location, with optional children. Spans nest naturally — a builtin that forces its arguments produces child spans for each argument's evaluation.

```
┌─ [builtin: map]  versions.llt:8  [1200ms total, 2ms self] ─────────────┐
│ ┌─ versions.llt:12:3-12:45  [fn [let crate] ...]  [10ms] ─┐  ...       │
│ └──────────────────────────────────────────────────────────┘            │
└─────────────────────────────────────────────────────────────────────────┘
```

Each span carries:

- **source_span** — the tinct `Span` (file, start byte offset, end byte offset) of the expression being forced; already present on every thunk
- **builtin_name** — for Rust builtins (which have no tinct source span), a static string identifying the builtin (e.g., `"map"`, `"filter"`, `"connect"`)
- **force_parent** — the span whose force triggered this span (execution parent)
- **origin_builtin** — when this tinct span was scheduled by a Rust builtin, the originating builtin's name (e.g., `"map"`, `"filter"`)
- **start_ts / end_ts** — wall-clock timestamps in microseconds

Tinct spans aggregate by `(file, start, end)` — every invocation of the expression at `versions.llt:12:3-12:45` contributes to the same row in the profile table, regardless of how many times it is called. Rust builtin spans aggregate by `builtin_name`. No naming layer is needed: source location is the unique, stable identity for every tinct expression.

### Cross-Boundary Attribution

When a Rust builtin creates a `PendingCall` to invoke a tinct function, the `PendingCall` is tagged with the originating builtin name:

```rust
// In builtin_map when creating per-element callbacks
PendingCall {
    func: callback_thunk,
    args: vec![element_thunk],
    named_args: IndexMap::new(),
    origin_builtin: Some("map"),
}
```

The child span inherits `origin_builtin`. In the profile table, this appears as an annotation on each tinct source location showing which Rust builtin drove its evaluation. The profiler also aggregates from the Rust side: the `[builtin: map]` row shows total child time attributable to its tinct re-entries.

### Profiling Mode (`--profile`)

```sh
tinct --profile program.llt
tinct --profile --profile-format=json program.llt
```

Aggregates spans by source location for tinct expressions, by builtin name for Rust builtins. Reports a sorted table to stderr after program exit:

```
PROFILE: samples/versions.llt  [total: 1234ms]
────────────────────────────────────────────────────────────────────────────────────────
  calls    self_ms   total_ms   location                    source
────────────────────────────────────────────────────────────────────────────────────────
      1      800.0    1234.0    <toplevel>
      3      400.0     610.0    [builtin: map]               [origin: versions.llt:8:9]
   8432      190.0     190.0    versions.llt:12:3-12:45     [fn [let crate] [str "https://...
      1       60.0     200.0    [builtin: reduce]
     12       30.0      30.0    versions.llt:31:5-31:62     [fn [let v] [split "." v]]
      1       20.0      20.0    [builtin: connect]
────────────────────────────────────────────────────────────────────────────────────────
```

Columns:
- **calls** — number of times this span was entered
- **self_ms** — time in this span excluding children
- **total_ms** — total including all child spans
- **location** — `file:line:col` for tinct expressions; `[builtin: name]` for Rust builtins
- **source** — leading characters of the source text at that span; `[origin: ...]` for Rust builtins shows the tinct call site that invoked them

The `--profile-format=json` flag emits the raw aggregated data as JSON for programmatic analysis.

### Tracing Mode (`--trace`)

```sh
tinct --trace out.json program.llt
```

Emits a [Perfetto](https://ui.perfetto.dev/)-compatible JSON trace file (Chrome Trace Event Format). Load `out.json` in the Perfetto UI for an interactive flame graph with full span nesting and timing.

```json
{"traceEvents": [
  {"ph": "B", "ts": 0,   "name": "[builtin: map]",      "pid": 1, "tid": 1,
   "args": {"origin": "versions.llt:8:9"}},
  {"ph": "B", "ts": 100, "name": "versions.llt:12:3",   "pid": 1, "tid": 1,
   "args": {"source": "[fn [let crate] [str \"https://...", "origin_builtin": "map", "call": 0}},
  {"ph": "E", "ts": 150, "name": "versions.llt:12:3",   "pid": 1, "tid": 1},
  {"ph": "B", "ts": 150, "name": "versions.llt:12:3",   "pid": 1, "tid": 1,
   "args": {"source": "[fn [let crate] [str \"https://...", "origin_builtin": "map", "call": 1}},
  {"ph": "E", "ts": 200, "name": "versions.llt:12:3",   "pid": 1, "tid": 1},
  {"ph": "E", "ts": 200, "name": "[builtin: map]",      "pid": 1, "tid": 1}
]}
```

The `name` field in each event is the source location (`file:line:col`) for tinct expressions, so the Perfetto UI labels every row with a precise code pointer rather than a derived name. The `args.source` field carries the actual source text snippet for display in the detail pane.

Perfetto "flow events" (`ph: "s"/"f"`) connect thunk creation sites to forcing sites, making the lazy evaluation decoupling visible: an arrow links the `result: [map ...]` creation span to the `emit` forcing span.

### Rust-Level Profiling

For Rust-specific hotspots, the standard `cargo flamegraph` workflow is used without code changes:

```sh
cargo install flamegraph
cargo flamegraph --bin tinct -- samples/versions.llt
```

This produces a flamegraph SVG showing Rust call stacks. It answers "which Rust functions consume CPU?" but loses tinct-level attribution. Combined with `--profile`, the two views answer complementary questions:

| Tool | Answers |
|---|---|
| `cargo flamegraph` | Which Rust functions are hot? Allocator pressure, O(n) loops in Rust. |
| `--profile` | Which tinct functions are called most? Which builtins drive the most re-entry? |
| `--trace` | What is the full call sequence? How do lazy thunks connect creation to forcing? |

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

The `bench_deep_scope` benchmark specifically provides the measurement that unblocks `string-interning.md` and `union-find-substitution.md`.

### Corpus Test Integration

```sh
just profile-test tests/corpus/foo.llt
just trace-test   tests/corpus/foo.llt
```

`just profile-test` runs a single corpus file with `--profile` and prints the table. `just trace-test` emits `.tmp/trace-foo.json`. Both run the full corpus evaluation including type-checking, so profiling includes type inference cost.

### Overhead Model

Profiling and tracing are off by default. When disabled, the hot eval path checks a single thread-local boolean — negligible overhead that branch prediction eliminates. No compile-time feature flag is needed.

When enabled:

| Mode | Per-span overhead | Acceptable for |
|---|---|---|
| `--profile` | ~50ns (two `Instant::now()` + counter bump) | All programs |
| `--trace` | ~200ns (two `Instant::now()` + Vec push) | Programs with < 5M spans |

A program with 100,000 thunk forces incurs ~5ms profiling overhead and ~20ms tracing overhead on top of its natural runtime.

## What Would Change

### `src/eval.rs`

**Current:** thunk forces have no instrumentation.
**Proposed:** `ProfilingContext` in `EvalContext` (initialized from CLI flags); span open/close bracketing each `materialize()` call; span identity taken directly from the thunk's existing `Span` field — no new naming infrastructure needed.
**Impact:** Moderate — affects the hot eval path, but gated behind a runtime flag check.

### `src/builtins.rs` and related

**Current:** `PendingCall`/`PendingBuiltin` have no origin annotation.
**Proposed:** `origin_builtin: Option<&'static str>` on both types; set by `builtin_map`, `builtin_filter`, `builtin_reduce`, `builtin_loop_select`, `builtin_include`, and any Rust builtin that schedules tinct callbacks.
**Impact:** Minor — one pointer-sized field per deferred call.

### `src/main.rs`

**Current:** No profiling CLI flags.
**Proposed:** `--profile`, `--profile-format=<table|json>`, `--trace <file.json>`; each initializes `ProfilingContext` before eval and finalizes (table print or file write) after.
**Impact:** Minor — ~50 lines of flag parsing and output.

### `Cargo.toml`

**Current:** No dev-dependencies for benchmarking.
**Proposed:** `criterion = "0.5"` under `[dev-dependencies]`; `[[bench]] name = "eval"` entry.
**Impact:** Minor — build-time only.

### `justfile`

**Current:** `just bench` runs `cargo bench` against no benchmark files.
**Proposed:** `just bench` runs the criterion suite; `just profile-test <file>` runs one file with `--profile`; `just trace-test <file>` emits the Perfetto trace.
**Impact:** Minor.

## Prerequisites

No blocking prerequisites. This proposal is entirely additive. The Criterion benchmarks produce the most stable data after the `runtime-v2` migration (`Rc`→`Arc`, `OnceLock`) stabilizes — benchmarks should target the post-migration evaluator architecture rather than the transitional state. Implementation can begin before `runtime-v2` is complete; the `--profile` and `--trace` flags work on the current evaluator.

## References

- Google (2012). "Trace Event Format." Chrome DevTools documentation. — The JSON format consumed by `chrome://tracing` and Perfetto UI; the output format for `--trace` mode.
- The Perfetto Project (2019–). *Perfetto Trace Viewer*. <https://ui.perfetto.dev/> — Modern trace visualization; loads Chrome trace JSON and supports flow events for async causal chains.
- Criterion contributors (2014–). *criterion.rs*. — Rust microbenchmark harness with paired t-test significance testing; the standard Rust tool for before/after comparison.
- Launchbury, J. (1993). "A natural semantics for lazy evaluation." In *POPL '93*, pp. 144–154. ACM. — The thunk-forcing semantics that defines what a "span" corresponds to in a lazy evaluator; justifies forcing-context attribution.
- Wadler, P. & Hughes, J. (1987). "The concatenate vanishes." Technical report, Glasgow. — Semantic equivalence of lazy and strict evaluation for always-demanded arguments; justifies attributing cost to the forcing context rather than the creation context.
