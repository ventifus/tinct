# What If: Literate Mode v2 — Self-Hosted Block Formatters

**State:** Proposal

What would it take to migrate `tinct literate` from a Rust-heavy implementation into a self-hosted system where block formatters live in tinct, dynamically looked up the same way `-o json` works for `tinct run`?

## Current State

`tinct literate` has three modes — `tangle`, `eval`, `weave` — implemented entirely in Rust (`src/main.rs`, `src/literate.rs`). The weave mode evaluates each tinct code block, serializes the result to JSON via hardcoded Rust, and embeds it as a `=== out` section. Per-block output format is fixed: every block outputs JSON, always.

The fence opening ` ```tinct ` is matched exactly by `src/literate.rs:82`:

```rust
if trimmed == "```tinct" || trimmed == "```llt" {
```

Any text after the language identifier on the fence line is silently ignored. There is no way to say "show this block as raw text" or "format this block as CSV."

### What's Missing

1. **Per-block output format.** All blocks serialize the same way. A block returning a Float should show as a raw number; a block returning a Seq of records might want CSV; a dict might want pretty-printed JSON.
2. **User-defined formatters.** There is no way to add custom serializers. Rust must be changed to support a new output format in literate mode.
3. **Duplication with `tinct run` formatters.** The serialization logic in `run_literate_weave` is a parallel implementation of what `stdlib/cli/out/json.llt` does. The two paths diverge over time.
4. **No symmetry with `-o`.** `tinct run -o json` uses a dynamically loaded tinct formatter. `tinct literate weave` uses hardcoded Rust. Users learn two different mental models.

## Why Literate v2 Matters for tinct

**`tinct literate json` feels identical to `tinct run -o json`.** One model: a formatter name resolves to a file in `stdlib/literate/out/`, the engine threads `%` through it, the result is embedded. Users who know how `-o` works already know how per-block formatting works.

**Self-hosted and user-extensible.** Drop a file into `stdlib/literate/out/myformat.llt` and ```` ```tinct myformat ```` works. No Rust changes, no recompile, no CLI flags to add. Custom formatters for domain types (e.g. a pretty-printer for a domain-specific record) are first-class.

**Fence metadata declares intent, not logic.** The second word on the fence line is a format name — a declaration, not code. Complex transformations belong inside the block or in preceding pipeline blocks threaded with `--- %name`. This separation keeps fence lines readable and keeps business logic where readers expect it.

**Formatters can be identical to `stdlib/cli/out/`.** The contract is the same: receive `%`, write formatted output. A literate formatter and a CLI output formatter can be the same file, or literate can symlink to cli/out. Zero duplication when the formats align.

## Design

### Fence Syntax — Format Name as Second Word

The fence opening accepts an optional format name as the second whitespace-separated word:

```text
```tinct              ← no format name: use default (json)
```tinct json         ← use stdlib/literate/out/json.llt
```tinct raw          ← use stdlib/literate/out/raw.llt
```tinct json-pretty  ← use stdlib/literate/out/json-pretty.llt
```tinct csv          ← use stdlib/literate/out/csv.llt
```tinct myformat     ← use stdlib/literate/out/myformat.llt
```

The format name is a bare identifier (same rules as stdlib file names: `[a-z][a-z0-9-]*`). Everything else on the fence line after the format name is ignored (future extensibility). No pipeline syntax, no tinct expressions — just a name.

This is the direct analogue of `-o json` in `tinct run`. The literate engine resolves the name to a file, loads it, and threads the block's result through it as `%`.

### Formatter Contract — Reuse `stdlib/cli/out/` Directly

Literate formatters **are** the existing CLI output programs (`stdlib/cli/out/json.llt` etc.) — no new formatter files needed. The same output program contract applies: drain `%emit`, force `%`, write to `%stdout`. The only difference is that in literate mode, `%stdout` is a captured buffer rather than the terminal, and the formatter's **return value is discarded** — the block's original result threads forward as `%` to the next block.

The `%emit` channel is **shared** between a block and its formatter within each section. The block emits values into it; the formatter drains it. This is identical to the `tinct run` model where eval-programs creates one `%emit` for the whole pipeline.

### `literate-eval-programs` — the %-threading coordinator

The key orchestration lives in `stdlib/literate.llt`. Standard `eval-programs` threads `%` sequentially through every program, including formatters — but literate mode must discard the formatter's return value and preserve the block's result as the next `%`:

```tinct
# stdlib/literate.llt
literate-eval-programs: [fn [let sections initial-input]
  # sections: [Seq [block: Program  formatter: Program | []]]
  [builtin-reduce
    [fn [let percent section]
      # Shared %emit for this section — block and formatter share it
      [emit-ch: [builtin-channel 64]]
      # Run the block; its return value is the data % for the next section
      [block-result: [eval-program section.block percent emit-ch]]
      # Run the formatter with block-result as % and the shared emit-ch.
      # Formatter drains %emit from block execution, writes to captured %stdout.
      # Formatter's return value is discarded — block-result threads forward.
      [if [= [] section.formatter]
        block-result
        [[eval-program section.formatter block-result emit-ch]
         block-result]]]
    initial-input
    sections]]
```

The `eval-program` calls are standard. The literate engine provides a per-section `%stdout` capture handle so the formatter's `[write %stdout ...]` calls go to a buffer instead of the terminal. After the formatter completes, the buffer contents become the `=== out` section; `block-result` (not the formatter's `[]` return) becomes `%` for the next section.

**The CLI formatters need zero changes.** `stdlib/cli/out/json.llt` running against a literate section:

1. Spawns drain task on `%emit` — drains values emitted during block execution ✓
2. Forces `%` (= block-result) — serializes the block's return value ✓
3. Writes to `%stdout` — captured by the literate engine per-section ✓
4. Returns `[await drain]` result — discarded; block-result threads forward ✓

### Dynamic Lookup

The engine resolves ```` ```tinct json ```` by looking up `stdlib/cli/out/json.llt` directly. There is one formatter registry: `stdlib/cli/out/`. Both `tinct run -o json` and ```` ```tinct json ```` resolve to the same file.

If a format name is not found in `stdlib/cli/out/`, the error is `unknown format: "json"` — same error as an unknown `-o` flag. There is no separate `stdlib/literate/out/` directory; having two places for formatters creates confusion about which takes precedence and where to add new ones.

Formatters that only make sense in a literate context (e.g. a Markdown table renderer) are added to `stdlib/cli/out/md-table.llt` under a distinct name. They're available to both `tinct run -o md-table` and ```` ```tinct md-table ```` automatically.

### Default Format

A block with no format name uses the `json` formatter by default — the same output as today. Existing documents that don't use the second-word syntax are fully backward compatible.

### The Corpus Formatter — Runtime Diagnostics Channels

`=== out` is only one of four sections the literate weaver produces. The full corpus structure is:

- `=== out` — what the output formatter writes to `%stdout`
- `=== warn` — type-level warnings (T010, T011, T013, etc.)
- `=== error` — evaluation errors or type errors
- `=== info` — diagnostic/log output

Currently Rust assembles these four channels separately: it catches Rust `Result` errors, collects `Vec<TypeDiagnostic>` from the type checker, and captures stdout. For a fully tinct-native model, the runtime must expose these as **tinct-accessible channels**:

**`%logger`** — a `Channel@LogLine` injected by the literate engine into each block's eval context. User calls to `[info "msg" ...]`, `[warn "msg" ...]`, etc. send structured `LogLine` dicts into `%logger`. A tinct log router program (from `stdlib/log.llt`) drains `%logger`, formats each entry, and calls `[emit fmt]` — so log output appears in `=== out` alongside the block's final result. User logging never populates `=== warn`, `=== info`, or `=== error`; those sections are exclusively for runtime/type-checker messages. This is the async-channel resolution of the open question in `doc/whatif/structured-logging.md`.

**`%diagnostics`** — a `Channel@Diagnostic` injected by the literate engine. The type checker and evaluator send structured diagnostic values into this channel as evaluation proceeds. The corpus formatter drains it to produce `=== warn` (type warnings), `=== error` (evaluation errors), and `=== info` (runtime informational notes from the type checker's Info tier). User code has no access to this channel.

```tinct
Diagnostic: [type Diagnostics
  [Warning  code: String  message: String  span: String]
  [Error    code: String  message: String  span: String]
  [Info     message: String]]
```

The **corpus formatter** is a tinct program in `stdlib/literate/corpus.llt` that wraps block execution and assembles all four sections:

```tinct
# stdlib/literate/corpus.llt
# Runs a block section and collects all corpus output channels.
# Returns: [CorpusSection out: String warn: String error: String info: String]
[
  run-section: [fn [let section percent]
    [emit-ch:   [builtin-channel 64]]   # block's emitted values + log output
    [logger-ch: [builtin-channel 64]]   # user log entries (LogLine dicts)
    [diag-ch:   [builtin-channel 64]]   # runtime diagnostics (type warnings, errors)
    [stdout-buf: [string-handle]]        # captures formatter's %stdout

    # Run the block with all three injected channels
    [result: [try [fn []
      [eval-program section.block percent emit-ch logger-ch diag-ch]]]]

    # Log router drains logger-ch, formats each LogLine, sends to emit-ch
    # (so log output joins emitted values in === out)
    [run-log-router logger-ch emit-ch]

    # If block succeeded and has a formatter, run it to produce === out content
    [if [= [] section.formatter]
      []
      [match result
        [Ok block-result]:
          [eval-program section.formatter block-result emit-ch logger-ch diag-ch stdout-buf]
        [Error _]: []]]

    # Drain runtime diagnostics
    [warnings: [collect-typed diag-ch [Warning]]]
    [errors:   [collect-typed diag-ch [Error]]]
    [infos:    [collect-typed diag-ch [Info]]]

    [CorpusSection
      out:   [read-all stdout-buf]
      warn:  [join "\n" [map format-diagnostic warnings]]
      error: [match result [Ok _]: [join "\n" [map format-diagnostic errors]] [Error e]: [format-error e]]
      info:  [join "\n" [map format-diagnostic infos]]]]]
]
```

`eval-program` gains `logger-ch` and `diag-ch` parameters alongside `emit-ch`. The type checker sends `Warning`/`Info` diagnostics into `diag-ch`; the evaluator sends `Error` diagnostics into `diag-ch` or propagates them as tinct exceptions; user logging goes through `%logger` → `logger-ch` → log router → `emit-ch` → `=== out`.

The three channels form a clear ownership boundary:

- `%emit` / `emit-ch` — user program output (emit calls)
- `%logger` / `logger-ch` — user log entries (structured, level-tagged)
- `%diagnostics` / `diag-ch` — runtime messages (type checker, evaluator) — user code has no write access

### `=== out` Content

The `=== out` section contains exactly what the output formatter writes to `%stdout` (the captured `string-handle`). The corpus formatter collects this and the other channels into a `CorpusSection` which the literate engine embeds in the markdown:

```markdown
```tinct raw
[length [1 2 3 4 5]]
```

=== out
5

```markdown

```markdown
```tinct json
[name: "Alice"  age: 30]
```

=== out
{"name": "Alice", "age": 30}

```markdown

### Transformations Live in the Block

If a block's result needs transformation before formatting, the transformation happens inside the block or in a preceding pipeline block. The format name is not the place for logic:

```markdown
<!-- Transform in the block: -->
```tinct csv
[%.users | each | [filter _.active] | collect]
```

<!-- Or split into pipeline stages: -->
```tinct
[users | each | [filter _.active]]
```

```tinct csv
%
```

```markdown

This keeps fence metadata as pure declarations and keeps transformations visible in the code blocks where readers expect them.

## What Would Change

### `src/literate.rs` — fence parsing

**Current:** Exact match `trimmed == "```tinct"`. Info string silently ignored.

**Proposed:** Parse second word as optional format name:

```rust
pub struct CodeBlock {
    pub content: String,
    pub format: Option<String>,  // None = default (json), Some("csv") = csv formatter
}
```

`LiterateBlock` gains `format: Option<String>`. `extract_code_blocks` and `extract_blocks` updated to parse the second word. The second word must match `[a-z][a-z0-9-]*`; anything else is treated as no format (for future extensibility).

**Impact:** Minor. Backward compatible.

### `src/main.rs` — run_literate_weave

**Current:** ~200 lines of hardcoded JSON serialization.

**Proposed:** For each block, look up the formatter file (step 1: `stdlib/literate/out/{name}.llt`, step 2: `stdlib/cli/out/{name}.llt`), load it, and pass it to `eval-programs` alongside the block program. Capture `%stdout` output as the `=== out` content. Shrinks to ~40 lines.

**Impact:** Moderate. Output behavior preserved for the default `json` format.

### Corpus test format

**Current:** `=== out` always contains JSON.

**Proposed:** `=== out` contains whatever the formatter produces. Default (no format name) is still JSON — no existing tests break.

**Impact:** Minor, backward compatible.

## Prerequisites

**`stdlib/cli/out/` formatters complete (S-796 `data-streaming-formatters`).** The formatter lookup requires `stdlib/cli/out/{name}.llt` to exist for the standard formats. The data-streaming sprint creates these.

**`eval-programs` wired to CLI (S-800 `builtin-privacy-phase4`).** The engine uses `eval-programs` to run blocks and formatters. Requires Phase 4 CLI wiring.

**`StringHandle` — writable Handle backed by a String buffer.** The literate engine captures `%stdout` per-section into a buffer rather than the terminal. Requires a new `Value::WriteHandle` variant backed by `Arc<Mutex<String>>` rather than a file descriptor.

**`%diagnostics` channel — runtime diagnostics exposed to tinct.** The type checker and evaluator must send structured `Diagnostic` values (warnings, errors, spans) into a `Channel@Diagnostic` injected by the literate engine per section. The type checker currently returns `Vec<TypeDiagnostic>` as a Rust value; it must be converted to channel sends accessible from tinct. This requires threading `diag-ch` through `eval-program`, `builtin-eval`, and the type checker entry points. Significant work — the largest prerequisite in this whatif.

**`structured-logging` whatif resolved — `%logger` as async channel.** The open question in `doc/whatif/structured-logging.md` ("use async channels for this") must be answered before literate-v2 can be implemented. The design assumed here: `%logger: Channel@LogLine` is the ambient logging channel, injected alongside `%emit`; a log router program drains `%logger`, formats entries, and calls `[emit fmt]` so log output appears in `=== out`. Literate-v2 requires this channel model to be settled.

**`remove-emitted-flag` sprint.** The `emitted` flag that suppresses final JSON serialization when `emit` is called must be removed. After removal, log output and the block's final result both appear in `=== out`, as this design requires.

**`stdlib/literate/corpus.llt` (new).** The corpus formatter that runs a section and collects all four output channels (`=== out/warn/error/info`) into a `CorpusSection`. Depends on all of the above.

## References

- Knuth, D.E. (1984). "Literate Programming." *Computer Journal* 27(2), pp. 97–111. — the original literate programming model.
- CommonMark spec §4.5 (Fenced code blocks) — info string: everything after the opening fence backticks and language identifier. The second whitespace-delimited word is the format name in this design.
